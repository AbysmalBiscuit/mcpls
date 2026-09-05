//! Writing LSP `WorkspaceEdit`s to the working tree.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Mutex;

pub mod journal;
pub mod offsets;
pub mod plan;

pub use journal::{Step, execute};
pub use offsets::LineTable;
pub use plan::{EditPlan, Operation};

use crate::bridge::encoding::{EncodingConverter, PositionEncoding};
use crate::bridge::translator::ResourceOperation;
use crate::bridge::{lock_std, uri_to_path, validate_path_against_roots};
use crate::config::ApplyConfig;
use crate::error::{Error, Result};

/// Paths an apply has committed to changing, held until whoever tracks open
/// documents has dropped them.
///
/// [`Applier::apply`] records them once planning succeeds and *before* the
/// journal runs, so they outlive the caller: the write itself completes on a
/// blocking thread whether or not anyone is still awaiting it, and dropping
/// the awaiting future -- a cancelled request, a client disconnect, shutdown
/// -- would otherwise lose the summary and with it every path that had to be
/// forgotten. Whatever a cancelled apply leaves here, the next apply or the
/// next tool call that opens a document takes and acts on.
///
/// Recorded from the plan rather than from the outcome, so a run that fails
/// partway and rolls back is covered too: a rollback that could not restore
/// a file leaves it holding the new content, and one that could still moved
/// the file out and back.
#[derive(Debug, Clone, Default)]
pub struct InvalidationQueue(Arc<StdMutex<Vec<PathBuf>>>);

impl InvalidationQueue {
    /// Add every path in `paths` that is not already queued.
    pub fn extend(&self, paths: &[PathBuf]) {
        let mut queued = lock_std(&self.0);
        for path in paths {
            if !queued.contains(path) {
                queued.push(path.clone());
            }
        }
    }

    /// Take everything queued, leaving the queue empty.
    #[must_use]
    pub fn take(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *lock_std(&self.0))
    }
}

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
    /// File-system operations actually performed. An operation the plan
    /// asked for but the applier skipped -- a create whose target already
    /// exists under `ignoreIfExists`, say -- is absent, so this together
    /// with `files_changed` says whether the tree changed at all.
    pub resource_operations: Vec<ResourceOperation>,
    /// Every absolute path whose on-disk content no longer matches what a
    /// cache of it would hold: written files plus both ends of a rename and
    /// the source of a delete. A caller tracking open documents must drop
    /// its entry for each of these.
    pub paths_invalidated: Vec<PathBuf>,
}

/// Applies validated `WorkspaceEdit`s within a set of workspace roots.
#[derive(Debug)]
pub struct Applier {
    roots: Vec<PathBuf>,
    config: ApplyConfig,
    /// Serializes applies. Held by the blocking task that does the writing
    /// rather than by [`Self::apply`]'s future, so dropping that future --
    /// a cancelled request, a disconnected client, shutdown -- cannot
    /// release the lock while the journal is still rewriting the tree.
    lock: Arc<Mutex<()>>,
}

impl Applier {
    /// Build an applier confined to `roots`.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>, config: ApplyConfig) -> Self {
        Self {
            roots,
            config,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Which tools this applier permits to write. Read by
    /// `Translator::applier_for` to gate a call before any LSP request.
    #[must_use]
    pub const fn config(&self) -> &ApplyConfig {
        &self.config
    }

    /// Plan `plan` into a journal and execute it.
    ///
    /// Applies are serialized against each other: two concurrent ones would
    /// otherwise both plan against the same pre-edit content and the second
    /// would overwrite the first.
    ///
    /// `encoding` is the encoding the server that produced `plan`
    /// negotiated, from `Translator::position_encoding_for`. Passing the
    /// wrong one misplaces every edit after a non-ASCII character.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when the applier has no workspace
    /// roots configured, when an operation targets a path outside the
    /// workspace, deletes a file without `apply.allow_file_deletion`, or
    /// resolves to an invalid range, and [`Error::ApplyPartiallyFailed`]
    /// when a step fails after another has already landed.
    ///
    /// Every path this is about to change is recorded in `invalidated`
    /// before the journal runs, so a caller that stops awaiting this call
    /// still leaves them somewhere the next caller will find them. See
    /// [`InvalidationQueue`].
    pub async fn apply(
        &self,
        plan: EditPlan,
        encoding: PositionEncoding,
        invalidated: &InvalidationQueue,
    ) -> Result<ApplySummary> {
        // `validate_path_against_roots` treats an empty root list as "any
        // path is in bounds", which is fine for the read-only queries it
        // was written for but would let a misconfigured applier write
        // anywhere the server names a URI. Refuse before any path is
        // resolved rather than relying on that fallback here.
        if self.roots.is_empty() {
            return Err(Error::ApplyRefused(
                "no workspace roots are configured, so the applier refuses to write anywhere"
                    .to_string(),
            ));
        }

        let roots = self.roots.clone();
        let config = self.config.clone();
        let invalidated = invalidated.clone();
        // Taken here and moved into the blocking task, so the lock's
        // lifetime is the write's rather than this future's: a dropped
        // future cancels the await but not the task already writing.
        let guard = Arc::clone(&self.lock).lock_owned().await;
        // Planning reads files and execution writes them, so none of it
        // belongs on a runtime thread.
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let outcome = Planner::new(&roots, &config, encoding).plan(&plan)?;
            // Queued before the first step runs: from here on the tree is
            // going to change whether or not anyone is still awaiting this.
            invalidated.extend(&outcome.paths_invalidated);
            journal::execute(&outcome.steps)?;
            Ok(ApplySummary {
                files_changed: outcome.files_changed,
                resource_operations: outcome.resource_operations,
                paths_invalidated: outcome.paths_invalidated,
            })
        })
        .await
        .map_err(|e| Error::ApplyRefused(format!("apply task panicked: {e}")))?
    }
}

/// `path` with its longest existing prefix canonicalized, which is how
/// [`Planner::resolve`] shapes every overlay key: an existing path becomes
/// its canonical self, and one that does not exist yet becomes its canonical
/// parent plus the names below it.
fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => normalize(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Whether the filesystem marks `path` read-only.
///
/// A path that cannot be stat'd is not read-only for this purpose: it is
/// either absent, which is fine, or unreadable, which the operation itself
/// will report.
fn is_read_only(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.permissions().readonly())
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

/// What planning a whole `EditPlan` produced.
struct PlannedEdit {
    steps: Vec<Step>,
    files_changed: Vec<FileChange>,
    resource_operations: Vec<ResourceOperation>,
    paths_invalidated: Vec<PathBuf>,
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
    resource_operations: Vec<ResourceOperation>,
    paths_invalidated: Vec<PathBuf>,
    read_only: Vec<PathBuf>,
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
            resource_operations: Vec::new(),
            paths_invalidated: Vec::new(),
            read_only: Vec::new(),
        }
    }

    fn plan(mut self, plan: &EditPlan) -> Result<PlannedEdit> {
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
        // Collected across the whole plan rather than refused at the first
        // one: a Perforce or ClearCase-style checkout marks every unopened
        // file read-only, so a thirty-file rename would otherwise be thirty
        // rounds of chmod-and-retry with no way to see the whole list.
        if !self.read_only.is_empty() {
            let names: Vec<String> = self
                .read_only
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            return Err(Error::ApplyRefused(format!(
                "this edit would change files the filesystem marks read-only, which mcpls \
                 refuses to do: {}",
                names.join(", ")
            )));
        }

        Ok(PlannedEdit {
            steps: self.steps,
            files_changed: self.files_changed,
            resource_operations: self.resource_operations,
            paths_invalidated: self.paths_invalidated,
        })
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

        // The overlay is keyed on each path an operation names, never on
        // that path's children, so a file under a directory the same edit
        // creates or moves resolves against the tree as it stands before
        // the move. gopls emits exactly this for a package move. Nothing is
        // lost by refusing -- the journal would fail mid-run and roll back
        // -- but the caller gets a message that says what is unsupported
        // instead of one naming a directory it never mentioned.
        if let Some(ancestor) = self.planned_ancestor(&path) {
            return Err(Error::ApplyRefused(format!(
                "{} sits under {}, which this edit also creates or moves; mcpls cannot apply \
                 an edit to a file under a directory the same edit relocates",
                path.display(),
                ancestor.display()
            )));
        }

        if path.exists() {
            return validate_path_against_roots(&path, self.roots);
        }
        let parent = path.parent().ok_or_else(|| {
            Error::ApplyRefused(format!("{} has no parent directory", path.display()))
        })?;
        let file_name = path
            .file_name()
            .ok_or_else(|| Error::ApplyRefused(format!("{} has no file name", path.display())))?;
        // Checked before confinement, which canonicalizes and would
        // otherwise report a missing directory as a file-I/O failure.
        // rust-analyzer's "create module" quick fix lands here.
        if !parent.exists() {
            return Err(Error::ApplyRefused(format!(
                "{} would be created in {}, which does not exist; mcpls does not create \
                 directories",
                path.display(),
                parent.display()
            )));
        }
        let canonical_parent = validate_path_against_roots(parent, self.roots)?;
        Ok(canonical_parent.join(file_name))
    }

    /// The directory an earlier operation in this plan creates or moves that
    /// `path` sits under, if any.
    fn planned_ancestor(&self, path: &Path) -> Option<PathBuf> {
        path.ancestors()
            .skip(1)
            .map(normalize)
            .find(|ancestor| self.overlay.contains_key(ancestor))
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

    /// Refuse an operation that destroys `path`'s existing content unless
    /// `apply.allow_file_deletion` permits it.
    ///
    /// An overwrite discards content as completely as a delete does, and any
    /// server can ask for one, so both answer to the same key: a user who
    /// turned deletion off is entitled to expect that nothing is destroyed.
    fn require_destruction_allowed(&self, path: &Path, what: &str) -> Result<()> {
        if self.config.allow_file_deletion {
            return Ok(());
        }
        Err(Error::ApplyRefused(format!(
            "{} would be {what}, but `apply.allow_file_deletion` is false",
            path.display()
        )))
    }

    /// Record that this plan would change `path`, which the filesystem
    /// marks read-only.
    ///
    /// Renaming onto a read-only destination succeeds on Unix and fails
    /// with access-denied on Windows, and deleting one differs the same
    /// way, so taking the platform's answer would mean the same edit
    /// silently destroying a protected file on one machine and erroring on
    /// another. mcpls refuses on both: a file is marked read-only
    /// deliberately, and an edit is not the place to override that. Noted
    /// rather than returned so [`Self::plan`] can name every such file at
    /// once.
    fn note_read_only(&mut self, path: &Path) {
        if is_read_only(path) && !self.read_only.iter().any(|known| known == path) {
            self.read_only.push(path.to_path_buf());
        }
    }

    /// Note that `path`'s on-disk content no longer matches anything cached
    /// against it, so a caller tracking open documents must drop its entry.
    fn invalidate(&mut self, path: &Path) {
        if !self.paths_invalidated.iter().any(|known| known == path) {
            self.paths_invalidated.push(path.to_path_buf());
        }
    }

    fn record_change(&mut self, path: &Path, edits: usize) {
        self.invalidate(path);
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
        self.note_read_only(&path);
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
                self.require_destruction_allowed(&path, "truncated to empty")?;
                self.note_read_only(&path);
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
        self.resource_operations.push(ResourceOperation {
            kind: "create".to_string(),
            uri: uri.to_string(),
            new_uri: None,
        });
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
            self.require_destruction_allowed(&to, "replaced by a rename")?;
            self.note_read_only(&to);
            let trash = self.trash_path(&to)?;
            self.steps.push(Step::Trash {
                path: to.clone(),
                trash,
            });
        }

        self.overlay.insert(from.clone(), Presence::Absent);
        self.overlay.insert(to.clone(), moving);
        self.invalidate(&from);
        self.invalidate(&to);
        self.steps.push(Step::Move { from, to });
        self.resource_operations.push(ResourceOperation {
            kind: "rename".to_string(),
            uri: old.to_string(),
            new_uri: Some(new.to_string()),
        });
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

        self.note_read_only(&path);
        let trash = self.trash_path(&path)?;
        self.overlay.insert(path.clone(), Presence::Absent);
        self.invalidate(&path);
        self.steps.push(Step::Trash { path, trash });
        self.resource_operations.push(ResourceOperation {
            kind: "delete".to_string(),
            uri: uri.to_string(),
            new_uri: None,
        });
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

    use super::{Applier, EditPlan, InvalidationQueue};
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
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
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
                &InvalidationQueue::default(),
            )
            .await
            .expect("apply succeeds");
        let utf16_result = fs::read_to_string(&path).expect("read");

        fs::write(&path, seed).expect("reseed");
        applier
            .apply(
                plan_replacing(uri_for(&path), columns, "Z"),
                PositionEncoding::Utf8,
                &InvalidationQueue::default(),
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
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
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
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
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
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
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

        assert!(
            applier
                .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "x\n",
            "the file outside the workspace is untouched"
        );
    }

    /// A symlink inside a root pointing outside it is the interesting
    /// escape: the path the server names is in bounds, and only
    /// canonicalizing it shows where the write would land.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_refuses_a_symlink_that_leaves_the_workspace() {
        let inside = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let target = outside.path().join("secret.rs");
        fs::write(&target, "x\n").expect("seed");
        let link = inside.path().join("innocent.rs");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let applier = Applier::new(vec![inside.path().to_path_buf()], permissive());
        let plan = plan_replacing(
            uri_for(&link),
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            "y",
        );

        assert!(
            applier
                .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "x\n",
            "a path inside a root that resolves outside one is still outside"
        );
    }

    /// The whole delete path through the applier: planned, journalled, and
    /// the trash entry purged, leaving neither the file nor a stray sibling.
    #[tokio::test]
    async fn test_deletes_a_file_when_the_config_permits_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("doomed.rs");
        fs::write(&path, "bye\n").expect("seed");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
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

        let summary = applier
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
            .await
            .expect("the delete applies");

        assert!(!path.exists(), "the file is gone");
        assert_eq!(summary.resource_operations.len(), 1);
        assert_eq!(summary.resource_operations[0].kind, "delete");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("mcpls-trash"))
            .collect();
        assert!(leftovers.is_empty(), "the trash entry is purged");
    }

    #[tokio::test]
    async fn test_refuses_to_write_with_no_workspace_roots_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rs");
        fs::write(&path, "fn old() {}\n").expect("seed");

        let config = ApplyConfig {
            rename: true,
            ..ApplyConfig::default()
        };
        let applier = Applier::new(vec![], config);
        let plan = plan_replacing(
            uri_for(&path),
            Range::new(Position::new(0, 3), Position::new(0, 6)),
            "new",
        );

        assert!(
            applier
                .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "fn old() {}\n",
            "no workspace roots means nothing is written, not just an error returned"
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
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
            .await
            .expect_err("deletion is refused");
        assert!(
            error.to_string().contains("apply.allow_file_deletion"),
            "the error names the key that would permit it: {error}"
        );
        assert!(path.exists(), "the file survives a refused deletion");
    }

    /// The write runs on a blocking thread that dropping the awaiting
    /// future cannot cancel, so what it changed must not be carried only by
    /// the summary that future would have returned. Pressing Esc during a
    /// slow workspace rename is exactly this.
    ///
    /// The journal is held at the step until the caller's future has
    /// actually been dropped, so the cancellation lands strictly before any
    /// byte is written instead of racing the write.
    #[tokio::test]
    async fn test_an_apply_dropped_before_it_writes_still_records_what_it_changed() {
        use std::sync::{Arc, Barrier};

        use tokio::time::{Duration, sleep};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rs");
        fs::write(&path, "fn old() {}\n").expect("seed");
        let canonical = path.canonicalize().expect("the fixture exists");

        let arrived = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        super::journal::install_step_barrier(Some(super::journal::StepBarrier {
            path: canonical.clone(),
            arrived: Arc::clone(&arrived),
            resume: Arc::clone(&resume),
        }));

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        let queue = InvalidationQueue::default();
        let plan = plan_replacing(
            uri_for(&path),
            Range::new(Position::new(0, 3), Position::new(0, 6)),
            "new",
        );
        let task = {
            let queue = queue.clone();
            tokio::spawn(async move { applier.apply(plan, PositionEncoding::Utf16, &queue).await })
        };

        // Returns once the journal is at the step: planning is done, the
        // paths are recorded, and nothing has been written.
        let waiting = Arc::clone(&arrived);
        tokio::task::spawn_blocking(move || waiting.wait())
            .await
            .expect("the journal reaches the step");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "fn old() {}\n",
            "the barrier holds the journal before it writes anything"
        );

        // The caller stops awaiting. Awaiting the aborted handle returns
        // only once the future has actually been dropped, so the write is
        // released strictly afterwards.
        task.abort();
        assert!(task.await.is_err(), "the caller's future is dropped");

        let waiting = Arc::clone(&resume);
        tokio::task::spawn_blocking(move || waiting.wait())
            .await
            .expect("the journal resumes");

        // The queue is filled before the write, so the write is what has to
        // be waited for -- reading it as soon as the queue fills would race
        // the whole journal.
        let mut content = String::new();
        for _ in 0..500 {
            content = fs::read_to_string(&path).expect("read");
            if content == "fn new() {}\n" {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            content, "fn new() {}\n",
            "the write completed without anyone awaiting it"
        );
        assert_eq!(
            queue.take(),
            vec![canonical],
            "and the path it changed is queued for whoever drains next"
        );

        super::journal::install_step_barrier(None);
    }

    /// Mark `path` read-only, or writable again.
    ///
    /// `set_readonly` is the one spelling that means the same thing on both
    /// platforms -- it clears the write bits on Unix and sets the read-only
    /// attribute on Windows -- which is what lets the tests below run on
    /// Windows, the platform the refusal exists for. They restore write
    /// access before returning: Windows will not remove a read-only file, so
    /// the temporary directory could not otherwise clean itself up.
    fn set_read_only(path: &Path, read_only: bool) {
        let mut permissions = fs::metadata(path).expect("stat").permissions();
        permissions.set_readonly(read_only);
        fs::set_permissions(path, permissions).expect("set permissions");
    }

    /// Rewriting a read-only file succeeds on Unix and fails with
    /// access-denied on Windows. mcpls refuses on both rather than letting
    /// the platform decide whether a protected file is rewritten, and names
    /// every such file at once so a checkout that marks all of them
    /// read-only does not become a chmod-and-retry loop.
    #[tokio::test]
    async fn test_refuses_to_rewrite_read_only_files_and_names_them_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.rs");
        let second = dir.path().join("second.rs");
        for path in [&first, &second] {
            fs::write(path, "fn old() {}\n").expect("seed");
            set_read_only(path, true);
        }

        let mut changes = HashMap::new();
        for path in [&first, &second] {
            changes.insert(
                uri_for(path),
                vec![TextEdit {
                    range: Range::new(Position::new(0, 3), Position::new(0, 6)),
                    new_text: "new".to_string(),
                }],
            );
        }
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        let error = applier
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
            .await
            .expect_err("a read-only file is not rewritten");

        let message = error.to_string();
        assert!(
            message.contains("read-only"),
            "the error says why: {message}"
        );
        for path in [&first, &second] {
            assert!(
                message.contains(&path.display().to_string()),
                "every read-only file is named, missing {}: {message}",
                path.display()
            );
            assert_eq!(fs::read_to_string(path).expect("read"), "fn old() {}\n");
            set_read_only(path, false);
        }
    }

    /// Deleting destroys a read-only file as surely as rewriting it does,
    /// and `allow_file_deletion` is exactly the setting the documentation
    /// tells a user to turn on for a delete.
    #[tokio::test]
    async fn test_refuses_to_delete_a_read_only_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("protected.rs");
        fs::write(&path, "keep me\n").expect("seed");
        set_read_only(&path, true);

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
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
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
            .await
            .expect_err("a read-only file is not deleted either");
        assert!(
            error.to_string().contains("read-only"),
            "the error says why: {error}"
        );
        assert!(path.exists(), "the file survives");
        set_read_only(&path, false);
    }

    /// rust-analyzer's "create module" quick fix asks for a file in a
    /// directory that does not exist yet. mcpls does not create directories,
    /// and the refusal must say so rather than surface as a file-I/O failure
    /// naming a directory the caller never asked about.
    #[tokio::test]
    async fn test_refuses_a_create_into_a_directory_that_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("submodule");
        let path = missing.join("mod.rs");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri_for(&path),
                    options: None,
                    annotation_id: None,
                })),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let error = applier
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
            .await
            .expect_err("mcpls does not create directories");
        assert!(
            error.to_string().contains("does not create directories"),
            "the error says what is unsupported: {error}"
        );
        assert!(!missing.exists());
    }

    /// gopls emits a package move as a directory rename followed by edits to
    /// files under the new directory. The overlay is keyed on the directory,
    /// not its children, so this shape cannot be planned; refusing it names
    /// the shape rather than failing later on a path the caller never saw.
    #[tokio::test]
    async fn test_refuses_an_edit_under_a_directory_the_same_edit_moves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package = dir.path().join("pkg");
        fs::create_dir(&package).expect("create the package directory");
        fs::write(package.join("thing.go"), "package pkg\n").expect("seed");
        let moved = dir.path().join("renamed");

        let applier = Applier::new(vec![dir.path().to_path_buf()], permissive());
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                    old_uri: uri_for(&package),
                    new_uri: uri_for(&moved),
                    options: None,
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: uri_for(&moved.join("thing.go")),
                        version: None,
                    },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range::new(Position::new(0, 8), Position::new(0, 11)),
                        new_text: "renamed".to_string(),
                    })],
                }),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let error = applier
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
            .await
            .expect_err("an edit under a moved directory is unsupported");
        assert!(
            error.to_string().contains("the same edit relocates"),
            "the error names the unsupported shape: {error}"
        );
        assert!(package.is_dir(), "nothing was moved");
        assert!(!moved.exists());
    }

    /// A create with `overwrite` truncates an existing file to empty, which
    /// destroys its content as completely as a delete does.
    #[tokio::test]
    async fn test_refuses_an_overwriting_create_when_deletion_is_forbidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("existing.rs");
        fs::write(&path, "keep me\n").expect("seed");

        let config = ApplyConfig {
            allow_file_deletion: false,
            ..permissive()
        };
        let applier = Applier::new(vec![dir.path().to_path_buf()], config);
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri_for(&path),
                    options: Some(lsp_types::CreateFileOptions {
                        overwrite: Some(true),
                        ignore_if_exists: None,
                    }),
                    annotation_id: None,
                })),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let error = applier
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
            .await
            .expect_err("an overwriting create is a destruction");
        assert!(
            error.to_string().contains("apply.allow_file_deletion"),
            "the error names the key that would permit it: {error}"
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), "keep me\n");
    }

    /// A rename with `overwrite` trashes the destination, and a successful
    /// run purges the trash, so the destination's content is gone for good.
    #[tokio::test]
    async fn test_refuses_an_overwriting_rename_when_deletion_is_forbidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old.rs");
        let victim = dir.path().join("victim.rs");
        fs::write(&old, "source\n").expect("seed source");
        fs::write(&victim, "keep me\n").expect("seed destination");

        let config = ApplyConfig {
            allow_file_deletion: false,
            ..permissive()
        };
        let applier = Applier::new(vec![dir.path().to_path_buf()], config);
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                    old_uri: uri_for(&old),
                    new_uri: uri_for(&victim),
                    options: Some(lsp_types::RenameFileOptions {
                        overwrite: Some(true),
                        ignore_if_exists: None,
                    }),
                    annotation_id: None,
                })),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        let error = applier
            .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
            .await
            .expect_err("an overwriting rename is a destruction");
        assert!(
            error.to_string().contains("apply.allow_file_deletion"),
            "the error names the key that would permit it: {error}"
        );
        assert_eq!(fs::read_to_string(&victim).expect("read"), "keep me\n");
        assert!(old.exists(), "the source is untouched too");
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
        assert!(
            applier
                .apply(plan, PositionEncoding::Utf16, &InvalidationQueue::default())
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(&good).expect("read"),
            "fn a() {}\n",
            "planning fails before any step runs"
        );
        assert!(!missing.exists());
    }
}
