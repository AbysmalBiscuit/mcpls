# LSP Edit Application Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `rename_symbol`, `format_document`, and code actions write their edits to the working tree when config permits, defaulting to read-only.

**Architecture:** A new `bridge/apply/` module turns an `lsp_types::WorkspaceEdit` into an ordered operation list, resolves LSP positions to byte offsets with the existing encoding converter, and writes each file through a temp-file rename under a process-wide mutex. Tools gain an `apply` parameter gated by a new `[apply]` config table. Servers that deliver assists as commands answer through an inbound `workspace/applyEdit`, handled by a spawned task so the client message loop never parks against its own command channel.

**Tech Stack:** Rust 2024, tokio, `lsp_types` 0.97, rmcp 3.1, serde/toml, rstest, tempfile.

**Spec:** `docs/superpowers/specs/2026-09-05-lsp-apply-and-diagnostics-hooks-design.md` (Part 1, plus the "Multiple agents" section)

**Not in this plan:** Parts 2 and 3 of the spec, file watching and diagnostics injection, ship as a separate plan. Nothing here depends on them.

## Global Constraints

- Rust edition 2024, MSRV 1.88. Do not raise either.
- `unsafe_code = "deny"` at the workspace level. No `unsafe` blocks.
- `missing_docs = "warn"`: every public item gets a doc comment.
- clippy `pedantic` and `nursery` at warn, `unwrap_used` and `expect_used` at warn. Production code returns `Result`. Test code that needs `expect` carries `#[allow(clippy::expect_used)]` on the test function, matching `crates/mcpls-core/tests/integration/basic_tests.rs`.
- `ServerConfig` carries `#[serde(deny_unknown_fields)]`. Every new config struct does too.
- Default behavior is unchanged: with no `[apply]` table present, every tool stays read-only.
- Run `cargo clippy --workspace --all-targets` before each commit. Warnings introduced by your change block the commit.
- Commit messages follow Conventional Commits, imperative mood, subject at most 50 characters.

---

### Task 1: The `[apply]` config table

**Files:**
- Modify: `crates/mcpls-core/src/config/mod.rs`
- Test: `crates/mcpls-core/src/config/mod.rs` (the existing `#[cfg(test)] mod tests` at line 882)

**Interfaces:**
- Produces: `ApplyConfig { rename: bool, format_document: bool, code_actions: bool, allow_file_deletion: bool }`, `ServerConfig::apply: ApplyConfig`, and `ApplyConfig::permits(ToolKind) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/mcpls-core/src/config/mod.rs`:

```rust
#[test]
#[allow(clippy::expect_used)]
fn test_apply_defaults_to_read_only() {
    let config: ServerConfig = toml::from_str("").expect("empty config parses");
    assert!(!config.apply.rename);
    assert!(!config.apply.format_document);
    assert!(!config.apply.code_actions);
    assert!(!config.apply.allow_file_deletion);
}

#[test]
#[allow(clippy::expect_used)]
fn test_apply_permits_only_enabled_tools() {
    let config: ServerConfig = toml::from_str(
        "[apply]\nrename = true\n",
    )
    .expect("config parses");
    assert!(config.apply.permits(ToolKind::Rename));
    assert!(!config.apply.permits(ToolKind::FormatDocument));
    assert!(!config.apply.permits(ToolKind::CodeActions));
    assert!(!config.apply.permits(ToolKind::Hover));
}

#[test]
fn test_apply_rejects_unknown_key() {
    let result: std::result::Result<ServerConfig, _> =
        toml::from_str("[apply]\nrenmae = true\n");
    assert!(result.is_err(), "typo in an apply key must fail to parse");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib config::tests::test_apply`
Expected: FAIL, `no field 'apply' on type 'ServerConfig'`.

- [ ] **Step 3: Write the implementation**

Add to `crates/mcpls-core/src/config/mod.rs`, next to the other config structs:

```rust
/// Which tools may write their edits to the working tree.
///
/// Every field defaults to `false`, so a configuration without an
/// `[apply]` table leaves mcpls entirely read-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyConfig {
    /// `rename_symbol` may apply its `WorkspaceEdit`.
    #[serde(default)]
    pub rename: bool,

    /// `format_document` may apply its edits.
    #[serde(default)]
    pub format_document: bool,

    /// `apply_code_action` may apply a resolved action.
    #[serde(default)]
    pub code_actions: bool,

    /// Delete operations inside an otherwise permitted `WorkspaceEdit` are
    /// honored. Gates deletion for every tool above rather than any one of
    /// them, because a delete destroys content no other operation does.
    #[serde(default)]
    pub allow_file_deletion: bool,
}

impl ApplyConfig {
    /// Whether `tool` may write. Tools with nothing to write are always
    /// `false`, so a caller need not know which of the fifteen `ToolKind`
    /// variants can mutate anything.
    #[must_use]
    pub const fn permits(&self, tool: ToolKind) -> bool {
        match tool {
            ToolKind::Rename => self.rename,
            ToolKind::FormatDocument => self.format_document,
            ToolKind::CodeActions => self.code_actions,
            _ => false,
        }
    }
}
```

Add the field to `ServerConfig`, after `lsp_servers`:

```rust
    /// Which tools may write their edits to the working tree.
    #[serde(default)]
    pub apply: ApplyConfig,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib config::tests::test_apply`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/mcpls-core/src/config/mod.rs
git commit -m "feat(config): add apply permission table"
```

---

### Task 2: Apply error variants

**Files:**
- Modify: `crates/mcpls-core/src/error.rs`
- Test: `crates/mcpls-core/src/error.rs` (add a `#[cfg(test)] mod tests` if none exists, otherwise extend it)

**Interfaces:**
- Consumes: nothing.
- Produces: `Error::ApplyDisabled { tool: &'static str, config_key: &'static str }`, `Error::ApplyRefused(String)`, `Error::ApplyPartiallyFailed { written: Vec<PathBuf>, restored: Vec<PathBuf>, failed: Vec<PathBuf>, reason: String }`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_disabled_names_the_config_key() {
        let err = Error::ApplyDisabled {
            tool: "rename_symbol",
            config_key: "apply.rename",
        };
        let message = err.to_string();
        assert!(message.contains("rename_symbol"), "names the tool: {message}");
        assert!(message.contains("apply.rename"), "names the key: {message}");
    }

    #[test]
    fn test_apply_partially_failed_lists_each_file_group() {
        let err = Error::ApplyPartiallyFailed {
            written: vec![PathBuf::from("/w/a.rs")],
            restored: vec![PathBuf::from("/w/b.rs")],
            failed: vec![PathBuf::from("/w/c.rs")],
            reason: "permission denied".to_string(),
        };
        let message = err.to_string();
        for expected in ["/w/a.rs", "/w/b.rs", "/w/c.rs", "permission denied"] {
            assert!(message.contains(expected), "missing {expected} in: {message}");
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mcpls-core --lib error::tests`
Expected: FAIL, `no variant named 'ApplyDisabled'`.

- [ ] **Step 3: Write the implementation**

Add to the `Error` enum in `crates/mcpls-core/src/error.rs`:

```rust
    /// A tool was called with `apply: true` while its config key is `false`.
    #[error(
        "{tool} cannot write: set `{config_key} = true` in mcpls.toml to allow it"
    )]
    ApplyDisabled {
        /// MCP tool name the caller used.
        tool: &'static str,
        /// Dotted config key that would permit the write.
        config_key: &'static str,
    },

    /// The edit was rejected before anything was written.
    #[error("edit refused: {0}")]
    ApplyRefused(String),

    /// A write failed partway through and rollback did not fully restore
    /// the tree. Names every file in each state so the caller can recover.
    #[error(
        "apply failed partway: {reason}. written and kept: {written:?}; \
         restored to original: {restored:?}; left in an unknown state: {failed:?}"
    )]
    ApplyPartiallyFailed {
        /// Files whose new content is on disk.
        written: Vec<PathBuf>,
        /// Files rolled back to their original content.
        restored: Vec<PathBuf>,
        /// Files whose rollback itself failed.
        failed: Vec<PathBuf>,
        /// Why the original write failed.
        reason: String,
    },
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mcpls-core --lib error::tests`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/mcpls-core/src/error.rs
git commit -m "feat(error): add apply failure variants"
```

---

### Task 3: Document line table and position resolution

**Files:**
- Create: `crates/mcpls-core/src/bridge/apply/mod.rs`
- Create: `crates/mcpls-core/src/bridge/apply/offsets.rs`
- Modify: `crates/mcpls-core/src/bridge/mod.rs`
- Test: `crates/mcpls-core/src/bridge/apply/offsets.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::bridge::encoding::{EncodingConverter, PositionEncoding}`, whose `character_to_byte_offset(&self, text: &str, character_offset: u32) -> Result<usize, String>` resolves a column within one line.
- Produces: `LineTable::new(text: &str) -> Self`, `LineTable::byte_offset(&self, position: lsp_types::Position, converter: &EncodingConverter) -> Result<usize>`, `LineTable::byte_range(&self, range: lsp_types::Range, converter: &EncodingConverter) -> Result<std::ops::Range<usize>>`.

Build the table by scanning for `\n` rather than using `str::lines()`, which drops a trailing empty line and strips `\r`. Both matter: LSP position `(1, 0)` is valid in `"a\n"`, and a CRLF file's column offsets count the `\r`.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcpls-core/src/bridge/apply/offsets.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use lsp_types::{Position, Range};

    use super::LineTable;
    use crate::bridge::encoding::{EncodingConverter, PositionEncoding};

    fn utf16() -> EncodingConverter {
        EncodingConverter::new(PositionEncoding::Utf16)
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_resolves_ascii_position() {
        let table = LineTable::new("fn main() {}\nlet x = 1;\n");
        let offset = table
            .byte_offset(Position::new(1, 4), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 17, "line 1 starts at 13, plus 4 columns");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_counts_utf16_units_not_bytes() {
        // "héllo" is 6 bytes and 5 UTF-16 units; column 3 sits after "hél".
        let table = LineTable::new("héllo\n");
        let offset = table
            .byte_offset(Position::new(0, 3), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 4, "h=1, é=2, l=1");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_crlf_line_keeps_carriage_return() {
        let table = LineTable::new("ab\r\ncd\r\n");
        let offset = table
            .byte_offset(Position::new(1, 2), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 6, "line 1 starts at 4 and \\r belongs to line 0");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_position_on_line_after_final_terminator() {
        let table = LineTable::new("a\n");
        let offset = table
            .byte_offset(Position::new(1, 0), &utf16())
            .expect("the empty final line is addressable");
        assert_eq!(offset, 2);
    }

    #[test]
    fn test_line_beyond_document_is_an_error() {
        let table = LineTable::new("a\n");
        assert!(table.byte_offset(Position::new(9, 0), &utf16()).is_err());
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_byte_range_spans_lines() {
        let table = LineTable::new("abc\ndef\n");
        let range = table
            .byte_range(
                Range::new(Position::new(0, 1), Position::new(1, 2)),
                &utf16(),
            )
            .expect("range resolves");
        assert_eq!(range, 1..6);
    }

    #[test]
    fn test_inverted_range_is_an_error() {
        let table = LineTable::new("abc\ndef\n");
        let range = Range::new(Position::new(1, 2), Position::new(0, 1));
        assert!(table.byte_range(range, &utf16()).is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib bridge::apply::offsets`
Expected: FAIL to compile, `unresolved import 'super::LineTable'`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/mcpls-core/src/bridge/apply/offsets.rs`:

```rust
//! Resolving LSP positions to byte offsets in a document.

use lsp_types::{Position, Range};

use crate::bridge::encoding::EncodingConverter;
use crate::error::{Error, Result};

/// Byte offsets of each line start in a document, so an LSP position can be
/// resolved without rescanning the text.
///
/// Lines are split on `\n` only. A `\r` stays part of the line it terminates,
/// matching how a server counts columns in a CRLF file, and a document ending
/// in a newline gets one final empty line, which LSP treats as addressable.
pub struct LineTable<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineTable<'a> {
    /// Index `text` by line start.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { text, line_starts }
    }

    /// The text of one line, without its terminating `\n`.
    fn line(&self, line: u32) -> Result<&'a str> {
        let index = line as usize;
        let start = *self.line_starts.get(index).ok_or_else(|| {
            Error::ApplyRefused(format!(
                "line {line} is beyond the document, which has {} lines",
                self.line_starts.len()
            ))
        })?;
        let end = self
            .line_starts
            .get(index + 1)
            .map_or(self.text.len(), |next| next - 1);
        Ok(&self.text[start..end])
    }

    /// Byte offset of `position`, with columns counted in `converter`'s
    /// encoding.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when the line is beyond the document
    /// or the column is beyond the line.
    pub fn byte_offset(
        &self,
        position: Position,
        converter: &EncodingConverter,
    ) -> Result<usize> {
        let line_text = self.line(position.line)?;
        let within_line = converter
            .character_to_byte_offset(line_text, position.character)
            .map_err(|e| {
                Error::ApplyRefused(format!(
                    "column {} on line {}: {e}",
                    position.character, position.line
                ))
            })?;
        Ok(self.line_starts[position.line as usize] + within_line)
    }

    /// Byte range of `range`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when either endpoint fails to resolve
    /// or the end precedes the start.
    pub fn byte_range(
        &self,
        range: Range,
        converter: &EncodingConverter,
    ) -> Result<std::ops::Range<usize>> {
        let start = self.byte_offset(range.start, converter)?;
        let end = self.byte_offset(range.end, converter)?;
        if end < start {
            return Err(Error::ApplyRefused(format!(
                "range end {end} precedes start {start}"
            )));
        }
        Ok(start..end)
    }
}
```

Create `crates/mcpls-core/src/bridge/apply/mod.rs`:

```rust
//! Writing LSP `WorkspaceEdit`s to the working tree.

pub mod offsets;

pub use offsets::LineTable;
```

Add to `crates/mcpls-core/src/bridge/mod.rs`, beside the other module declarations:

```rust
pub mod apply;
```

If `encoding` is private in `bridge/mod.rs`, make it `pub(crate)` so `apply` can reach `EncodingConverter`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::apply::offsets`
Expected: PASS, 7 tests.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/apply/ crates/mcpls-core/src/bridge/mod.rs
git commit -m "feat(apply): add line table for position resolution"
```

---

### Task 4: Normalize a WorkspaceEdit into an ordered operation list

**Files:**
- Create: `crates/mcpls-core/src/bridge/apply/plan.rs`
- Modify: `crates/mcpls-core/src/bridge/apply/mod.rs`
- Test: `crates/mcpls-core/src/bridge/apply/plan.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `EditPlan::from_workspace_edit(edit: lsp_types::WorkspaceEdit) -> Result<EditPlan>`, `EditPlan::operations(&self) -> &[Operation]`, and the enums:

```rust
pub enum Operation {
    Edit { uri: lsp_types::Uri, version: Option<i32>, edits: Vec<lsp_types::TextEdit> },
    Create { uri: lsp_types::Uri, overwrite: bool },
    Rename { old: lsp_types::Uri, new: lsp_types::Uri, overwrite: bool },
    Delete { uri: lsp_types::Uri, recursive: bool },
}
```

Three rules the tests pin down. Resource operations keep their array order, because a rename followed by an edit to the new path means something different from the reverse. Text edits within one file sort descending by start position, so applying them in order never invalidates a later range. Two edits whose ranges overlap in one file are an error rather than a merge.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcpls-core/src/bridge/apply/plan.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;

    use lsp_types::{
        CreateFile, DocumentChangeOperation, DocumentChanges, OneOf,
        OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp, TextDocumentEdit,
        TextEdit, Uri, WorkspaceEdit,
    };

    use super::{EditPlan, Operation};

    #[allow(clippy::expect_used)]
    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).expect("valid uri")
    }

    fn edit(start_line: u32, end_line: u32, text: &str) -> TextEdit {
        TextEdit {
            range: Range::new(Position::new(start_line, 0), Position::new(end_line, 0)),
            new_text: text.to_string(),
        }
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_changes_map_becomes_one_edit_operation_per_file() {
        let mut changes = HashMap::new();
        changes.insert(uri("/w/a.rs"), vec![edit(0, 0, "x")]);
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");
        assert_eq!(plan.operations().len(), 1);
        assert!(matches!(plan.operations()[0], Operation::Edit { .. }));
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_edits_within_a_file_sort_bottom_up() {
        let mut changes = HashMap::new();
        changes.insert(uri("/w/a.rs"), vec![edit(1, 1, "first"), edit(5, 5, "second")]);
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");
        let Operation::Edit { edits, .. } = &plan.operations()[0] else {
            panic!("expected an edit operation");
        };
        assert_eq!(edits[0].new_text, "second", "highest line applies first");
        assert_eq!(edits[1].new_text, "first");
    }

    #[test]
    fn test_overlapping_edits_are_refused() {
        let mut changes = HashMap::new();
        changes.insert(uri("/w/a.rs"), vec![edit(1, 4, "a"), edit(2, 6, "b")]);
        let result = EditPlan::from_workspace_edit(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        });
        assert!(result.is_err(), "overlapping ranges must not be merged");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_operations_keep_their_array_order() {
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri("/w/new.rs"),
                    options: None,
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: uri("/w/new.rs"),
                        version: Some(3),
                    },
                    edits: vec![OneOf::Left(edit(0, 0, "content"))],
                }),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");
        assert!(matches!(plan.operations()[0], Operation::Create { .. }));
        assert!(matches!(
            plan.operations()[1],
            Operation::Edit { version: Some(3), .. }
        ));
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_empty_edit_produces_no_operations() {
        let plan =
            EditPlan::from_workspace_edit(WorkspaceEdit::default()).expect("plan builds");
        assert!(plan.operations().is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib bridge::apply::plan`
Expected: FAIL to compile, `unresolved import 'super::EditPlan'`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/mcpls-core/src/bridge/apply/plan.rs`:

```rust
//! Turning a `WorkspaceEdit` into an ordered, validated operation list.

use lsp_types::{TextEdit, Uri, WorkspaceEdit};

use crate::error::{Error, Result};

/// One step of an edit, in the order it must be performed.
#[derive(Debug)]
pub enum Operation {
    /// Replace ranges within one document. `edits` is sorted so the
    /// highest position applies first.
    Edit {
        /// Document to edit.
        uri: Uri,
        /// Version the server computed the edit against, when it said.
        version: Option<i32>,
        /// Edits, ordered bottom-up.
        edits: Vec<TextEdit>,
    },
    /// Create a file.
    Create {
        /// Path to create.
        uri: Uri,
        /// Replace the file if it already exists.
        overwrite: bool,
    },
    /// Move a file.
    Rename {
        /// Current path.
        old: Uri,
        /// Path to move it to.
        new: Uri,
        /// Replace the destination if it already exists.
        overwrite: bool,
    },
    /// Remove a file or directory.
    Delete {
        /// Path to remove.
        uri: Uri,
        /// Remove directory contents as well.
        recursive: bool,
    },
}

/// A `WorkspaceEdit` normalized into operations that can be executed in order.
#[derive(Debug)]
pub struct EditPlan {
    operations: Vec<Operation>,
}

impl EditPlan {
    /// Normalize `edit`.
    ///
    /// Accepts all three shapes a server may send: the legacy `changes`
    /// map, `documentChanges` as plain edits, and `documentChanges` with
    /// resource operations interleaved. Resource operations keep their
    /// array order; text edits within a file are sorted bottom-up.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when two edits to the same document
    /// have overlapping ranges.
    pub fn from_workspace_edit(edit: WorkspaceEdit) -> Result<Self> {
        let mut operations = Vec::new();

        if let Some(changes) = edit.changes {
            for (uri, edits) in changes {
                operations.push(Self::edit_operation(uri, None, edits)?);
            }
        }

        match edit.document_changes {
            Some(lsp_types::DocumentChanges::Edits(edits)) => {
                for tde in edits {
                    operations.push(Self::text_document_edit(tde)?);
                }
            }
            Some(lsp_types::DocumentChanges::Operations(ops)) => {
                for op in ops {
                    operations.push(match op {
                        lsp_types::DocumentChangeOperation::Edit(tde) => {
                            Self::text_document_edit(tde)?
                        }
                        lsp_types::DocumentChangeOperation::Op(resource) => {
                            Self::resource_operation(resource)
                        }
                    });
                }
            }
            None => {}
        }

        Ok(Self { operations })
    }

    /// The normalized operations, in execution order.
    #[must_use]
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    fn text_document_edit(tde: lsp_types::TextDocumentEdit) -> Result<Operation> {
        let edits = tde
            .edits
            .into_iter()
            .map(|one_of| match one_of {
                lsp_types::OneOf::Left(te) => te,
                lsp_types::OneOf::Right(ate) => ate.text_edit,
            })
            .collect();
        Self::edit_operation(tde.text_document.uri, tde.text_document.version, edits)
    }

    fn edit_operation(
        uri: Uri,
        version: Option<i32>,
        mut edits: Vec<TextEdit>,
    ) -> Result<Operation> {
        edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
        for pair in edits.windows(2) {
            // Sorted descending, so pair[1] starts no later than pair[0].
            if pair[1].range.end > pair[0].range.start {
                return Err(Error::ApplyRefused(format!(
                    "overlapping edits in {}: {:?} and {:?}",
                    uri.as_str(),
                    pair[1].range,
                    pair[0].range
                )));
            }
        }
        Ok(Operation::Edit {
            uri,
            version,
            edits,
        })
    }

    fn resource_operation(resource: lsp_types::ResourceOp) -> Operation {
        match resource {
            lsp_types::ResourceOp::Create(create) => Operation::Create {
                uri: create.uri,
                overwrite: create
                    .options
                    .and_then(|o| o.overwrite)
                    .unwrap_or(false),
            },
            lsp_types::ResourceOp::Rename(rename) => Operation::Rename {
                old: rename.old_uri,
                new: rename.new_uri,
                overwrite: rename
                    .options
                    .and_then(|o| o.overwrite)
                    .unwrap_or(false),
            },
            lsp_types::ResourceOp::Delete(delete) => Operation::Delete {
                uri: delete.uri,
                recursive: delete
                    .options
                    .and_then(|o| o.recursive)
                    .unwrap_or(false),
            },
        }
    }
}
```

Add to `crates/mcpls-core/src/bridge/apply/mod.rs`:

```rust
pub mod plan;

pub use plan::{EditPlan, Operation};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::apply::plan`
Expected: PASS, 5 tests.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/apply/
git commit -m "feat(apply): normalize workspace edits into a plan"
```

---

### Task 5: Show resource operations in the preview

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/dto.rs:99-113` and `:210-213`
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs:225-231`
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs` (the existing `#[cfg(test)] mod tests` at line 429)

**Interfaces:**
- Consumes: `EditPlan` and `Operation` from Task 4.
- Produces: `ResourceOperation { kind: String, uri: String, new_uri: Option<String> }`, a `resource_operations: Vec<ResourceOperation>` field on both `RenameResult` and `WorkspaceEditDescription`, and `resource_operations_from_plan(&EditPlan) -> Vec<ResourceOperation>`.

Once Task 6 advertises `resource_operations`, rust-analyzer starts returning file renames for a module rename. `handle_rename` currently drops them (`edits.rs:226`, the `DocumentChangeOperation::Op(_) => None` arm), which would show a preview missing the rename that apply then performs. Preview and apply must describe the same thing.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/mcpls-core/src/bridge/translator/edits.rs`:

```rust
#[test]
#[allow(clippy::expect_used)]
fn test_preview_reports_file_rename_operations() {
    use std::str::FromStr;

    use lsp_types::{
        DocumentChangeOperation, DocumentChanges, RenameFile, ResourceOp, Uri, WorkspaceEdit,
    };

    use crate::bridge::apply::EditPlan;
    use crate::bridge::translator::dto::resource_operations_from_plan;

    let edit = WorkspaceEdit {
        document_changes: Some(DocumentChanges::Operations(vec![
            DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                old_uri: Uri::from_str("file:///w/foo.rs").expect("valid uri"),
                new_uri: Uri::from_str("file:///w/bar.rs").expect("valid uri"),
                options: None,
                annotation_id: None,
            })),
        ])),
        ..WorkspaceEdit::default()
    };
    let plan = EditPlan::from_workspace_edit(edit).expect("plan builds");
    let ops = resource_operations_from_plan(&plan);

    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].kind, "rename");
    assert_eq!(ops[0].uri, "file:///w/foo.rs");
    assert_eq!(ops[0].new_uri.as_deref(), Some("file:///w/bar.rs"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mcpls-core --lib test_preview_reports_file_rename_operations`
Expected: FAIL to compile, `unresolved import 'resource_operations_from_plan'`.

- [ ] **Step 3: Write the implementation**

Add to `crates/mcpls-core/src/bridge/translator/dto.rs`:

```rust
/// A file-system change carried by a `WorkspaceEdit`, described for a
/// caller previewing the edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceOperation {
    /// One of `create`, `rename`, or `delete`.
    pub kind: String,
    /// Path the operation acts on. For a rename, the source.
    pub uri: String,
    /// Destination of a rename. `None` for create and delete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_uri: Option<String>,
}

/// Describe the file-system operations in `plan` for a preview response.
#[must_use]
pub fn resource_operations_from_plan(
    plan: &crate::bridge::apply::EditPlan,
) -> Vec<ResourceOperation> {
    use crate::bridge::apply::Operation;

    plan.operations()
        .iter()
        .filter_map(|op| match op {
            Operation::Edit { .. } => None,
            Operation::Create { uri, .. } => Some(ResourceOperation {
                kind: "create".to_string(),
                uri: uri.to_string(),
                new_uri: None,
            }),
            Operation::Rename { old, new, .. } => Some(ResourceOperation {
                kind: "rename".to_string(),
                uri: old.to_string(),
                new_uri: Some(new.to_string()),
            }),
            Operation::Delete { uri, .. } => Some(ResourceOperation {
                kind: "delete".to_string(),
                uri: uri.to_string(),
                new_uri: None,
            }),
        })
        .collect()
}
```

Add the field to `RenameResult` and to `WorkspaceEditDescription`, in both cases after their existing `changes` field:

```rust
    /// File-system operations the edit performs alongside its text changes.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub resource_operations: Vec<ResourceOperation>,
```

In `handle_rename` (`edits.rs`), stop discarding resource operations. Replace the body that builds `changes` with a pass through `EditPlan`, so preview and apply read the same normalization:

```rust
        let (changes, resource_operations) = if let Some(edit) = response {
            let plan = EditPlan::from_workspace_edit(edit)?;
            let resource_operations = resource_operations_from_plan(&plan);
            let mut result_changes = Vec::new();
            for operation in plan.operations() {
                if let Operation::Edit { uri, edits, .. } = operation {
                    let mut text_edits = Vec::with_capacity(edits.len());
                    for e in edits {
                        text_edits.push(TextEdit {
                            range: ctx.normalize_range(uri, e.range).await,
                            new_text: e.new_text.clone(),
                        });
                    }
                    result_changes.push(DocumentChanges {
                        uri: uri.to_string(),
                        edits: text_edits,
                    });
                }
            }
            (result_changes, resource_operations)
        } else {
            (vec![], vec![])
        };

        Ok(RenameResult {
            changes,
            resource_operations,
        })
```

Add the imports `use crate::bridge::apply::{EditPlan, Operation};` and extend the `dto` import with `resource_operations_from_plan, ResourceOperation`.

Every other construction site of `RenameResult` and `WorkspaceEditDescription` needs the new field. Find them with:

```bash
rg -n "RenameResult \{|WorkspaceEditDescription \{" crates/mcpls-core
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::translator::edits`
Expected: PASS, including the existing tests in that module.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/translator/
git commit -m "feat(preview): report file operations in rename preview"
```

---

### Task 6: Advertise the client capabilities apply needs

**Files:**
- Modify: `crates/mcpls-core/src/lsp/lifecycle.rs:460-464`
- Test: `crates/mcpls-core/src/lsp/lifecycle.rs` (inline `#[cfg(test)] mod tests`, extend if present)

**Interfaces:**
- Consumes: nothing.
- Produces: no new API. The `initialize` handshake now declares `workspace.applyEdit`, `workspace.workspaceEdit.documentChanges`, `workspace.workspaceEdit.resourceOperations`, and `textDocument.synchronization.didSave`.

A server gates behavior on these. rust-analyzer will not emit file renames without `resourceOperations`, and nothing currently tells a server that mcpls sends `didSave`, which the applier's resync does.

Do not add `workspace.didChangeWatchedFiles.dynamicRegistration` here. That belongs to the file-watching plan and, advertised without its notification half, makes gopls and tsgo strictly worse.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_client_capabilities_declare_edit_application() {
    let caps = build_client_capabilities();
    let workspace = caps.workspace.expect("workspace capabilities are declared");

    assert_eq!(workspace.apply_edit, Some(true));

    let edit = workspace
        .workspace_edit
        .expect("workspaceEdit capabilities are declared");
    assert_eq!(edit.document_changes, Some(true));
    let ops = edit
        .resource_operations
        .expect("resource operations are declared");
    assert!(ops.contains(&lsp_types::ResourceOperationKind::Create));
    assert!(ops.contains(&lsp_types::ResourceOperationKind::Rename));
    assert!(ops.contains(&lsp_types::ResourceOperationKind::Delete));

    let sync = caps
        .text_document
        .expect("textDocument capabilities are declared")
        .synchronization
        .expect("synchronization capabilities are declared");
    assert_eq!(sync.did_save, Some(true));
}

#[test]
fn test_client_capabilities_do_not_claim_dynamic_file_watching() {
    let caps = build_client_capabilities();
    let declared = caps
        .workspace
        .and_then(|w| w.did_change_watched_files)
        .and_then(|w| w.dynamic_registration);
    assert_ne!(
        declared,
        Some(true),
        "advertising this without sending the notification blinds gopls and tsgo"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib lsp::lifecycle`
Expected: FAIL, either `build_client_capabilities` is not a function or `apply_edit` is `None`.

- [ ] **Step 3: Write the implementation**

The capabilities are currently built inline inside the `InitializeParams` literal in
`lsp/lifecycle.rs`, which is why nothing can assert them. Move the whole
`ClientCapabilities` value into a free function in the same module, adding the four new
declarations. `position_encodings` stays a parameter because it comes from config:

```rust
/// Client capabilities mcpls declares during `initialize`.
///
/// A free function rather than an inline literal so the declaration can be
/// asserted directly: several servers change behavior based on these, and a
/// silent regression is invisible until a tool quietly returns less than it
/// should.
fn build_client_capabilities(
    position_encodings: Vec<lsp_types::PositionEncodingKind>,
) -> ClientCapabilities {
    ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(position_encodings),
            ..Default::default()
        }),
        text_document: Some(lsp_types::TextDocumentClientCapabilities {
            synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(false),
                did_save: Some(true),
                will_save: Some(false),
                will_save_wait_until: Some(false),
            }),
            hover: Some(lsp_types::HoverClientCapabilities {
                dynamic_registration: Some(false),
                content_format: Some(vec![
                    lsp_types::MarkupKind::Markdown,
                    lsp_types::MarkupKind::PlainText,
                ]),
            }),
            definition: Some(lsp_types::GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(true),
            }),
            references: Some(lsp_types::ReferenceClientCapabilities {
                dynamic_registration: Some(false),
            }),
            code_action: Some(lsp_types::CodeActionClientCapabilities {
                dynamic_registration: Some(false),
                data_support: Some(true),
                resolve_support: Some(lsp_types::CodeActionCapabilityResolveSupport {
                    properties: vec!["edit".to_string()],
                }),
                // Declare supported action kinds so the server returns
                // CodeAction objects (not just legacy Command objects).
                code_action_literal_support: Some(lsp_types::CodeActionLiteralSupport {
                    code_action_kind: lsp_types::CodeActionKindLiteralSupport {
                        value_set: [
                            lsp_types::CodeActionKind::EMPTY,
                            lsp_types::CodeActionKind::QUICKFIX,
                            lsp_types::CodeActionKind::REFACTOR,
                            lsp_types::CodeActionKind::REFACTOR_EXTRACT,
                            lsp_types::CodeActionKind::REFACTOR_INLINE,
                            lsp_types::CodeActionKind::REFACTOR_REWRITE,
                            lsp_types::CodeActionKind::SOURCE,
                            lsp_types::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                        ]
                        .iter()
                        .map(|k| k.as_str().to_string())
                        .collect(),
                    },
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        workspace: Some(lsp_types::WorkspaceClientCapabilities {
            workspace_folders: Some(true),
            apply_edit: Some(true),
            workspace_edit: Some(lsp_types::WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                resource_operations: Some(vec![
                    lsp_types::ResourceOperationKind::Create,
                    lsp_types::ResourceOperationKind::Rename,
                    lsp_types::ResourceOperationKind::Delete,
                ]),
                failure_handling: Some(lsp_types::FailureHandlingKind::Abort),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}
```

`failure_handling: Abort` matches what the applier actually does: it validates the whole
plan before writing anything, so a refused edit leaves the tree untouched.

In `InitializeParams`, replace the whole `capabilities: ClientCapabilities { ... }` literal
with:

```rust
            capabilities: build_client_capabilities(resolve_position_encodings(
                &config.position_encodings,
            )),
```

The two tests call `build_client_capabilities(vec![])`, since the encodings are irrelevant
to what they assert.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib lsp::lifecycle`
Expected: PASS.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/lsp/lifecycle.rs
git commit -m "feat(lsp): declare edit application capabilities"
```

---

### Task 7: The writer

**Files:**
- Create: `crates/mcpls-core/src/bridge/apply/writer.rs`
- Modify: `crates/mcpls-core/src/bridge/apply/mod.rs`
- Test: `crates/mcpls-core/src/bridge/apply/writer.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Error::ApplyPartiallyFailed` (Task 2).
- Produces: `StagedWrite { path: PathBuf, original: Option<String>, new_content: String }` and `commit_writes(writes: Vec<StagedWrite>) -> Result<Vec<PathBuf>>`.

Phase two of the applier, kept separate from planning so it can be tested without an LSP server. Each file is written to a temp file in the same directory and renamed over the target, which makes a single file's replacement atomic. The temp file copies the target's permissions, because a rename replaces the inode and an executable script would otherwise lose its mode bit. The temp name is dot-prefixed with a `.mcpls-tmp` suffix so it matches no source-file glob and no language server picks it up as a new source file.

Rollback restores originals in reverse order. If a restore itself fails, the error names which files are in which state rather than claiming a clean failure.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{commit_writes, StagedWrite};

    #[test]
    #[allow(clippy::expect_used)]
    fn test_writes_new_content_to_each_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, "old a").expect("seed a");
        fs::write(&b, "old b").expect("seed b");

        let written = commit_writes(vec![
            StagedWrite {
                path: a.clone(),
                original: Some("old a".to_string()),
                new_content: "new a".to_string(),
            },
            StagedWrite {
                path: b.clone(),
                original: Some("old b".to_string()),
                new_content: "new b".to_string(),
            },
        ])
        .expect("writes commit");

        assert_eq!(written.len(), 2);
        assert_eq!(fs::read_to_string(&a).expect("read a"), "new a");
        assert_eq!(fs::read_to_string(&b).expect("read b"), "new b");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_creates_a_file_that_did_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fresh.txt");

        commit_writes(vec![StagedWrite {
            path: path.clone(),
            original: None,
            new_content: "hello".to_string(),
        }])
        .expect("writes commit");

        assert_eq!(fs::read_to_string(&path).expect("read"), "hello");
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::expect_used)]
    fn test_preserves_mode_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("script.sh");
        fs::write(&path, "#!/bin/sh\n").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

        commit_writes(vec![StagedWrite {
            path: path.clone(),
            original: Some("#!/bin/sh\n".to_string()),
            new_content: "#!/bin/sh\necho hi\n".to_string(),
        }])
        .expect("writes commit");

        let mode = fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "executable bit survives the rename");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_rolls_back_earlier_writes_when_a_later_one_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.txt");
        fs::write(&good, "original").expect("seed");
        // A path whose parent does not exist cannot be written.
        let bad: PathBuf = dir.path().join("missing-dir").join("bad.txt");

        let result = commit_writes(vec![
            StagedWrite {
                path: good.clone(),
                original: Some("original".to_string()),
                new_content: "changed".to_string(),
            },
            StagedWrite {
                path: bad,
                original: None,
                new_content: "never lands".to_string(),
            },
        ]);

        assert!(result.is_err(), "the batch fails");
        assert_eq!(
            fs::read_to_string(&good).expect("read"),
            "original",
            "the earlier write is rolled back"
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn test_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        fs::write(&path, "old").expect("seed");

        commit_writes(vec![StagedWrite {
            path,
            original: Some("old".to_string()),
            new_content: "new".to_string(),
        }])
        .expect("writes commit");

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("mcpls-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files are cleaned up");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib bridge::apply::writer`
Expected: FAIL to compile, `unresolved import 'super::commit_writes'`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/mcpls-core/src/bridge/apply/writer.rs`:

```rust
//! Committing computed file contents to disk.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// One file's content, computed and validated, waiting to be written.
#[derive(Debug)]
pub struct StagedWrite {
    /// Absolute path to write.
    pub path: PathBuf,
    /// Content before the edit, or `None` when the file is being created.
    /// Held so a later failure can restore it.
    pub original: Option<String>,
    /// Content to write.
    pub new_content: String,
}

/// Write every staged file, rolling back on the first failure.
///
/// Each file is written to a temp file beside it and renamed over the
/// target, so a single file never observes a partial write. The temp file
/// carries the target's permissions, because the rename replaces the inode.
///
/// # Errors
///
/// Returns [`Error::ApplyPartiallyFailed`] when a write fails. Files
/// written before the failure are restored; the error names any whose
/// restore also failed.
pub fn commit_writes(writes: Vec<StagedWrite>) -> Result<Vec<PathBuf>> {
    let mut committed: Vec<&StagedWrite> = Vec::with_capacity(writes.len());

    for write in &writes {
        match write_one(&write.path, &write.new_content) {
            Ok(()) => committed.push(write),
            Err(reason) => return Err(roll_back(&committed, reason)),
        }
    }

    Ok(writes.iter().map(|w| w.path.clone()).collect())
}

fn write_one(path: &Path, content: &str) -> std::result::Result<(), String> {
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

    fs::rename(&temp, path).map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("renaming onto {}: {e}", path.display())
    })
}

fn roll_back(committed: &[&StagedWrite], reason: String) -> Error {
    let mut restored = Vec::new();
    let mut failed = Vec::new();

    for write in committed.iter().rev() {
        let outcome = match &write.original {
            Some(original) => write_one(&write.path, original),
            None => fs::remove_file(&write.path)
                .map_err(|e| format!("removing {}: {e}", write.path.display())),
        };
        match outcome {
            Ok(()) => restored.push(write.path.clone()),
            Err(_) => failed.push(write.path.clone()),
        }
    }

    Error::ApplyPartiallyFailed {
        written: Vec::new(),
        restored,
        failed,
        reason,
    }
}
```

Add to `crates/mcpls-core/src/bridge/apply/mod.rs`:

```rust
pub mod writer;

pub use writer::{commit_writes, StagedWrite};
```

Add `tempfile` to `[dev-dependencies]` in `crates/mcpls-core/Cargo.toml` if it is not already there. It is, so no change should be needed; confirm with `rg -n tempfile crates/mcpls-core/Cargo.toml`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::apply::writer`
Expected: PASS, 5 tests on Unix and 4 on Windows.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/apply/
git commit -m "feat(apply): commit staged writes atomically"
```

---

### Task 8: The applier entry point

**Files:**
- Modify: `crates/mcpls-core/src/bridge/apply/mod.rs`
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs`
- Test: `crates/mcpls-core/src/bridge/apply/mod.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `LineTable` (Task 3), `EditPlan` and `Operation` (Task 4), `commit_writes` and `StagedWrite` (Task 7), `ApplyConfig` (Task 1), `validate_path_against_roots` from `crate::bridge::translator::routing`.
- Produces:

```rust
pub struct ApplySummary {
    pub files_changed: Vec<FileChange>,
    pub resource_operations: Vec<crate::bridge::translator::dto::ResourceOperation>,
}
pub struct FileChange { pub path: PathBuf, pub edits: usize }
pub struct Applier { /* roots, config, encoding */ }
impl Applier {
    pub fn new(roots: Vec<PathBuf>, config: ApplyConfig, encoding: PositionEncoding) -> Self;
    pub async fn apply(&self, plan: EditPlan) -> Result<ApplySummary>;
}
```

`Translator` gains `apply_lock: Arc<tokio::sync::Mutex<()>>` and `applier: Option<Arc<Applier>>`. Every apply-enabled call holds that lock for its whole duration, which serializes two concurrent applies to the same file and, in Task 12, defines the window when an inbound `workspace/applyEdit` is honored.

Phase one resolves each `Operation::Edit` against current disk content and produces `StagedWrite`s. Phase two calls `commit_writes`. Nothing is written until every operation validates.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::str::FromStr;

    use lsp_types::{Position, Range, TextEdit, Uri, WorkspaceEdit};

    use super::{Applier, EditPlan};
    use crate::bridge::encoding::PositionEncoding;
    use crate::config::ApplyConfig;

    #[allow(clippy::expect_used)]
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

    fn permissive() -> ApplyConfig {
        ApplyConfig {
            rename: true,
            format_document: true,
            code_actions: true,
            allow_file_deletion: true,
        }
    }

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn test_applies_a_text_edit_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rs");
        fs::write(&path, "fn old() {}\n").expect("seed");

        let applier = Applier::new(
            vec![dir.path().to_path_buf()],
            permissive(),
            PositionEncoding::Utf16,
        );
        let uri = Uri::from_str(&format!("file://{}", path.display())).expect("uri");
        let plan = plan_replacing(
            uri,
            Range::new(Position::new(0, 3), Position::new(0, 6)),
            "new",
        );

        let summary = applier.apply(plan).await.expect("apply succeeds");

        assert_eq!(summary.files_changed.len(), 1);
        assert_eq!(summary.files_changed[0].edits, 1);
        assert_eq!(fs::read_to_string(&path).expect("read"), "fn new() {}\n");
    }

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn test_refuses_a_path_outside_every_root() {
        let inside = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let path = outside.path().join("escape.rs");
        fs::write(&path, "x\n").expect("seed");

        let applier = Applier::new(
            vec![inside.path().to_path_buf()],
            permissive(),
            PositionEncoding::Utf16,
        );
        let uri = Uri::from_str(&format!("file://{}", path.display())).expect("uri");
        let plan = plan_replacing(
            uri,
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            "y",
        );

        assert!(applier.apply(plan).await.is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "x\n",
            "the file outside the workspace is untouched"
        );
    }

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn test_refuses_deletion_when_the_config_forbids_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("doomed.rs");
        fs::write(&path, "x\n").expect("seed");

        let config = ApplyConfig {
            allow_file_deletion: false,
            ..permissive()
        };
        let applier =
            Applier::new(vec![dir.path().to_path_buf()], config, PositionEncoding::Utf16);
        let uri = Uri::from_str(&format!("file://{}", path.display())).expect("uri");
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(lsp_types::DocumentChanges::Operations(vec![
                lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Delete(
                    lsp_types::DeleteFile {
                        uri,
                        options: None,
                        annotation_id: None,
                    },
                )),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        assert!(applier.apply(plan).await.is_err());
        assert!(path.exists(), "the file survives a refused deletion");
    }

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn test_nothing_is_written_when_one_operation_is_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.rs");
        let missing = dir.path().join("missing.rs");
        fs::write(&good, "fn a() {}\n").expect("seed");

        let applier = Applier::new(
            vec![dir.path().to_path_buf()],
            permissive(),
            PositionEncoding::Utf16,
        );
        let mut changes = HashMap::new();
        changes.insert(
            Uri::from_str(&format!("file://{}", good.display())).expect("uri"),
            vec![TextEdit {
                range: Range::new(Position::new(0, 3), Position::new(0, 4)),
                new_text: "b".to_string(),
            }],
        );
        changes.insert(
            Uri::from_str(&format!("file://{}", missing.display())).expect("uri"),
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

        assert!(applier.apply(plan).await.is_err());
        assert_eq!(
            fs::read_to_string(&good).expect("read"),
            "fn a() {}\n",
            "validation runs before any write"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib bridge::apply::tests`
Expected: FAIL to compile, `unresolved import 'super::Applier'`.

- [ ] **Step 3: Write the implementation**

Add to `crates/mcpls-core/src/bridge/apply/mod.rs`:

```rust
//! Writing LSP `WorkspaceEdit`s to the working tree.

use std::fs;
use std::path::PathBuf;

pub mod offsets;
pub mod plan;
pub mod writer;

pub use offsets::LineTable;
pub use plan::{EditPlan, Operation};
pub use writer::{commit_writes, StagedWrite};

use crate::bridge::encoding::{EncodingConverter, PositionEncoding};
use crate::bridge::translator::dto::{resource_operations_from_plan, ResourceOperation};
use crate::bridge::translator::routing::validate_path_against_roots;
use crate::config::ApplyConfig;
use crate::error::{Error, Result};

/// One file the applier changed.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Absolute path.
    pub path: PathBuf,
    /// Number of text edits applied to it.
    pub edits: usize,
}

/// What an apply did, returned to the caller so it knows which of its
/// cached file contents are now stale.
#[derive(Debug, Clone)]
pub struct ApplySummary {
    /// Files whose text changed.
    pub files_changed: Vec<FileChange>,
    /// File-system operations performed.
    pub resource_operations: Vec<ResourceOperation>,
}

/// Applies validated `WorkspaceEdit`s within a set of workspace roots.
pub struct Applier {
    roots: Vec<PathBuf>,
    config: ApplyConfig,
    encoding: PositionEncoding,
}

impl Applier {
    /// Build an applier confined to `roots`.
    #[must_use]
    pub const fn new(
        roots: Vec<PathBuf>,
        config: ApplyConfig,
        encoding: PositionEncoding,
    ) -> Self {
        Self {
            roots,
            config,
            encoding,
        }
    }

    /// Which tools this applier permits to write. Read by
    /// `Translator::applier_for` to gate a call before any LSP request.
    #[must_use]
    pub const fn config(&self) -> &ApplyConfig {
        &self.config
    }

    /// Validate every operation, then write.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when an operation targets a path
    /// outside the workspace, deletes a file without
    /// `apply.allow_file_deletion`, or resolves to an invalid range, and
    /// [`Error::ApplyPartiallyFailed`] when a write fails after another
    /// has already landed.
    pub async fn apply(&self, plan: EditPlan) -> Result<ApplySummary> {
        let resource_operations = resource_operations_from_plan(&plan);
        let converter = EncodingConverter::new(self.encoding);
        let mut staged: Vec<StagedWrite> = Vec::new();
        let mut files_changed = Vec::new();

        for operation in plan.operations() {
            match operation {
                Operation::Edit { uri, edits, .. } => {
                    let path = self.resolve_existing(uri)?;
                    let original = fs::read_to_string(&path).map_err(|e| Error::FileIo {
                        path: path.clone(),
                        source: e,
                    })?;
                    let mut content = original.clone();
                    // `edits` is sorted bottom-up, so each splice leaves
                    // every not-yet-applied range valid.
                    for edit in edits {
                        let table = LineTable::new(&content);
                        let range = table.byte_range(edit.range, &converter)?;
                        content.replace_range(range, &edit.new_text);
                    }
                    files_changed.push(FileChange {
                        path: path.clone(),
                        edits: edits.len(),
                    });
                    staged.push(StagedWrite {
                        path,
                        original: Some(original),
                        new_content: content,
                    });
                }
                Operation::Create { uri, overwrite } => {
                    let path = self.resolve_new(uri)?;
                    if path.exists() && !overwrite {
                        return Err(Error::ApplyRefused(format!(
                            "{} already exists and the edit did not ask to overwrite it",
                            path.display()
                        )));
                    }
                    staged.push(StagedWrite {
                        path,
                        original: None,
                        new_content: String::new(),
                    });
                }
                Operation::Rename { .. } | Operation::Delete { .. } => {
                    self.validate_resource_operation(operation)?;
                }
            }
        }

        commit_writes(staged)?;
        self.perform_resource_operations(plan.operations())?;

        Ok(ApplySummary {
            files_changed,
            resource_operations,
        })
    }

    /// Path of an existing target, confined to the workspace roots.
    fn resolve_existing(&self, uri: &lsp_types::Uri) -> Result<PathBuf> {
        let path = crate::util::uri_to_path(uri)?;
        validate_path_against_roots(&path, &self.roots)
    }

    /// Path of a target that does not exist yet. The parent must exist and
    /// must be inside a root; canonicalizing the path itself would fail.
    fn resolve_new(&self, uri: &lsp_types::Uri) -> Result<PathBuf> {
        let path = crate::util::uri_to_path(uri)?;
        let parent = path.parent().ok_or_else(|| {
            Error::ApplyRefused(format!("{} has no parent directory", path.display()))
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            Error::ApplyRefused(format!("{} has no file name", path.display()))
        })?;
        let canonical_parent = validate_path_against_roots(parent, &self.roots)?;
        Ok(canonical_parent.join(file_name))
    }

    fn validate_resource_operation(&self, operation: &Operation) -> Result<()> {
        match operation {
            Operation::Delete { uri, .. } => {
                if !self.config.allow_file_deletion {
                    return Err(Error::ApplyRefused(format!(
                        "{} would be deleted, but `apply.allow_file_deletion` is false",
                        uri.as_str()
                    )));
                }
                self.resolve_existing(uri)?;
                Ok(())
            }
            Operation::Rename { old, new, .. } => {
                self.resolve_existing(old)?;
                self.resolve_new(new)?;
                Ok(())
            }
            Operation::Create { .. } | Operation::Edit { .. } => Ok(()),
        }
    }

    fn perform_resource_operations(&self, operations: &[Operation]) -> Result<()> {
        for operation in operations {
            match operation {
                Operation::Rename { old, new, .. } => {
                    let from = self.resolve_existing(old)?;
                    let to = self.resolve_new(new)?;
                    fs::rename(&from, &to).map_err(|e| Error::FileIo {
                        path: to,
                        source: e,
                    })?;
                }
                Operation::Delete { uri, recursive } => {
                    let path = self.resolve_existing(uri)?;
                    let outcome = if *recursive && path.is_dir() {
                        fs::remove_dir_all(&path)
                    } else {
                        fs::remove_file(&path)
                    };
                    outcome.map_err(|e| Error::FileIo { path, source: e })?;
                }
                Operation::Create { .. } | Operation::Edit { .. } => {}
            }
        }
        Ok(())
    }
}
```

`validate_path_against_roots` lives in `bridge/translator/routing.rs`. If either that
function or the `routing` module is private, widen it to `pub(crate)` so `apply` can reach
it, rather than writing a second confinement check that could drift from the first.

If `crate::util` has no `uri_to_path`, use whatever the translator already uses to convert a `Uri` into a `PathBuf`. Find it with:

```bash
rg -n "fn .*uri.*path|fn .*path.*uri" crates/mcpls-core/src/util.rs
```

Add the two fields to `Translator` in `crates/mcpls-core/src/bridge/translator/mod.rs`, beside `notification_cache`:

```rust
    /// Serializes every apply-enabled tool call. Two concurrent applies to
    /// one file would otherwise both pass validation and the second would
    /// overwrite the first.
    apply_lock: Arc<tokio::sync::Mutex<()>>,

    /// `None` when no tool is permitted to write, which is the default.
    applier: Option<Arc<Applier>>,
```

with a `with_applier(mut self, applier: Arc<Applier>) -> Self` builder matching the existing `with_notification_cache` style, and `apply_lock: Arc::new(tokio::sync::Mutex::new(()))` plus `applier: None` in the constructor.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::apply`
Expected: PASS, all tests across the four apply modules.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/
git commit -m "feat(apply): add confined workspace edit applier"
```

---

### Task 9: Apply from `rename_symbol`

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs` (`handle_rename`)
- Modify: `crates/mcpls-core/src/mcp/tools.rs` (`RenameParams`)
- Modify: `crates/mcpls-core/src/mcp/server.rs:297-315` (the `rename_symbol` tool) and `:182-201` (the annotation default)
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs`

**Interfaces:**
- Consumes: `Applier` and `ApplySummary` (Task 8), `ApplyConfig::permits` (Task 1), `Error::ApplyDisabled` (Task 2).
- Produces: `handle_rename(&self, file_path: String, line: u32, character: u32, new_name: String, apply: bool) -> Result<RenameResult>`, and `RenameResult` gains `applied: bool` and `files_written: Vec<String>`.

`mcp/server.rs:182-201` currently stamps read-only annotations onto every tool and carries a doc comment asserting that every mcpls tool is a read-only query, backed by `test_tool_annotation_classifications_match_intent`. That guardrail fires now, by design. Give `rename_symbol` an explicit annotation with `read_only_hint: false` and `destructive_hint: true`, and update the doc comment so it describes the new rule: tools are read-only unless they declare otherwise.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
#[allow(clippy::expect_used)]
async fn test_rename_with_apply_is_refused_when_config_forbids_it() {
    let translator = Translator::new();
    let error = translator
        .handle_rename("/w/a.rs".to_string(), 1, 1, "new".to_string(), true)
        .await
        .expect_err("apply must be refused without an applier");
    let message = error.to_string();
    assert!(
        message.contains("apply.rename"),
        "the error names the config key that would permit it: {message}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mcpls-core --lib test_rename_with_apply_is_refused`
Expected: FAIL to compile, `handle_rename` takes 4 arguments.

- [ ] **Step 3: Write the implementation**

Add the parameter to `handle_rename` and, before any LSP work, the gate:

```rust
    pub async fn handle_rename(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
        apply: bool,
    ) -> Result<RenameResult> {
        validate_rename_params(&new_name)?;

        let applier = if apply {
            Some(self.applier_for(ToolKind::Rename, "rename_symbol", "apply.rename")?)
        } else {
            None
        };
```

Add the helper to `Translator` in `bridge/translator/mod.rs`:

```rust
    /// The applier, if `tool` is permitted to write.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyDisabled`] naming `config_key` when no applier
    /// is configured or the tool's key is `false`.
    fn applier_for(
        &self,
        tool: ToolKind,
        tool_name: &'static str,
        config_key: &'static str,
    ) -> Result<Arc<Applier>> {
        self.applier
            .as_ref()
            .filter(|applier| applier.config().permits(tool))
            .map(Arc::clone)
            .ok_or(Error::ApplyDisabled {
                tool: tool_name,
                config_key,
            })
    }
```

and a `config(&self) -> &ApplyConfig` accessor on `Applier`.

At the end of `handle_rename`, after the plan is built, apply when asked. Hold the lock across the whole apply:

```rust
        let (applied, files_written) = if let Some(applier) = applier {
            let _guard = self.apply_lock.lock().await;
            let summary = applier.apply(plan).await?;
            (
                true,
                summary
                    .files_changed
                    .iter()
                    .map(|c| c.path.display().to_string())
                    .collect(),
            )
        } else {
            (false, Vec::new())
        };
```

This requires keeping the `EditPlan` rather than consuming it while building `changes`; build `changes` from `plan.operations()` by reference, as Task 5 already does.

Add to `RenameResult` in `dto.rs`:

```rust
    /// Whether the edits were written to disk.
    #[serde(default)]
    pub applied: bool,
    /// Files written, when `applied`. The caller's cached contents for
    /// these paths are stale.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub files_written: Vec<String>,
```

Add to `RenameParams` in `mcp/tools.rs`:

```rust
    /// Write the edits to disk instead of only describing them. Requires
    /// `apply.rename = true` in mcpls.toml.
    #[schemars(
        description = "Write the edits to disk instead of only describing them. \
                       Requires apply.rename = true in mcpls.toml."
    )]
    #[serde(default)]
    pub apply: bool,
```

Update the `rename_symbol` tool in `mcp/server.rs` to destructure and forward `apply`, and give it an explicit annotation so the read-only default does not apply to it:

```rust
    #[tool(
        description = "Rename symbol across workspace. Returns text edits for all files \
                       where symbol is used. With apply=true and apply.rename enabled in \
                       config, writes those edits to disk.",
        title = "Rename Symbol",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false)
    )]
```

Update the `tool_router` doc comment at `mcp/server.rs:182-192` so it states the rule that now holds: every tool inherits read-only annotations unless it declares its own, and `rename_symbol` declares its own because it can write.

Update `test_tool_annotation_classifications_match_intent` to expect `rename_symbol` in the writing set.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core`
Expected: PASS. Fix any call site of `handle_rename` the compiler names by passing `false`.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/
git commit -m "feat(rename): apply rename edits when config allows"
```

---

### Task 10: Apply from `format_document`

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs` (`handle_format_document`)
- Modify: `crates/mcpls-core/src/mcp/tools.rs` (`FormatDocumentParams`)
- Modify: `crates/mcpls-core/src/mcp/server.rs` (the `format_document` tool)
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs`

**Interfaces:**
- Consumes: everything Task 9 consumes, plus `Translator::applier_for`.
- Produces: `handle_format_document(&self, file_path: String, tab_size: u32, insert_spaces: bool, apply: bool) -> Result<FormatDocumentResult>`, and `FormatDocumentResult` gains `applied: bool`.

A formatting response is a `Vec<TextEdit>` for one document rather than a `WorkspaceEdit`, so it is wrapped into one before reaching the applier. That keeps a single normalization and a single write path.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
#[allow(clippy::expect_used)]
async fn test_format_with_apply_is_refused_when_config_forbids_it() {
    let translator = Translator::new();
    let error = translator
        .handle_format_document("/w/a.rs".to_string(), 4, true, true)
        .await
        .expect_err("apply must be refused without an applier");
    assert!(
        error.to_string().contains("apply.format_document"),
        "the error names the config key"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mcpls-core --lib test_format_with_apply_is_refused`
Expected: FAIL to compile, `handle_format_document` takes 3 arguments.

- [ ] **Step 3: Write the implementation**

Gate at the top of `handle_format_document`, exactly as Task 9 does:

```rust
        let applier = if apply {
            Some(self.applier_for(
                ToolKind::FormatDocument,
                "format_document",
                "apply.format_document",
            )?)
        } else {
            None
        };
```

After the LSP response yields `Vec<lsp_types::TextEdit>`, wrap and apply:

```rust
        let applied = if let Some(applier) = applier {
            let mut changes = std::collections::HashMap::new();
            changes.insert(response_uri.clone(), lsp_edits.clone());
            let plan = EditPlan::from_workspace_edit(lsp_types::WorkspaceEdit {
                changes: Some(changes),
                ..lsp_types::WorkspaceEdit::default()
            })?;
            let _guard = self.apply_lock.lock().await;
            applier.apply(plan).await?;
            true
        } else {
            false
        };
```

Add `applied: bool` with `#[serde(default)]` to `FormatDocumentResult`, add the same `apply` parameter to `FormatDocumentParams` with a description naming `apply.format_document`, forward it from the tool, and give `format_document` the same explicit annotation Task 9 gave `rename_symbol`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/
git commit -m "feat(format): apply formatting edits when config allows"
```

---

### Task 11: The `apply_code_action` tool

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs`
- Modify: `crates/mcpls-core/src/bridge/translator/dto.rs` (`CodeAction`)
- Modify: `crates/mcpls-core/src/mcp/tools.rs`
- Modify: `crates/mcpls-core/src/mcp/server.rs`
- Modify: `crates/mcpls-core/src/config/routing.rs` if a new `ToolKind` is needed; it is not, `ToolKind::CodeActions` covers this route.
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs`

**Interfaces:**
- Consumes: `Applier` (Task 8), `ApplyConfig::permits` (Task 1).
- Produces: `handle_apply_code_action(&self, file_path: String, start_line: u32, start_character: u32, end_line: u32, end_character: u32, action: CodeActionSelector) -> Result<ApplyCodeActionResult>`, plus `CodeActionSelector { Index(usize), Title(String) }` and `ApplyCodeActionResult { title: String, applied: bool, files_written: Vec<String>, executed_command: Option<String> }`. `CodeAction` gains `data: Option<serde_json::Value>` and `index: usize`.

`get_code_actions` returns a list, so `apply: true` on it has no defined meaning. Selection is a separate call that re-issues `textDocument/codeAction` for the same range, picks by index or exact title, resolves, and applies. Stateless, so there is no pending-edit cache to keep fresh.

`resolve_support.properties = ["edit"]` and `data_support = true` are already advertised (`lsp/lifecycle.rs:431-435`), but the DTO drops `data`, which `codeAction/resolve` requires the client to send back unchanged.

An action that resolves to a command rather than an edit is dispatched through `workspace/executeCommand`, and the server answers by sending `workspace/applyEdit`. That inbound path is Task 12. Until Task 12 lands, a command-only action returns `executed_command: Some(..)` with `applied: false`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn test_code_action_selector_matches_by_index() {
    let actions = vec![
        stub_action(0, "Add missing import"),
        stub_action(1, "Extract into function"),
    ];
    let chosen = select_action(&actions, &CodeActionSelector::Index(1))
        .expect("index 1 selects the second action");
    assert_eq!(chosen.title, "Extract into function");
}

#[test]
fn test_code_action_selector_matches_by_exact_title() {
    let actions = vec![
        stub_action(0, "Add missing import"),
        stub_action(1, "Extract into function"),
    ];
    let chosen = select_action(
        &actions,
        &CodeActionSelector::Title("Add missing import".to_string()),
    )
    .expect("exact title selects");
    assert_eq!(chosen.index, 0);
}

#[test]
fn test_code_action_selector_rejects_an_ambiguous_title() {
    let actions = vec![stub_action(0, "Fill match arms"), stub_action(1, "Fill match arms")];
    let error = select_action(
        &actions,
        &CodeActionSelector::Title("Fill match arms".to_string()),
    )
    .expect_err("two actions share the title");
    assert!(error.to_string().contains("ambiguous"));
}

#[test]
fn test_code_action_selector_rejects_an_out_of_range_index() {
    let actions = vec![stub_action(0, "Only one")];
    assert!(select_action(&actions, &CodeActionSelector::Index(7)).is_err());
}

fn stub_action(index: usize, title: &str) -> CodeAction {
    CodeAction {
        title: title.to_string(),
        kind: None,
        diagnostics: Vec::new(),
        edit: None,
        command: None,
        is_preferred: false,
        data: None,
        index,
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib test_code_action_selector`
Expected: FAIL to compile, `cannot find function 'select_action'`.

- [ ] **Step 3: Write the implementation**

Add to `dto.rs`, on `CodeAction`:

```rust
    /// Opaque payload the server needs back verbatim on
    /// `codeAction/resolve`. Dropping it makes an unresolved action
    /// impossible to resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,

    /// Position in the returned list, so `apply_code_action` can name this
    /// action without the caller repeating its title.
    pub index: usize,
```

Add to `edits.rs`:

```rust
/// How a caller names the code action to apply.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CodeActionSelector {
    /// Position in the list `get_code_actions` returned.
    Index(usize),
    /// Exact title, which must match exactly one action.
    Title(String),
}

/// Pick the action `selector` names.
///
/// # Errors
///
/// Returns [`Error::InvalidToolParams`] when the index is out of range, no
/// title matches, or a title matches more than one action.
fn select_action<'a>(
    actions: &'a [CodeAction],
    selector: &CodeActionSelector,
) -> Result<&'a CodeAction> {
    match selector {
        CodeActionSelector::Index(index) => actions.get(*index).ok_or_else(|| {
            Error::InvalidToolParams(format!(
                "action index {index} is out of range: {} actions available",
                actions.len()
            ))
        }),
        CodeActionSelector::Title(title) => {
            let mut matches = actions.iter().filter(|a| a.title == *title);
            let first = matches.next().ok_or_else(|| {
                Error::InvalidToolParams(format!("no code action titled {title:?}"))
            })?;
            if matches.next().is_some() {
                return Err(Error::InvalidToolParams(format!(
                    "title {title:?} is ambiguous: more than one action shares it, \
                     select by index instead"
                )));
            }
            Ok(first)
        }
    }
}
```

Add `handle_apply_code_action`. It repeats the `prepare_gated_document` and
`textDocument/codeAction` request that `handle_code_actions` performs, but keeps the raw
LSP values rather than converting them to DTOs, because `codeAction/resolve` needs the
server's own `data` payload back unchanged:

```rust
        let response: Option<lsp_types::CodeActionResponse> = client
            .request("textDocument/codeAction", params, client.request_timeout())
            .await?;

        // Legacy `Command` entries carry no edit and cannot be resolved, so
        // only `CodeAction` literals are selectable.
        let lsp_actions: Vec<lsp_types::CodeAction> = response
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| match entry {
                lsp_types::CodeActionOrCommand::CodeAction(action) => Some(action),
                lsp_types::CodeActionOrCommand::Command(_) => None,
            })
            .collect();

        let selectable: Vec<CodeAction> = lsp_actions
            .iter()
            .enumerate()
            .map(|(index, action)| CodeAction {
                title: action.title.clone(),
                kind: action.kind.as_ref().map(|k| k.as_str().to_string()),
                diagnostics: Vec::new(),
                edit: None,
                command: None,
                is_preferred: action.is_preferred.unwrap_or(false),
                data: action.data.clone(),
                index,
            })
            .collect();

        let chosen_index = select_action(&selectable, &action)?.index;
        let lsp_action = lsp_actions[chosen_index].clone();

        let resolved = if lsp_action.edit.is_none() && lsp_action.data.is_some() {
            client
                .request::<_, lsp_types::CodeAction>(
                    "codeAction/resolve",
                    lsp_action,
                    client.request_timeout(),
                )
                .await?
        } else {
            lsp_action
        };

        let _guard = self.apply_lock.lock().await;

        if let Some(edit) = resolved.edit {
            let plan = EditPlan::from_workspace_edit(edit)?;
            let summary = applier.apply(plan).await?;
            return Ok(ApplyCodeActionResult {
                title: resolved.title,
                applied: true,
                files_written: summary
                    .files_changed
                    .iter()
                    .map(|c| c.path.display().to_string())
                    .collect(),
                executed_command: None,
            });
        }

        let Some(command) = resolved.command else {
            return Err(Error::ApplyRefused(format!(
                "code action {:?} resolved to neither an edit nor a command",
                resolved.title
            )));
        };
        client
            .request::<_, serde_json::Value>(
                "workspace/executeCommand",
                lsp_types::ExecuteCommandParams {
                    command: command.command.clone(),
                    arguments: command.arguments.unwrap_or_default(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                },
                client.request_timeout(),
            )
            .await?;
        Ok(ApplyCodeActionResult {
            title: resolved.title,
            applied: false,
            files_written: Vec::new(),
            executed_command: Some(command.command),
        })
```

Register the tool in `mcp/server.rs` with an explicit non-read-only annotation and a `ApplyCodeActionParams` in `mcp/tools.rs` carrying the range plus the selector.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/
git commit -m "feat(actions): add apply_code_action tool"
```

---

### Task 12: Answer inbound `workspace/applyEdit`

**Files:**
- Modify: `crates/mcpls-core/src/lsp/client.rs:119-130` (`ClientCommand`), `:521-639` (`message_loop_inner`), `:641-680` (`server_request_response` and `server_request_result`)
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs` (install the sink around a code-action apply)
- Test: `crates/mcpls-core/src/lsp/client.rs`

**Interfaces:**
- Consumes: `Applier` (Task 8), the apply mutex on `Translator`.
- Produces: `ClientCommand::SendResponse { response: JsonRpcResponse }`, `LspClient::set_apply_sink(&self, sink: Option<ApplySink>)`, and `type ApplySink = mpsc::Sender<(WorkspaceEdit, oneshot::Sender<bool>)>`.

The message loop must never await the applier inline. It is a single `select!` over the command channel and the transport, and the applier's own resync sends notifications back through that same command channel, which has capacity 100 (`client.rs:163`). A rename touching 51 files sends 102 notifications, fills the channel, and blocks the applier against the loop that would drain it. So the request is handed to a spawned task, which answers later through `ClientCommand::SendResponse`.

The sink exists only while a code-action apply holds the apply mutex. Outside that window there is no sink and the answer stays `{"applied": false}`, so a server cannot write to the tree at a moment of its own choosing.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn test_apply_edit_is_refused_when_no_sink_is_installed() {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: "workspace/applyEdit".to_string(),
        params: Some(serde_json::json!({ "edit": { "changes": {} } })),
    };
    let response = LspClient::server_request_response(request, None).await;
    assert_eq!(
        response.result,
        Some(serde_json::json!({ "applied": false })),
        "with no apply in flight the server is told no"
    );
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn test_apply_edit_is_forwarded_to_an_installed_sink() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let (_edit, reply) = rx.recv().await.expect("the request arrives");
        reply.send(true).expect("the applier answers");
    });

    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: "workspace/applyEdit".to_string(),
        params: Some(serde_json::json!({ "edit": { "changes": {} } })),
    };
    let response = LspClient::server_request_response(request, Some(tx)).await;
    assert_eq!(response.result, Some(serde_json::json!({ "applied": true })));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib lsp::client`
Expected: FAIL to compile, `server_request_response` takes 1 argument and is not async.

- [ ] **Step 3: Write the implementation**

Add the sink type and the command variant:

```rust
/// Channel a code-action apply installs so an inbound `workspace/applyEdit`
/// can reach the applier. The `oneshot` carries the LSP `applied` answer.
pub type ApplySink = mpsc::Sender<(lsp_types::WorkspaceEdit, oneshot::Sender<bool>)>;
```

```rust
    /// Write a response the message loop did not produce itself, so a
    /// request needing async work does not block the loop.
    SendResponse {
        /// Response to write to the transport.
        response: JsonRpcResponse,
    },
```

Make `server_request_response` async and sink-aware, and keep every other method routed through the existing sync `server_request_result`:

```rust
    async fn server_request_response(
        request: JsonRpcRequest,
        apply_sink: Option<ApplySink>,
    ) -> JsonRpcResponse {
        if request.method == "workspace/applyEdit" {
            let applied = Self::forward_apply_edit(request.params.as_ref(), apply_sink).await;
            return JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(serde_json::json!({ "applied": applied })),
                error: None,
            };
        }
        match Self::server_request_result(&request.method, request.params.as_ref()) {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }

    async fn forward_apply_edit(
        params: Option<&Value>,
        apply_sink: Option<ApplySink>,
    ) -> bool {
        let Some(sink) = apply_sink else {
            return false;
        };
        let Some(edit) = params
            .and_then(|p| p.get("edit"))
            .and_then(|e| serde_json::from_value::<lsp_types::WorkspaceEdit>(e.clone()).ok())
        else {
            return false;
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if sink.send((edit, reply_tx)).await.is_err() {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }
```

Remove the `"workspace/applyEdit"` arm from `server_request_result` (`client.rs:672`), since the new path handles it before that function is reached.

In `message_loop_inner`, replace the inline call with a spawn, and handle the new command:

```rust
                        InboundMessage::Request(request) => {
                            let sink = apply_sink.lock().await.clone();
                            let command_tx = command_tx.clone();
                            tokio::spawn(async move {
                                let response =
                                    Self::server_request_response(request, sink).await;
                                let _ = command_tx
                                    .send(ClientCommand::SendResponse { response })
                                    .await;
                            });
                        }
```

```rust
                        ClientCommand::SendResponse { response } => {
                            let value = serde_json::to_value(&response)?;
                            transport.send(&value).await?;
                        }
```

The existing `InboundMessage::Request` arm serializes with `serde_json::to_value` before
sending, so the new arm does the same rather than handing the transport a typed value it
cannot write.

`apply_sink` is an `Arc<tokio::sync::Mutex<Option<ApplySink>>>` held by the client and cloned into the loop, set by:

```rust
    /// Install or remove the sink an inbound `workspace/applyEdit` reaches.
    ///
    /// Installed only while an apply-enabled code action holds the
    /// translator's apply mutex, so a server cannot push edits outside a
    /// call the agent made.
    pub async fn set_apply_sink(&self, sink: Option<ApplySink>) {
        *self.apply_sink.lock().await = sink;
    }
```

In `handle_apply_code_action`, wrap the `executeCommand` call:

```rust
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel(4);
        client.set_apply_sink(Some(sink_tx)).await;

        let applier_for_sink = Arc::clone(&applier);
        let pump = tokio::spawn(async move {
            let mut written = Vec::new();
            while let Some((edit, reply)) = sink_rx.recv().await {
                let outcome = match EditPlan::from_workspace_edit(edit) {
                    Ok(plan) => applier_for_sink.apply(plan).await.ok(),
                    Err(_) => None,
                };
                let applied = outcome.is_some();
                if let Some(summary) = outcome {
                    written.extend(
                        summary
                            .files_changed
                            .iter()
                            .map(|c| c.path.display().to_string()),
                    );
                }
                let _ = reply.send(applied);
            }
            written
        });

        let command_result = client
            .request::<_, serde_json::Value>(
                "workspace/executeCommand",
                lsp_types::ExecuteCommandParams {
                    command: command.command.clone(),
                    arguments: command.arguments.clone().unwrap_or_default(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                },
                client.request_timeout(),
            )
            .await;

        // Removing the client's clone and dropping the local one closes the
        // channel, which ends the pump loop and yields what it wrote. Both
        // must go: either sender alone keeps the receiver alive forever.
        client.set_apply_sink(None).await;
        drop(sink_tx);
        let files_written = pump.await.unwrap_or_default();
        command_result?;
```

Report `applied: !files_written.is_empty()` in the result.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib lsp::client`
Expected: PASS, including the existing `test_unknown_server_request_returns_method_not_found`.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/
git commit -m "feat(lsp): honor inbound applyEdit during code actions"
```

---

### Task 13: End-to-end rename against rust-analyzer

**Files:**
- Modify: `crates/mcpls-core/tests/ra_e2e.rs`
- Modify: `crates/mcpls-core/tests/fixtures/rust_workspace/src/` (add a fixture module the test renames)

**Interfaces:**
- Consumes: everything above, through the public `Translator` API.
- Produces: no new API.

The unit tests prove each piece. This proves the pieces compose against a real server: that rust-analyzer's actual `WorkspaceEdit` for a cross-file rename normalizes correctly, that the resulting files are valid Rust, and that the capability change in Task 6 makes a module rename return a file rename operation rather than text alone.

The test copies the fixture workspace into a temp directory first. It writes to disk, so it must never touch the checked-in fixture.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
#[allow(clippy::expect_used)]
async fn test_rename_with_apply_rewrites_every_referencing_file() {
    if !rust_analyzer_available() {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    }

    let workspace = copy_fixture_workspace();
    let translator = translator_with_apply(&workspace).await;

    let lib_path = workspace.path().join("src/lib.rs");
    let before = std::fs::read_to_string(&lib_path).expect("read lib.rs");
    assert!(
        before.contains("add_numbers"),
        "fixture must reference the symbol being renamed"
    );

    let functions_path = workspace.path().join("src/functions.rs");
    let (line, character) = position_of(&functions_path, "pub fn add_numbers");

    let result = translator
        .handle_rename(
            functions_path.display().to_string(),
            line,
            character,
            "sum_numbers".to_string(),
            true,
        )
        .await
        .expect("rename applies");

    assert!(result.applied);
    assert!(
        result.files_written.len() >= 2,
        "the definition and at least one reference are written: {:?}",
        result.files_written
    );

    let after_definition =
        std::fs::read_to_string(&functions_path).expect("read functions.rs");
    assert!(after_definition.contains("pub fn sum_numbers"));
    assert!(!after_definition.contains("add_numbers"));

    let after_lib = std::fs::read_to_string(&lib_path).expect("read lib.rs");
    assert!(
        after_lib.contains("sum_numbers"),
        "the reference in another file is rewritten too"
    );
}
```

Add the two helpers beside the existing ones in `ra_e2e.rs`:

```rust
/// Copy the checked-in fixture workspace into a temp directory, so a test
/// that writes cannot modify the repository.
#[allow(clippy::expect_used)]
fn copy_fixture_workspace() -> tempfile::TempDir {
    let source = rust_workspace_path();
    let dir = tempfile::tempdir().expect("tempdir");
    copy_dir_recursive(&source, dir.path()).expect("copy fixture");
    dir
}

/// Line and character (both 1-based, as mcpls tools take them) of the first
/// occurrence of `needle`, pointing at the identifier that follows it.
#[allow(clippy::expect_used)]
fn position_of(path: &std::path::Path, needle: &str) -> (u32, u32) {
    let text = std::fs::read_to_string(path).expect("read file");
    for (index, line) in text.lines().enumerate() {
        if let Some(column) = line.find(needle) {
            let identifier_column = column + needle.len() - "add_numbers".len();
            return (
                u32::try_from(index + 1).expect("line fits in u32"),
                u32::try_from(identifier_column + 1).expect("column fits in u32"),
            );
        }
    }
    panic!("{needle:?} not found in {}", path.display());
}
```

Write `copy_dir_recursive` and `translator_with_apply` alongside them. `translator_with_apply` builds a `Translator` the way the existing e2e tests do, plus:

```rust
    let applier = std::sync::Arc::new(mcpls_core::bridge::apply::Applier::new(
        vec![workspace.path().to_path_buf()],
        mcpls_core::config::ApplyConfig {
            rename: true,
            format_document: true,
            code_actions: true,
            allow_file_deletion: false,
        },
        mcpls_core::bridge::encoding::PositionEncoding::Utf16,
    ));
    let translator = translator.with_applier(applier);
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mcpls-core --test ra_e2e test_rename_with_apply -- --nocapture`
Expected: FAIL to compile, `cannot find function 'copy_fixture_workspace'`. After the helpers exist, expect a real assertion failure rather than a skip: confirm rust-analyzer is on PATH first with `which rust-analyzer`.

- [ ] **Step 3: Make it pass**

No new production code should be needed. If the test fails, the failure is a genuine integration bug in Tasks 3 through 9. Debug it there rather than weakening the assertions. The two likeliest causes:

- The position encoding rust-analyzer negotiated is not UTF-16, so `Applier::new` is being handed the wrong `PositionEncoding`. Take it from `Translator::position_encoding_for(&server_id)` rather than hardcoding.
- rust-analyzer returns `documentChanges` with an `AnnotatedTextEdit`, whose `text_edit` field Task 4 unwraps. Confirm the `OneOf::Right` arm is exercised.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mcpls-core/tests/
git commit -m "test(e2e): apply a real rename across files"
```

---

## Self-review notes

Checked against the spec's Part 1 section by section.

- Configuration: Task 1. Client capabilities: Task 6. Tool surface: Tasks 9, 10, 11. Normalization: Task 4. Encoding: Task 3. Staleness: partially covered, see the gap below. Confinement: Task 8. Atomicity: Task 7. Tracker updates: gap below. Resync: gap below. Return value: Tasks 9 and 10. Inbound applyEdit: Task 12.

Three spec requirements are deliberately deferred, because each needs the document-synchronization work that the file-watching plan builds:

1. **Staleness checks against `DocumentTracker`.** Task 8 reads current disk content and applies against it, which is the spec's rule for untracked targets. The tracked-target rule, requiring disk to equal what the tracker holds, needs `disk_phase` wired into the applier and belongs with the resync work.
2. **Tracker updates after a write.** Closing a renamed document under its old path and dropping its `NotificationCache` entry needs the same wiring.
3. **`didChange` and `didSave` after a write.** The applier writes but does not yet tell the servers. Until then, diagnostics after an apply come from whatever the server notices itself.

Each is a task in the file-watching plan rather than a hole here: applying edits is useful and correct without them, and doing them now would mean building the resync path twice.
