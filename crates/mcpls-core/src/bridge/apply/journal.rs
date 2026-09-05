//! Executing an ordered, reversible list of file-system steps.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::warn;

use crate::error::{Error, Result};

/// How many times a rename is attempted before the step is called failed.
///
/// On Windows a rename involving a path another process holds open fails
/// with a transient sharing violation or access-denied, and Defender's
/// real-time scanner, the Search Indexer and an open editor all take such a
/// handle for a moment after a file is written. One of those would
/// otherwise lose an otherwise-good multi-file apply to a rollback. Every
/// attempt after the first waits [`RENAME_RETRY_DELAY`].
const RENAME_ATTEMPTS: u32 = 3;

/// Pause between rename attempts. Long enough to outlast a scanner's handle,
/// short enough that three of them are imperceptible.
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(50);

/// `fs::rename` with a short bounded retry, for the transient failures
/// described on [`RENAME_ATTEMPTS`].
///
/// Sleeps the calling thread, which is always the blocking thread
/// `Applier::apply` puts the whole journal on.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    for _ in 1..RENAME_ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(_) => std::thread::sleep(RENAME_RETRY_DELAY),
        }
    }
    fs::rename(from, to)
}

/// One reversible file-system change, in the order it must be performed.
#[derive(Debug)]
pub enum Step {
    /// Replace a file's whole content.
    Write {
        /// Absolute path to write.
        path: PathBuf,
        /// Content to write.
        content: String,
        /// Content before this step, or `None` when the file does not exist
        /// yet, in which case rollback removes it.
        previous: Option<String>,
    },
    /// Move a file or directory. The destination must not exist.
    Move {
        /// Current path.
        from: PathBuf,
        /// Path to move it to.
        to: PathBuf,
    },
    /// Move a path aside so a later failure can put it back, and so a
    /// successful run can remove it without ever having held its contents.
    Trash {
        /// Path being removed.
        path: PathBuf,
        /// Sibling path it is parked at until the run finishes.
        trash: PathBuf,
    },
}

/// Perform every step in order, rolling back on the first failure.
///
/// # Errors
///
/// Returns [`Error::ApplyPartiallyFailed`] when a step fails. Completed
/// steps are reversed in order; the error names any file the reversal could
/// not return to its original state and says where it actually is.
pub fn execute(steps: &[Step]) -> Result<()> {
    for (completed, step) in steps.iter().enumerate() {
        if let Err(reason) = perform(step) {
            return Err(roll_back(&steps[..completed], reason));
        }
    }

    purge_trash(steps);
    Ok(())
}

fn perform(step: &Step) -> std::result::Result<(), String> {
    match step {
        Step::Write {
            path,
            content,
            previous: _,
        } => write_atomically(path, content),
        Step::Move { from, to } => {
            // `symlink_metadata`, not `exists`: a dangling symlink at the
            // destination reports as absent to `exists`, and `fs::rename`
            // would then replace it with no way for rollback to put it
            // back.
            if to.symlink_metadata().is_ok() {
                return Err(format!(
                    "{} already exists, so {} cannot be moved onto it",
                    to.display(),
                    from.display()
                ));
            }
            rename_with_retry(from, to)
                .map_err(|e| format!("moving {} to {}: {e}", from.display(), to.display()))
        }
        Step::Trash { path, trash } => rename_with_retry(path, trash).map_err(|e| {
            format!(
                "moving {} aside to {}: {e}",
                path.display(),
                trash.display()
            )
        }),
    }
}

/// Write `content` to `path` through a temp file in the same directory,
/// renamed over the target, so the file never holds a partial write.
fn write_atomically(path: &Path, content: &str) -> std::result::Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?
        .to_string_lossy()
        .into_owned();
    let temp = parent.join(format!(".{file_name}.mcpls-tmp"));

    fs::write(&temp, content).map_err(|e| format!("writing {}: {e}", temp.display()))?;

    if let Ok(meta) = fs::metadata(path) {
        // Best effort: a target without readable metadata simply keeps the
        // default permissions the temp file was created with.
        let _ = fs::set_permissions(&temp, meta.permissions());
    }

    rename_with_retry(&temp, path).map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("renaming onto {}: {e}", path.display())
    })
}

fn roll_back(completed: &[Step], reason: String) -> Error {
    let mut written = Vec::new();
    let mut restored = Vec::new();
    let mut unrecovered = Vec::new();

    for step in completed.iter().rev() {
        match step {
            Step::Write {
                path,
                content: _,
                previous,
            } => {
                let outcome = previous.as_ref().map_or_else(
                    || {
                        fs::remove_file(path)
                            .map_err(|e| format!("removing {}: {e}", path.display()))
                    },
                    |original| write_atomically(path, original),
                );
                match outcome {
                    Ok(()) => restored.push(path.clone()),
                    // The restore is itself a temp-and-rename, so a failed
                    // one changed nothing: the file still holds the content
                    // this run put there.
                    Err(_) => written.push(path.clone()),
                }
            }
            Step::Move { from, to } => match rename_with_retry(to, from) {
                Ok(()) => restored.push(from.clone()),
                Err(e) => {
                    unrecovered.push(format!("{} is at {} ({e})", from.display(), to.display()));
                }
            },
            Step::Trash { path, trash } => match rename_with_retry(trash, path) {
                Ok(()) => restored.push(path.clone()),
                Err(e) => unrecovered.push(format!(
                    "{} is at {} ({e})",
                    path.display(),
                    trash.display()
                )),
            },
        }
    }

    Error::ApplyPartiallyFailed {
        written,
        restored,
        unrecovered,
        reason,
    }
}

/// Remove every trash entry once the whole run has succeeded. A failure
/// here leaves a stray file but does not make the apply wrong, so it is
/// logged rather than returned.
fn purge_trash(steps: &[Step]) {
    for step in steps {
        if let Step::Trash { path, trash } = step {
            // `symlink_metadata`, not `is_dir`: a symlink pointing at a
            // directory is a file to remove, not a tree to walk.
            let is_dir = trash
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_dir());
            let outcome = if is_dir {
                fs::remove_dir_all(trash)
            } else {
                fs::remove_file(trash)
            };
            if let Err(e) = outcome {
                warn!(
                    "could not remove {} after deleting {}: {e}",
                    trash.display(),
                    path.display()
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{Step, execute};

    #[test]
    fn test_writes_new_content_to_each_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, "old a").expect("seed a");
        fs::write(&b, "old b").expect("seed b");

        execute(&[
            Step::Write {
                path: a.clone(),
                content: "new a".to_string(),
                previous: Some("old a".to_string()),
            },
            Step::Write {
                path: b.clone(),
                content: "new b".to_string(),
                previous: Some("old b".to_string()),
            },
        ])
        .expect("steps execute");

        assert_eq!(fs::read_to_string(&a).expect("read a"), "new a");
        assert_eq!(fs::read_to_string(&b).expect("read b"), "new b");
    }

    #[test]
    fn test_creates_then_edits_the_same_path_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fresh.rs");

        execute(&[
            Step::Write {
                path: path.clone(),
                content: String::new(),
                previous: None,
            },
            Step::Write {
                path: path.clone(),
                content: "mod inner;\n".to_string(),
                previous: Some(String::new()),
            },
        ])
        .expect("steps execute");

        assert_eq!(fs::read_to_string(&path).expect("read"), "mod inner;\n");
    }

    #[cfg(unix)]
    #[test]
    fn test_preserves_mode_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("script.sh");
        fs::write(&path, "#!/bin/sh\n").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

        execute(&[Step::Write {
            path: path.clone(),
            content: "#!/bin/sh\necho hi\n".to_string(),
            previous: Some("#!/bin/sh\n".to_string()),
        }])
        .expect("steps execute");

        let mode = fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "executable bit survives the rename");
    }

    #[test]
    fn test_moves_a_file_to_a_new_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("old.rs");
        let to = dir.path().join("new.rs");
        fs::write(&from, "content").expect("seed");

        execute(&[Step::Move {
            from: from.clone(),
            to: to.clone(),
        }])
        .expect("steps execute");

        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to).expect("read"), "content");
    }

    #[test]
    fn test_move_onto_an_existing_path_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("old.rs");
        let to = dir.path().join("occupied.rs");
        fs::write(&from, "source").expect("seed from");
        fs::write(&to, "victim").expect("seed to");

        assert!(
            execute(&[Step::Move {
                from,
                to: to.clone(),
            }])
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(&to).expect("read"),
            "victim",
            "the destination is never clobbered by a bare move"
        );
    }

    #[test]
    fn test_trashed_file_is_gone_after_a_successful_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doomed = dir.path().join("doomed.rs");
        fs::write(&doomed, "bye").expect("seed");
        let trash = dir.path().join(".doomed.rs.mcpls-trash0");

        execute(&[Step::Trash {
            path: doomed.clone(),
            trash: trash.clone(),
        }])
        .expect("steps execute");

        assert!(!doomed.exists(), "the file is deleted");
        assert!(!trash.exists(), "the trash entry is purged");
    }

    #[test]
    fn test_rolls_back_every_earlier_step_when_a_later_one_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let edited = dir.path().join("edited.rs");
        let doomed = dir.path().join("doomed.rs");
        fs::write(&edited, "original").expect("seed edited");
        fs::write(&doomed, "still here").expect("seed doomed");
        let trash = dir.path().join(".doomed.rs.mcpls-trash1");
        // A path whose parent does not exist cannot be written.
        let unwritable: PathBuf = dir.path().join("missing-dir").join("bad.rs");

        let result = execute(&[
            Step::Write {
                path: edited.clone(),
                content: "changed".to_string(),
                previous: Some("original".to_string()),
            },
            Step::Trash {
                path: doomed.clone(),
                trash,
            },
            Step::Write {
                path: unwritable,
                content: "never lands".to_string(),
                previous: None,
            },
        ]);

        assert!(result.is_err(), "the run fails");
        assert_eq!(
            fs::read_to_string(&edited).expect("read"),
            "original",
            "the earlier write is rolled back"
        );
        assert_eq!(
            fs::read_to_string(&doomed).expect("read"),
            "still here",
            "the trashed file comes back"
        );
    }

    #[test]
    fn test_rollback_removes_a_file_the_run_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let created = dir.path().join("created.rs");
        let unwritable: PathBuf = dir.path().join("missing-dir").join("bad.rs");

        let result = execute(&[
            Step::Write {
                path: created.clone(),
                content: "new file".to_string(),
                previous: None,
            },
            Step::Write {
                path: unwritable,
                content: "never lands".to_string(),
                previous: None,
            },
        ]);

        assert!(result.is_err());
        assert!(!created.exists(), "a created file is removed on rollback");
    }

    #[test]
    fn test_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        fs::write(&path, "old").expect("seed");

        execute(&[Step::Write {
            path,
            content: "new".to_string(),
            previous: Some("old".to_string()),
        }])
        .expect("steps execute");

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("mcpls-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files are cleaned up");
    }
}
