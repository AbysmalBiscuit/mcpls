//! Writing LSP `WorkspaceEdit`s to the working tree.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub mod journal;
pub mod offsets;
pub mod plan;

pub use journal::{Step, execute};
pub use offsets::LineTable;
pub use plan::{EditPlan, Operation};

use crate::bridge::encoding::{EncodingConverter, PositionEncoding};
use crate::bridge::translator::{ResourceOperation, resource_operations_from_plan};
use crate::bridge::{uri_to_path, validate_path_against_roots};
use crate::config::ApplyConfig;
use crate::error::{Error, Result};

/// One file the applier wrote.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Absolute path.
    pub path: PathBuf,
    /// Number of text edits applied to it. Zero for a file the edit only
    /// created.
    pub edits: usize,
}

/// What an apply did, returned to the caller so it knows which of its
/// cached file contents are now stale.
#[derive(Debug, Clone)]
pub struct ApplySummary {
    /// Files whose content was written. Renamed and deleted paths are in
    /// `resource_operations` instead.
    pub files_changed: Vec<FileChange>,
    /// File-system operations performed.
    pub resource_operations: Vec<ResourceOperation>,
}

/// Applies validated `WorkspaceEdit`s within a set of workspace roots.
#[derive(Debug)]
pub struct Applier {
    roots: Vec<PathBuf>,
    config: ApplyConfig,
}

impl Applier {
    /// Build an applier confined to `roots`.
    #[must_use]
    pub const fn new(roots: Vec<PathBuf>, config: ApplyConfig) -> Self {
        Self { roots, config }
    }

    /// Which tools this applier permits to write. Read by
    /// `Translator::applier_for` to gate a call before any LSP request.
    #[must_use]
    pub const fn config(&self) -> &ApplyConfig {
        &self.config
    }

    /// Plan `plan` into a journal and execute it.
    ///
    /// `encoding` is the encoding the server that produced `plan`
    /// negotiated, from `Translator::position_encoding_for`. Passing the
    /// wrong one misplaces every edit after a non-ASCII character.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when an operation targets a path
    /// outside the workspace, deletes a file without
    /// `apply.allow_file_deletion`, or resolves to an invalid range, and
    /// [`Error::ApplyPartiallyFailed`] when a step fails after another has
    /// already landed.
    pub async fn apply(&self, plan: EditPlan, encoding: PositionEncoding) -> Result<ApplySummary> {
        let roots = self.roots.clone();
        let config = self.config.clone();
        // Planning reads files and execution writes them, and the caller
        // holds the translator's apply mutex across this call, so none of
        // it belongs on a runtime thread.
        tokio::task::spawn_blocking(move || {
            let planner = Planner::new(&roots, &config, encoding);
            let (steps, files_changed) = planner.plan(&plan)?;
            journal::execute(&steps)?;
            Ok(ApplySummary {
                files_changed,
                resource_operations: resource_operations_from_plan(&plan),
            })
        })
        .await
        .map_err(|e| Error::ApplyRefused(format!("apply task panicked: {e}")))?
    }
}

/// What a path holds at some point during planning.
#[derive(Clone)]
enum Presence {
    /// The path does not exist.
    Absent,
    /// The path holds this text.
    Text(String),
    /// The path exists but is not editable text: a directory, or a file
    /// that is not valid UTF-8.
    Opaque,
}

/// Walks a plan's operations in order, resolving each against an overlay of
/// what the tree looks like at that point, and emitting journal steps.
struct Planner<'a> {
    roots: &'a [PathBuf],
    config: &'a ApplyConfig,
    converter: EncodingConverter,
    overlay: HashMap<PathBuf, Presence>,
    steps: Vec<Step>,
    files_changed: Vec<FileChange>,
}

impl<'a> Planner<'a> {
    fn new(roots: &'a [PathBuf], config: &'a ApplyConfig, encoding: PositionEncoding) -> Self {
        Self {
            roots,
            config,
            converter: EncodingConverter::new(encoding),
            overlay: HashMap::new(),
            steps: Vec::new(),
            files_changed: Vec::new(),
        }
    }

    fn plan(mut self, plan: &EditPlan) -> Result<(Vec<Step>, Vec<FileChange>)> {
        for operation in plan.operations() {
            match operation {
                Operation::Edit { uri, edits, .. } => self.plan_edit(uri, edits)?,
                Operation::Create {
                    uri,
                    overwrite,
                    ignore_if_exists,
                } => self.plan_create(uri, *overwrite, *ignore_if_exists)?,
                Operation::Rename {
                    old,
                    new,
                    overwrite,
                    ignore_if_exists,
                } => self.plan_rename(old, new, *overwrite, *ignore_if_exists)?,
                Operation::Delete {
                    uri,
                    recursive,
                    ignore_if_not_exists,
                } => self.plan_delete(uri, *recursive, *ignore_if_not_exists)?,
            }
        }
        Ok((self.steps, self.files_changed))
    }

    /// Absolute, confined path for `uri`.
    ///
    /// An existing path is canonicalized and checked against the roots. A
    /// path that does not exist yet cannot be canonicalized, so its parent
    /// is checked instead and the file name joined onto the canonical
    /// parent, which yields the same shape of path either way and so the
    /// same overlay key.
    fn resolve(&self, uri: &lsp_types::Uri) -> Result<PathBuf> {
        let path = uri_to_path(uri).ok_or_else(|| Error::InvalidUri(uri.as_str().to_string()))?;
        if path.exists() {
            return validate_path_against_roots(&path, self.roots);
        }
        let parent = path.parent().ok_or_else(|| {
            Error::ApplyRefused(format!("{} has no parent directory", path.display()))
        })?;
        let file_name = path
            .file_name()
            .ok_or_else(|| Error::ApplyRefused(format!("{} has no file name", path.display())))?;
        let canonical_parent = validate_path_against_roots(parent, self.roots)?;
        Ok(canonical_parent.join(file_name))
    }

    /// What `path` holds at this point in the plan, reading disk on the
    /// first touch and the overlay thereafter.
    fn presence(&self, path: &Path) -> Presence {
        if let Some(known) = self.overlay.get(path) {
            return known.clone();
        }
        if !path.exists() {
            return Presence::Absent;
        }
        if path.is_dir() {
            return Presence::Opaque;
        }
        fs::read_to_string(path).map_or(Presence::Opaque, Presence::Text)
    }

    fn record_change(&mut self, path: &Path, edits: usize) {
        if let Some(existing) = self
            .files_changed
            .iter_mut()
            .find(|change| change.path == path)
        {
            existing.edits += edits;
        } else {
            self.files_changed.push(FileChange {
                path: path.to_path_buf(),
                edits,
            });
        }
    }

    fn plan_edit(&mut self, uri: &lsp_types::Uri, edits: &[lsp_types::TextEdit]) -> Result<()> {
        let path = self.resolve(uri)?;
        let previous = match self.presence(&path) {
            Presence::Text(text) => text,
            Presence::Absent => {
                return Err(Error::ApplyRefused(format!(
                    "{} does not exist, so its edits cannot be applied",
                    path.display()
                )));
            }
            Presence::Opaque => {
                return Err(Error::ApplyRefused(format!(
                    "{} is not an editable text file",
                    path.display()
                )));
            }
        };

        let mut content = previous.clone();
        // `edits` is ordered so each splice leaves every not-yet-applied
        // range valid, so the table is rebuilt per edit against the text as
        // it stands.
        for edit in edits {
            let table = LineTable::new(&content);
            let range = table.byte_range(edit.range, &self.converter)?;
            content.replace_range(range, &edit.new_text);
        }

        self.overlay
            .insert(path.clone(), Presence::Text(content.clone()));
        self.steps.push(Step::Write {
            path: path.clone(),
            content,
            previous: Some(previous),
        });
        self.record_change(&path, edits.len());
        Ok(())
    }

    fn plan_create(
        &mut self,
        uri: &lsp_types::Uri,
        overwrite: bool,
        ignore_if_exists: bool,
    ) -> Result<()> {
        let path = self.resolve(uri)?;
        let previous = match self.presence(&path) {
            Presence::Absent => None,
            existing => {
                // `overwrite` wins over `ignore_if_exists` per the LSP spec.
                if !overwrite {
                    if ignore_if_exists {
                        return Ok(());
                    }
                    return Err(Error::ApplyRefused(format!(
                        "{} already exists and the edit did not ask to overwrite it",
                        path.display()
                    )));
                }
                match existing {
                    Presence::Text(text) => Some(text),
                    _ => {
                        return Err(Error::ApplyRefused(format!(
                            "{} exists and is not a text file, so it cannot be overwritten",
                            path.display()
                        )));
                    }
                }
            }
        };

        self.overlay
            .insert(path.clone(), Presence::Text(String::new()));
        self.steps.push(Step::Write {
            path: path.clone(),
            content: String::new(),
            previous,
        });
        self.record_change(&path, 0);
        Ok(())
    }

    fn plan_rename(
        &mut self,
        old: &lsp_types::Uri,
        new: &lsp_types::Uri,
        overwrite: bool,
        ignore_if_exists: bool,
    ) -> Result<()> {
        let from = self.resolve(old)?;
        let to = self.resolve(new)?;

        let moving = match self.presence(&from) {
            Presence::Absent => {
                return Err(Error::ApplyRefused(format!(
                    "{} does not exist, so it cannot be renamed",
                    from.display()
                )));
            }
            present => present,
        };

        if !matches!(self.presence(&to), Presence::Absent) {
            if !overwrite {
                if ignore_if_exists {
                    return Ok(());
                }
                return Err(Error::ApplyRefused(format!(
                    "{} already exists and the edit did not ask to overwrite it",
                    to.display()
                )));
            }
            let trash = self.trash_path(&to)?;
            self.steps.push(Step::Trash {
                path: to.clone(),
                trash,
            });
        }

        self.overlay.insert(from.clone(), Presence::Absent);
        self.overlay.insert(to.clone(), moving);
        self.steps.push(Step::Move { from, to });
        Ok(())
    }

    fn plan_delete(
        &mut self,
        uri: &lsp_types::Uri,
        recursive: bool,
        ignore_if_not_exists: bool,
    ) -> Result<()> {
        if !self.config.allow_file_deletion {
            return Err(Error::ApplyRefused(format!(
                "{} would be deleted, but `apply.allow_file_deletion` is false",
                uri.as_str()
            )));
        }

        let path = self.resolve(uri)?;
        if matches!(self.presence(&path), Presence::Absent) {
            if ignore_if_not_exists {
                return Ok(());
            }
            return Err(Error::ApplyRefused(format!(
                "{} does not exist, so it cannot be deleted",
                path.display()
            )));
        }

        if !recursive
            && path.is_dir()
            && fs::read_dir(&path)
                .map_err(|e| Error::FileIo {
                    path: path.clone(),
                    source: e,
                })?
                .next()
                .is_some()
        {
            return Err(Error::ApplyRefused(format!(
                "{} is a non-empty directory and the edit did not ask for a recursive delete",
                path.display()
            )));
        }

        let trash = self.trash_path(&path)?;
        self.overlay.insert(path.clone(), Presence::Absent);
        self.steps.push(Step::Trash { path, trash });
        Ok(())
    }

    /// Sibling path a removed file is parked at until the run finishes.
    /// The step index keeps two removals in one directory from colliding.
    fn trash_path(&self, path: &Path) -> Result<PathBuf> {
        let parent = path.parent().ok_or_else(|| {
            Error::ApplyRefused(format!("{} has no parent directory", path.display()))
        })?;
        let file_name = path
            .file_name()
            .ok_or_else(|| Error::ApplyRefused(format!("{} has no file name", path.display())))?
            .to_string_lossy()
            .into_owned();
        let index = self.steps.len();
        Ok(parent.join(format!(".{file_name}.mcpls-trash{index}")))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::mutable_key_type)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;

    use lsp_types::{
        CreateFile, DeleteFile, DocumentChangeOperation, DocumentChanges, OneOf,
        OptionalVersionedTextDocumentIdentifier, Position, Range, RenameFile, ResourceOp,
        TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
    };

    use super::{Applier, EditPlan};
    use crate::bridge::{PositionEncoding, path_to_uri};
    use crate::config::ApplyConfig;

    fn uri_for(path: &Path) -> Uri {
        path_to_uri(path).expect("path converts to a uri")
    }

    fn permissive() -> ApplyConfig {
        ApplyConfig {
            rename: true,
            format_document: true,
            code_actions: true,
            allow_file_deletion: true,
        }
    }

    fn plan_replacing(uri: Uri, range: Range, text: &str) -> EditPlan {
        let mut changes = HashMap::new();
        changes.insert(
            uri,
            vec![TextEdit {
                range,
                new_text: text.to_string(),
            }],
        );
        EditPlan::from_workspace_edit(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds")
    }

    #[tokio::test]
    async fn test_applies_a_text_edit_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rs");
        fs::write(&path, "fn old() {}\n").expect("seed");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        let plan = plan_replacing(
            uri_for(&path),
            Range::new(Position::new(0, 3), Position::new(0, 6)),
            "new",
        );

        let summary = applier
            .apply(plan, PositionEncoding::Utf16)
            .await
            .expect("apply succeeds");

        assert_eq!(summary.files_changed.len(), 1);
        assert_eq!(summary.files_changed[0].edits, 1);
        assert_eq!(fs::read_to_string(&path).expect("read"), "fn new() {}\n");
    }

    #[tokio::test]
    async fn test_utf8_and_utf16_columns_land_in_different_places() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wide.rs");
        // "é" is one UTF-16 unit and two UTF-8 bytes, so the same column
        // means a different byte offset in each encoding:
        //   bytes    a=0  é=1..2  ' '=3  '='=4  ' '=5  x=6 ...
        //   utf-16   a=0  é=1     ' '=2  '='=3  ' '=4  x=5 ...
        // Column 3 to 4 is therefore "=" in UTF-16 and " " in UTF-8, and
        // both land on character boundaries, so neither apply errors out.
        let seed = "aé = xyz;\n";
        let columns = Range::new(Position::new(0, 3), Position::new(0, 4));

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());

        fs::write(&path, seed).expect("seed");
        applier
            .apply(
                plan_replacing(uri_for(&path), columns, "Z"),
                PositionEncoding::Utf16,
            )
            .await
            .expect("apply succeeds");
        let utf16_result = fs::read_to_string(&path).expect("read");

        fs::write(&path, seed).expect("reseed");
        applier
            .apply(
                plan_replacing(uri_for(&path), columns, "Z"),
                PositionEncoding::Utf8,
            )
            .await
            .expect("apply succeeds");
        let utf8_result = fs::read_to_string(&path).expect("read");

        assert_eq!(utf16_result, "aé Z xyz;\n", "UTF-16 column 3 is the '='");
        assert_eq!(utf8_result, "aéZ= xyz;\n", "UTF-8 byte 3 is the space");
        assert_ne!(
            utf16_result, utf8_result,
            "an applier that ignores the negotiated encoding corrupts one of these"
        );
    }

    #[tokio::test]
    async fn test_creates_a_file_and_then_edits_it_in_one_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new.rs");
        let uri = uri_for(&dir.path().join("new.rs"));

        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri.clone(),
                    options: None,
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                        new_text: "pub fn generated() {}\n".to_string(),
                    })],
                }),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        applier
            .apply(plan, PositionEncoding::Utf16)
            .await
            .expect("create-then-edit succeeds");

        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "pub fn generated() {}\n"
        );
    }

    #[tokio::test]
    async fn test_renames_a_file_and_then_edits_its_new_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old.rs");
        let new = dir.path().join("new.rs");
        fs::write(&old, "fn old_name() {}\n").expect("seed");
        let new_uri = uri_for(&dir.path().join("new.rs"));

        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                    old_uri: uri_for(&old),
                    new_uri: new_uri.clone(),
                    options: None,
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: new_uri,
                        version: None,
                    },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range::new(Position::new(0, 3), Position::new(0, 11)),
                        new_text: "new_name".to_string(),
                    })],
                }),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        applier
            .apply(plan, PositionEncoding::Utf16)
            .await
            .expect("rename-then-edit succeeds");

        assert!(!old.exists());
        assert_eq!(
            fs::read_to_string(&new).expect("read"),
            "fn new_name() {}\n"
        );
    }

    #[tokio::test]
    async fn test_create_with_ignore_if_exists_leaves_the_file_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("existing.rs");
        fs::write(&path, "keep me\n").expect("seed");

        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri_for(&path),
                    options: Some(lsp_types::CreateFileOptions {
                        overwrite: None,
                        ignore_if_exists: Some(true),
                    }),
                    annotation_id: None,
                })),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        applier
            .apply(plan, PositionEncoding::Utf16)
            .await
            .expect("an ignored create is not a failure");

        assert_eq!(fs::read_to_string(&path).expect("read"), "keep me\n");
    }

    #[tokio::test]
    async fn test_refuses_a_path_outside_every_root() {
        let inside = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let path = outside.path().join("escape.rs");
        fs::write(&path, "x\n").expect("seed");

        let applier = Applier::new(vec![inside.path().to_path_buf()], permissive());
        let plan = plan_replacing(
            uri_for(&path),
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            "y",
        );

        assert!(applier.apply(plan, PositionEncoding::Utf16).await.is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "x\n",
            "the file outside the workspace is untouched"
        );
    }

    #[tokio::test]
    async fn test_refuses_deletion_when_the_config_forbids_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("doomed.rs");
        fs::write(&path, "x\n").expect("seed");

        let config = ApplyConfig {
            allow_file_deletion: false,
            ..permissive()
        };
        let applier = Applier::new(vec![dir.path().to_path_buf()], config);
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Delete(DeleteFile {
                    uri: uri_for(&path),
                    options: None,
                })),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let error = applier
            .apply(plan, PositionEncoding::Utf16)
            .await
            .expect_err("deletion is refused");
        assert!(
            error.to_string().contains("apply.allow_file_deletion"),
            "the error names the key that would permit it: {error}"
        );
        assert!(path.exists(), "the file survives a refused deletion");
    }

    #[tokio::test]
    async fn test_nothing_is_written_when_one_operation_is_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.rs");
        let missing = dir.path().join("missing.rs");
        fs::write(&good, "fn a() {}\n").expect("seed");

        let mut changes = HashMap::new();
        changes.insert(
            uri_for(&good),
            vec![TextEdit {
                range: Range::new(Position::new(0, 3), Position::new(0, 4)),
                new_text: "b".to_string(),
            }],
        );
        changes.insert(
            uri_for(&dir.path().join("missing.rs")),
            vec![TextEdit {
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                new_text: "z".to_string(),
            }],
        );
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        assert!(applier.apply(plan, PositionEncoding::Utf16).await.is_err());
        assert_eq!(
            fs::read_to_string(&good).expect("read"),
            "fn a() {}\n",
            "planning fails before any step runs"
        );
        assert!(!missing.exists());
    }
}
