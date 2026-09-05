# LSP Edit Application Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `rename_symbol`, `format_document`, and code actions write their edits to the working tree when config permits, defaulting to read-only.

**Architecture:** A new `bridge/apply/` module turns an `lsp_types::WorkspaceEdit` into an ordered operation list, plans that list into a journal of reversible file-system steps, and executes the journal under a process-wide mutex. Tools gain an `apply` parameter gated by a new `[apply]` config table. Servers that deliver assists as commands answer through an inbound `workspace/applyEdit`, handled by a spawned task so the client message loop never parks against its own command channel.

**Tech Stack:** Rust 2024, tokio, `lsp_types` 0.97, rmcp 3.1, serde/toml, tempfile.

**Spec:** `docs/superpowers/specs/2026-09-05-lsp-apply-and-diagnostics-hooks-design.md` (Part 1, plus the "Multiple agents" section)

**Not in this plan:** Parts 2 and 3 of the spec, file watching and diagnostics injection, ship as a separate plan. Nothing here depends on them.

## Global Constraints

- Rust edition 2024, MSRV 1.88. Do not raise either.
- `unsafe_code = "deny"` at the workspace level. No `unsafe` blocks.
- `missing_docs = "warn"`: every public item gets a doc comment.
- clippy `pedantic` and `nursery` at warn, `unwrap_used` and `expect_used` at warn. Production code returns `Result`. Test code that needs `expect` carries `#[allow(clippy::expect_used)]` on the test function or the test module.
- `clippy::unused_async` is in `pedantic`. An `async fn` with no `.await` in its body blocks the commit.
- `ServerConfig` carries `#[serde(deny_unknown_fields)]`. Every new config struct does too.
- Default behavior is unchanged: with no `[apply]` table present, every tool stays read-only.
- Run `cargo clippy --workspace --all-targets` before each commit. Warnings introduced by your change block the commit.
- Commit messages follow Conventional Commits, imperative mood, subject at most 50 characters.

### Two guard tests fire whenever the tool surface moves

`crates/mcpls-core/src/mcp/server.rs` holds two regression guards that a task touching any `#[tool]` attribute, parameter struct, or annotation will trip:

- `test_tool_surface_matches_golden_snapshot` (line 2039) compares `tool_router().list_all()` against `crates/mcpls-core/src/mcp/tool_surface.json`, which pins each tool's name, description, title, annotations, and input schema.
- `test_tool_annotation_classifications_match_intent` (line 1609) holds a `(name, read_only, destructive, idempotent)` table and asserts its length equals the registered tool count.

Regenerating the golden file is one command, and every task below that changes the surface names it explicitly:

```bash
cargo test -p mcpls-core --lib dump_tool_surface -- --ignored --nocapture > /tmp/tool_surface.txt
```

The command prints a leading `running 1 test` banner and a trailing summary; copy only the JSON array into `crates/mcpls-core/src/mcp/tool_surface.json`.

---

### Task 1: The `[apply]` config table

**Files:**
- Modify: `crates/mcpls-core/src/config/mod.rs`
- Test: `crates/mcpls-core/src/config/mod.rs` (the existing `#[cfg(test)] mod tests` at line 884)

**Interfaces:**
- Consumes: nothing.
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
    let config: ServerConfig = toml::from_str("[apply]\nrename = true\n")
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
- Test: `crates/mcpls-core/src/error.rs` (the existing `#[cfg(test)] mod tests` at line 275)

**Interfaces:**
- Consumes: nothing.
- Produces: `Error::ApplyDisabled { tool: &'static str, config_key: &'static str }`, `Error::ApplyRefused(String)`, `Error::ApplyPartiallyFailed { written: Vec<PathBuf>, restored: Vec<PathBuf>, unrecovered: Vec<String>, reason: String }`.

The three groups in `ApplyPartiallyFailed` are exhaustive and each says exactly where the bytes are. Task 7's executor writes through a temp file and renames, so a file whose restore failed still holds its *new* content: it belongs in `written`, never in a vaguer "unknown" bucket. `unrecovered` carries free text rather than paths because the only way to land there is a file sitting at a path the caller did not name, such as the trash sibling a failed delete-rollback left behind, and the message has to say where.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/mcpls-core/src/error.rs`:

```rust
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
        unrecovered: vec!["/w/c.rs is at /w/.c.rs.mcpls-trash0".to_string()],
        reason: "permission denied".to_string(),
    };
    let message = err.to_string();
    for expected in ["/w/a.rs", "/w/b.rs", "mcpls-trash0", "permission denied"] {
        assert!(message.contains(expected), "missing {expected} in: {message}");
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
    #[error("{tool} cannot write: set `{config_key} = true` in mcpls.toml to allow it")]
    ApplyDisabled {
        /// MCP tool name the caller used.
        tool: &'static str,
        /// Dotted config key that would permit the write.
        config_key: &'static str,
    },

    /// The edit was rejected before anything was written.
    #[error("edit refused: {0}")]
    ApplyRefused(String),

    /// A step failed partway through and rollback did not fully restore the
    /// tree. Names every file in each state so the caller can recover.
    #[error(
        "apply failed partway: {reason}. left holding new content: {written:?}; \
         restored to original: {restored:?}; not recovered: {unrecovered:?}"
    )]
    ApplyPartiallyFailed {
        /// Files whose new content is on disk, including any whose restore
        /// itself failed: the restore is atomic, so a failed one changed
        /// nothing.
        written: Vec<PathBuf>,
        /// Files rolled back to their original content.
        restored: Vec<PathBuf>,
        /// Files that are at neither their original nor their new location,
        /// each described with where they actually are.
        unrecovered: Vec<String>,
        /// Why the original step failed.
        reason: String,
    },
```

`error.rs` already imports `PathBuf`, so no import change is needed.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mcpls-core --lib error::tests`
Expected: PASS, 2 new tests plus the module's existing ones.

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
- Consumes: `crate::bridge::encoding::{EncodingConverter, PositionEncoding}`. `character_to_byte_offset(&self, text: &str, character_offset: u32) -> Result<usize, String>` resolves a column within one line and accepts `character == len`; `byte_offset_to_character(&self, text: &str, byte_offset: usize) -> Result<u32, String>` is the inverse, used here to compute a line's own length in the server's encoding.
- Produces: `LineTable::new(text: &str) -> Self`, `LineTable::byte_offset(&self, position: lsp_types::Position, converter: &EncodingConverter) -> Result<usize>`, `LineTable::byte_range(&self, range: lsp_types::Range, converter: &EncodingConverter) -> Result<std::ops::Range<usize>>`.

Build the table by scanning for `\n` rather than using `str::lines()`, which drops a trailing empty line and strips `\r`. Both matter: LSP position `(1, 0)` is valid in `"a\n"`, and a CRLF file's column offsets count the `\r`.

A column past the end of its line is clamped to the line's length, which is what the LSP specification requires ("if the character value is greater than the line length it defaults back to the line length"). A line past the end of the document stays an error: there is no defensible offset to clamp it to, and a server asking for one is confused about which document it is editing.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcpls-core/src/bridge/apply/offsets.rs` containing only the test module for now:

```rust
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use lsp_types::{Position, Range};

    use super::LineTable;
    use crate::bridge::encoding::{EncodingConverter, PositionEncoding};

    fn utf16() -> EncodingConverter {
        EncodingConverter::new(PositionEncoding::Utf16)
    }

    #[test]
    fn test_resolves_ascii_position() {
        let table = LineTable::new("fn main() {}\nlet x = 1;\n");
        let offset = table
            .byte_offset(Position::new(1, 4), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 17, "line 1 starts at 13, plus 4 columns");
    }

    #[test]
    fn test_counts_utf16_units_not_bytes() {
        // "héllo" is 6 bytes and 5 UTF-16 units; column 3 sits after "hél".
        let table = LineTable::new("héllo\n");
        let offset = table
            .byte_offset(Position::new(0, 3), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 4, "h=1, é=2, l=1");
    }

    #[test]
    fn test_counts_utf8_bytes_when_the_server_negotiated_utf8() {
        let table = LineTable::new("héllo\n");
        let converter = EncodingConverter::new(PositionEncoding::Utf8);
        let offset = table
            .byte_offset(Position::new(0, 4), &converter)
            .expect("position resolves");
        assert_eq!(offset, 4, "in UTF-8 the column is already a byte offset");
    }

    #[test]
    fn test_crlf_line_keeps_carriage_return() {
        let table = LineTable::new("ab\r\ncd\r\n");
        let offset = table
            .byte_offset(Position::new(1, 2), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 6, "line 1 starts at 4 and \\r belongs to line 0");
    }

    #[test]
    fn test_position_on_line_after_final_terminator() {
        let table = LineTable::new("a\n");
        let offset = table
            .byte_offset(Position::new(1, 0), &utf16())
            .expect("the empty final line is addressable");
        assert_eq!(offset, 2);
    }

    #[test]
    fn test_column_past_end_of_line_clamps_to_the_line_end() {
        let table = LineTable::new("abc\ndef\n");
        let offset = table
            .byte_offset(Position::new(0, 99), &utf16())
            .expect("the spec says clamp, not refuse");
        assert_eq!(offset, 3, "end of line 0, before its newline");
    }

    #[test]
    fn test_line_beyond_document_is_an_error() {
        let table = LineTable::new("a\n");
        assert!(table.byte_offset(Position::new(9, 0), &utf16()).is_err());
    }

    #[test]
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
    /// A column past the end of its line resolves to the line's end, which
    /// the LSP specification requires. A line past the end of the document
    /// is an error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when the line is beyond the document
    /// or the column does not land on a character boundary.
    pub fn byte_offset(
        &self,
        position: Position,
        converter: &EncodingConverter,
    ) -> Result<usize> {
        let line_text = self.line(position.line)?;
        let line_length = converter
            .byte_offset_to_character(line_text, line_text.len())
            .map_err(|e| {
                Error::ApplyRefused(format!("measuring line {}: {e}", position.line))
            })?;
        let character = position.character.min(line_length);
        let within_line = converter
            .character_to_byte_offset(line_text, character)
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

Add to `crates/mcpls-core/src/bridge/mod.rs`, beside the other module declarations at lines 8 to 12:

```rust
pub mod apply;
```

`encoding` is already declared `mod encoding;` at line 8 with `pub use encoding::{PositionEncoding, ...}` at line 14. `apply` is a sibling module inside `bridge`, so `crate::bridge::encoding::EncodingConverter` resolves without widening anything.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::apply::offsets`
Expected: PASS, 9 tests.

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
- Produces: `EditPlan::from_workspace_edit(edit: lsp_types::WorkspaceEdit) -> Result<EditPlan>`, `EditPlan::operations(&self) -> &[Operation]`, and:

```rust
pub enum Operation {
    Edit { uri: lsp_types::Uri, version: Option<i32>, edits: Vec<lsp_types::TextEdit> },
    Create { uri: lsp_types::Uri, overwrite: bool, ignore_if_exists: bool },
    Rename { old: lsp_types::Uri, new: lsp_types::Uri, overwrite: bool, ignore_if_exists: bool },
    Delete { uri: lsp_types::Uri, recursive: bool, ignore_if_not_exists: bool },
}
```

Five rules the tests pin down.

1. `documentChanges` wins when both shapes are present. The specification says a client that advertises `documentChanges`, which Task 6 makes mcpls do, must prefer it. Concatenating both would apply every edit twice.
2. Operations keep their array order. A rename followed by an edit to the new path means something different from the reverse.
3. Text edits within one file sort descending by start position, so applying them front to back never invalidates a later range.
4. Among edits that start at the same position, later array entries sort first. Applying them front to back then reproduces array order in the resulting text, which is what the specification requires for consecutive inserts at one point. A stable sort alone gets this backwards and silently emits `"ba"` for an edit that says `"ab"`.
5. Two edits whose ranges genuinely overlap in one file are an error rather than a merge.

`ignore_if_exists` and `ignore_if_not_exists` are carried through rather than dropped. Both mean "skip this operation", and turning either into a refusal breaks a plan the server considers valid.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcpls-core/src/bridge/apply/plan.rs` with the test module:

```rust
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;

    use lsp_types::{
        CreateFile, DocumentChangeOperation, DocumentChanges, OneOf,
        OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp, TextDocumentEdit,
        TextEdit, Uri, WorkspaceEdit,
    };

    use super::{EditPlan, Operation};

    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).expect("valid uri")
    }

    fn edit(start_line: u32, end_line: u32, text: &str) -> TextEdit {
        TextEdit {
            range: Range::new(Position::new(start_line, 0), Position::new(end_line, 0)),
            new_text: text.to_string(),
        }
    }

    fn insert_at(line: u32, character: u32, text: &str) -> TextEdit {
        let point = Position::new(line, character);
        TextEdit {
            range: Range::new(point, point),
            new_text: text.to_string(),
        }
    }

    #[test]
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
    fn test_document_changes_wins_when_both_shapes_are_present() {
        let mut changes = HashMap::new();
        changes.insert(uri("/w/legacy.rs"), vec![edit(0, 0, "legacy")]);
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            changes: Some(changes),
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri("/w/modern.rs"),
                    version: None,
                },
                edits: vec![OneOf::Left(edit(0, 0, "modern"))],
            }])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");

        assert_eq!(
            plan.operations().len(),
            1,
            "applying both shapes would edit the same change twice"
        );
        let Operation::Edit { uri: target, .. } = &plan.operations()[0] else {
            panic!("expected an edit operation");
        };
        assert_eq!(target.as_str(), "file:///w/modern.rs");
    }

    #[test]
    fn test_edits_within_a_file_sort_bottom_up() {
        let mut changes = HashMap::new();
        changes.insert(
            uri("/w/a.rs"),
            vec![edit(1, 1, "first"), edit(5, 5, "second")],
        );
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
    fn test_inserts_at_one_position_apply_in_array_order() {
        let mut changes = HashMap::new();
        changes.insert(
            uri("/w/a.rs"),
            vec![insert_at(0, 0, "a"), insert_at(0, 0, "b")],
        );
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");
        let Operation::Edit { edits, .. } = &plan.operations()[0] else {
            panic!("expected an edit operation");
        };
        // Applied front to back into the same offset, the last one spliced
        // ends up first in the text, so "b" must be spliced before "a" for
        // the document to read "ab".
        assert_eq!(edits[0].new_text, "b");
        assert_eq!(edits[1].new_text, "a");
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
    fn test_create_carries_its_ignore_if_exists_flag() {
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri("/w/new.rs"),
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
        assert!(matches!(
            plan.operations()[0],
            Operation::Create {
                overwrite: false,
                ignore_if_exists: true,
                ..
            }
        ));
    }

    #[test]
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

use std::cmp::Ordering;

use lsp_types::{TextEdit, Uri, WorkspaceEdit};

use crate::error::{Error, Result};

/// One step of an edit, in the order it must be performed.
#[derive(Debug)]
pub enum Operation {
    /// Replace ranges within one document. `edits` is ordered so that
    /// splicing them front to back leaves every later range valid.
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
        /// Replace the file if it already exists. Wins over
        /// `ignore_if_exists`.
        overwrite: bool,
        /// Do nothing if the file already exists.
        ignore_if_exists: bool,
    },
    /// Move a file.
    Rename {
        /// Current path.
        old: Uri,
        /// Path to move it to.
        new: Uri,
        /// Replace the destination if it already exists. Wins over
        /// `ignore_if_exists`.
        overwrite: bool,
        /// Do nothing if the destination already exists.
        ignore_if_exists: bool,
    },
    /// Remove a file or directory.
    Delete {
        /// Path to remove.
        uri: Uri,
        /// Remove directory contents as well.
        recursive: bool,
        /// Do nothing if the path is already gone.
        ignore_if_not_exists: bool,
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
    /// resource operations interleaved. `documentChanges` wins when both
    /// are present, since mcpls advertises support for it. Operations keep
    /// their array order; text edits within a file are ordered bottom-up.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when two edits to the same document
    /// have overlapping ranges.
    pub fn from_workspace_edit(edit: WorkspaceEdit) -> Result<Self> {
        let mut operations = Vec::new();

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
            None => {
                if let Some(changes) = edit.changes {
                    for (uri, edits) in changes {
                        operations.push(Self::edit_operation(uri, None, edits)?);
                    }
                }
            }
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
        edits: Vec<TextEdit>,
    ) -> Result<Operation> {
        // Descending by start, and among equal starts descending by the
        // server's own array index. Splicing front to back then reproduces
        // array order in the text for consecutive inserts at one point,
        // which the LSP specification requires.
        let mut indexed: Vec<(usize, TextEdit)> = edits.into_iter().enumerate().collect();
        indexed.sort_by(|(left_index, left), (right_index, right)| {
            match right.range.start.cmp(&left.range.start) {
                Ordering::Equal => right_index.cmp(left_index),
                other => other,
            }
        });
        let edits: Vec<TextEdit> = indexed.into_iter().map(|(_, edit)| edit).collect();

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

    /// `lsp-types` 0.97 does not derive `Default` on the three file-option
    /// structs, so each absent-options case is spelled out here rather than
    /// at the call sites.
    fn resource_operation(resource: lsp_types::ResourceOp) -> Operation {
        match resource {
            lsp_types::ResourceOp::Create(create) => {
                let (overwrite, ignore_if_exists) =
                    create.options.map_or((false, false), |o| {
                        (
                            o.overwrite.unwrap_or(false),
                            o.ignore_if_exists.unwrap_or(false),
                        )
                    });
                Operation::Create {
                    uri: create.uri,
                    overwrite,
                    ignore_if_exists,
                }
            }
            lsp_types::ResourceOp::Rename(rename) => {
                let (overwrite, ignore_if_exists) =
                    rename.options.map_or((false, false), |o| {
                        (
                            o.overwrite.unwrap_or(false),
                            o.ignore_if_exists.unwrap_or(false),
                        )
                    });
                Operation::Rename {
                    old: rename.old_uri,
                    new: rename.new_uri,
                    overwrite,
                    ignore_if_exists,
                }
            }
            lsp_types::ResourceOp::Delete(delete) => {
                let (recursive, ignore_if_not_exists) =
                    delete.options.map_or((false, false), |o| {
                        (
                            o.recursive.unwrap_or(false),
                            o.ignore_if_not_exists.unwrap_or(false),
                        )
                    });
                Operation::Delete {
                    uri: delete.uri,
                    recursive,
                    ignore_if_not_exists,
                }
            }
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
Expected: PASS, 8 tests.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/apply/
git commit -m "feat(apply): normalize workspace edits into a plan"
```

---

### Task 5: Show resource operations and documentChanges in the preview

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/dto.rs` (`RenameResult` at line 110, `WorkspaceEditDescription` at line 210)
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs` (`convert_code_action` at line 103, `handle_rename` at line 173)
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs` (the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `EditPlan` and `Operation` from Task 4.
- Produces: `ResourceOperation { kind: String, uri: String, new_uri: Option<String> }`, a `resource_operations: Vec<ResourceOperation>` field on both `RenameResult` and `WorkspaceEditDescription`, and `resource_operations_from_plan(&EditPlan) -> Vec<ResourceOperation>`.

Two preview paths currently describe less than an apply would perform, and the gap widens the moment Task 6 lands:

- `handle_rename` (`edits.rs:226`) drops `DocumentChangeOperation::Op(_)` on the floor. Once Task 6 advertises `resourceOperations`, rust-analyzer starts returning file renames for a module rename, and the preview would show text edits with no mention of the file moving.
- `convert_code_action` (`edits.rs:115-141`) reads `edit.changes` only and ignores `document_changes` entirely, so a rust-analyzer assist previews as an empty edit while Task 12 applies a full `documentChanges` set.

Routing both through `EditPlan` gives preview and apply one normalization to disagree about. The visible consequence is that previewed edits now arrive bottom-up rather than in the server's array order, which is the order they are applied in.

- [ ] **Step 1: Write the failing tests**

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

#[test]
#[allow(clippy::expect_used)]
fn test_workspace_edit_description_reads_document_changes() {
    use std::str::FromStr;

    use lsp_types::{
        DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, Position, Range,
        TextDocumentEdit, TextEdit as LspTextEdit, Uri, WorkspaceEdit,
    };

    use crate::bridge::apply::EditPlan;
    use crate::bridge::translator::dto::workspace_edit_description_from_plan;

    let edit = WorkspaceEdit {
        document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: Uri::from_str("file:///w/a.rs").expect("valid uri"),
                version: None,
            },
            edits: vec![OneOf::Left(LspTextEdit {
                range: Range::new(Position::new(0, 0), Position::new(0, 3)),
                new_text: "new".to_string(),
            })],
        }])),
        ..WorkspaceEdit::default()
    };
    let plan = EditPlan::from_workspace_edit(edit).expect("plan builds");
    let description = workspace_edit_description_from_plan(&plan);

    assert_eq!(
        description.changes.len(),
        1,
        "a documentChanges-only action must not preview as empty"
    );
    assert_eq!(description.changes[0].uri, "file:///w/a.rs");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib bridge::translator::edits`
Expected: FAIL to compile, `unresolved imports 'resource_operations_from_plan', 'workspace_edit_description_from_plan'`.

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

/// Describe `plan` as a preview, with LSP ranges left as the server sent
/// them.
///
/// Callers holding an encoding context convert the ranges afterwards; this
/// function exists so a caller that has none, and no need to convert, still
/// sees `documentChanges`.
#[must_use]
pub fn workspace_edit_description_from_plan(
    plan: &crate::bridge::apply::EditPlan,
) -> WorkspaceEditDescription {
    use crate::bridge::apply::Operation;

    let changes = plan
        .operations()
        .iter()
        .filter_map(|op| match op {
            Operation::Edit { uri, edits, .. } => Some(DocumentChanges {
                uri: uri.to_string(),
                edits: edits
                    .iter()
                    .map(|edit| TextEdit {
                        range: Range {
                            start_line: edit.range.start.line,
                            start_character: edit.range.start.character,
                            end_line: edit.range.end.line,
                            end_character: edit.range.end.character,
                        },
                        new_text: edit.new_text.clone(),
                    })
                    .collect(),
            }),
            _ => None,
        })
        .collect();

    WorkspaceEditDescription {
        changes,
        resource_operations: resource_operations_from_plan(plan),
    }
}
```

Check the `Range` DTO's field names against `dto.rs` before writing that literal; if they differ, use the module's own constructor rather than inventing one.

Add the new field to `RenameResult` and to `WorkspaceEditDescription`, in both cases after their existing `changes` field:

```rust
    /// File-system operations the edit performs alongside its text changes.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub resource_operations: Vec<ResourceOperation>,
```

In `handle_rename` (`edits.rs:207-244`), replace the whole `let changes = if let Some(edit) = response { ... }` block, both the legacy-map branch and the `documentChanges` fallback, with one pass through `EditPlan`:

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

In `convert_code_action` (`edits.rs:115-141`), replace the `edit.changes`-only block with the same normalization, keeping the range conversion the DTO expects:

```rust
    let edit = match action.edit {
        Some(workspace_edit) => {
            let plan = EditPlan::from_workspace_edit(workspace_edit).ok();
            match plan {
                Some(plan) => {
                    let mut changes = Vec::new();
                    for operation in plan.operations() {
                        if let Operation::Edit {
                            uri: edit_uri,
                            edits,
                            ..
                        } = operation
                        {
                            let mut text_edits = Vec::with_capacity(edits.len());
                            for e in edits {
                                text_edits.push(TextEdit {
                                    range: ctx.normalize_range(edit_uri, e.range).await,
                                    new_text: e.new_text.clone(),
                                });
                            }
                            changes.push(DocumentChanges {
                                uri: edit_uri.to_string(),
                                edits: text_edits,
                            });
                        }
                    }
                    Some(WorkspaceEditDescription {
                        resource_operations: resource_operations_from_plan(&plan),
                        changes,
                    })
                }
                // An edit this malformed cannot be applied either, so the
                // preview says the action carries no usable edit rather
                // than failing the whole listing.
                None => None,
            }
        }
        None => None,
    };
```

Add to the imports at the top of `edits.rs`:

```rust
use crate::bridge::apply::{EditPlan, Operation};
```

and extend the `super::dto` import list with `ResourceOperation` and `resource_operations_from_plan`.

Every other construction site of `RenameResult` and `WorkspaceEditDescription` needs the new field. Find them with:

```bash
rg -n "RenameResult \{|WorkspaceEditDescription \{" /home/lev/Git/lev/mcpls/crates/mcpls-core
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::translator::edits`
Expected: PASS, including the module's existing tests.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/translator/
git commit -m "feat(preview): show file operations in edit previews"
```

---

### Task 6: Advertise the client capabilities apply needs

**Files:**
- Modify: `crates/mcpls-core/src/lsp/lifecycle.rs:410-465` (the `ClientCapabilities` literal inside `InitializeParams`)
- Test: `crates/mcpls-core/src/lsp/lifecycle.rs` (the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `build_client_capabilities(position_encodings: Vec<lsp_types::PositionEncodingKind>) -> ClientCapabilities`, a private free function in `lifecycle.rs`. The `initialize` handshake now declares `workspace.applyEdit`, `workspace.workspaceEdit.documentChanges`, `workspace.workspaceEdit.resourceOperations`, and `textDocument.synchronization.didSave`.

A server gates behavior on these. rust-analyzer will not emit file renames without `resourceOperations`, and nothing currently tells a server that mcpls sends `didSave`.

Do not add `workspace.didChangeWatchedFiles.dynamicRegistration` here. That belongs to the file-watching plan and, advertised without its notification half, makes gopls and tsgo strictly worse: gopls's `registerWatchedDirectoriesLocked` returns immediately when the client does not support dynamic registration, with no fallback.

The `tests` module in `lifecycle.rs` carries `#[allow(clippy::unwrap_used)]` but not `expect_used`. Add `#[allow(clippy::expect_used)]` beside it, or these tests fail the lint gate.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
#[allow(clippy::expect_used)]
fn test_client_capabilities_declare_edit_application() {
    let caps = build_client_capabilities(vec![]);
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
    let caps = build_client_capabilities(vec![]);
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

#[test]
#[allow(clippy::expect_used)]
fn test_client_capabilities_pass_through_position_encodings() {
    let caps = build_client_capabilities(vec![lsp_types::PositionEncodingKind::UTF8]);
    let general = caps.general.expect("general capabilities are declared");
    let encodings = general
        .position_encodings
        .expect("position encodings are declared");
    assert_eq!(encodings, vec![lsp_types::PositionEncodingKind::UTF8]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib lsp::lifecycle`
Expected: FAIL to compile, `cannot find function 'build_client_capabilities'`.

- [ ] **Step 3: Write the implementation**

The capabilities are built inline inside the `InitializeParams` literal, which is why nothing can assert them. Move the whole `ClientCapabilities` value into a free function in the same module, copying the existing `text_document` block verbatim and adding the four new declarations. `position_encodings` stays a parameter because it comes from config.

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
            // Everything below is the existing literal from
            // `InitializeParams`, moved unchanged. Copy it across rather
            // than retyping it: a dropped field silently narrows what a
            // server returns.
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

Take the `text_document` block from `lifecycle.rs:410-450` as it stands, including its `hover`, `definition`, `references`, and `code_action` entries (the `code_action` entry already carries `data_support: Some(true)` and `resolve_support.properties = vec!["edit"]`, which Task 12 depends on). The only edit to that block is inserting the `synchronization` field shown above.

`failure_handling: Abort` matches what the applier actually does: Task 8 plans and validates every operation before Task 7's executor writes anything, so a refused edit leaves the tree untouched.

In `InitializeParams`, replace the whole `capabilities: ClientCapabilities { ... }` literal with:

```rust
            capabilities: build_client_capabilities(resolve_position_encodings(
                &config.position_encodings,
            )),
```

`resolve_position_encodings` already exists at `lifecycle.rs:702`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib lsp::lifecycle`
Expected: PASS, 3 new tests plus the module's existing ones.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/lsp/lifecycle.rs
git commit -m "feat(lsp): declare edit application capabilities"
```

---

### Task 7: The journal executor

**Files:**
- Create: `crates/mcpls-core/src/bridge/apply/journal.rs`
- Modify: `crates/mcpls-core/src/bridge/apply/mod.rs`
- Test: `crates/mcpls-core/src/bridge/apply/journal.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Error::ApplyPartiallyFailed` (Task 2).
- Produces:

```rust
pub enum Step {
    Write { path: PathBuf, content: String, previous: Option<String> },
    Move { from: PathBuf, to: PathBuf },
    Trash { path: PathBuf, trash: PathBuf },
}
pub fn execute(steps: &[Step]) -> Result<()>;
```

Phase two of the applier, kept separate from planning so it can be tested by handing it steps directly, with no LSP server and no `WorkspaceEdit` in sight.

Every step is reversible and the steps run in the order the plan produced them, which is the only way to honor a `WorkspaceEdit` that creates a file and then edits it. A two-list design that writes every file and then performs resource operations cannot express that ordering, and gets it wrong for the exact shape rust-analyzer's "create module" assist emits.

Three step kinds cover every operation:

- `Write` replaces a file's whole content through a temp file in the same directory, renamed over the target, so a single file never observes a partial write. The temp file copies the target's permissions, because the rename replaces the inode and an executable script would otherwise lose its mode bit. Its name is dot-prefixed with a `.mcpls-tmp` suffix so it matches no source-file glob and no language server picks it up as a new source file. `previous` holds what to put back, or `None` when the file is being created, in which case rollback removes it.
- `Move` renames a path. The destination must not exist: `std::fs::rename` replaces it silently on Unix and fails on Windows, and neither is a behavior to build on. A plan that needs to replace an existing destination emits a `Trash` for it first.
- `Trash` renames a path to a sibling instead of deleting it. Rollback renames it back, which is the only way to undo a delete without holding the file, or a whole directory tree, in memory. Once every step succeeds, the executor removes the trash entries.

Rollback walks the completed steps in reverse. A failed `Write` rollback leaves that file holding its new content, since the restore is itself a temp-and-rename and either lands whole or does nothing at all: it is reported in `written`, not in a vaguer bucket. A failed `Move` or `Trash` rollback leaves a file at a path the caller never named, so it is reported in `unrecovered` with that path spelled out.

- [ ] **Step 1: Write the failing tests**

Create `crates/mcpls-core/src/bridge/apply/journal.rs` with the test module:

```rust
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
                from: from.clone(),
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib bridge::apply::journal`
Expected: FAIL to compile, `unresolved imports 'super::Step', 'super::execute'`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/mcpls-core/src/bridge/apply/journal.rs`:

```rust
//! Executing an ordered, reversible list of file-system steps.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::error::{Error, Result};

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
    let mut completed = 0usize;

    for step in steps {
        if let Err(reason) = perform(step) {
            return Err(roll_back(&steps[..completed], reason));
        }
        completed += 1;
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
            if to.exists() {
                return Err(format!(
                    "{} already exists, so {} cannot be moved onto it",
                    to.display(),
                    from.display()
                ));
            }
            fs::rename(from, to).map_err(|e| {
                format!("moving {} to {}: {e}", from.display(), to.display())
            })
        }
        Step::Trash { path, trash } => fs::rename(path, trash).map_err(|e| {
            format!("moving {} aside to {}: {e}", path.display(), trash.display())
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

    fs::rename(&temp, path).map_err(|e| {
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
                let outcome = match previous {
                    Some(original) => write_atomically(path, original),
                    None => fs::remove_file(path)
                        .map_err(|e| format!("removing {}: {e}", path.display())),
                };
                match outcome {
                    Ok(()) => restored.push(path.clone()),
                    // The restore is itself a temp-and-rename, so a failed
                    // one changed nothing: the file still holds the content
                    // this run put there.
                    Err(_) => written.push(path.clone()),
                }
            }
            Step::Move { from, to } => match fs::rename(to, from) {
                Ok(()) => restored.push(from.clone()),
                Err(e) => unrecovered.push(format!(
                    "{} is at {} ({e})",
                    from.display(),
                    to.display()
                )),
            },
            Step::Trash { path, trash } => match fs::rename(trash, path) {
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
            let outcome = if trash.is_dir() {
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
```

Add to `crates/mcpls-core/src/bridge/apply/mod.rs`:

```rust
pub mod journal;

pub use journal::{Step, execute};
```

`tempfile` is already a dev-dependency of `mcpls-core` (`Cargo.toml:35`), so no manifest change is needed.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::apply::journal`
Expected: PASS, 9 tests on Unix and 8 on Windows.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/apply/
git commit -m "feat(apply): add reversible file-system journal"
```

---

### Task 8: The applier

**Files:**
- Modify: `crates/mcpls-core/src/bridge/apply/mod.rs`
- Test: `crates/mcpls-core/src/bridge/apply/mod.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `LineTable` (Task 3), `EditPlan` and `Operation` (Task 4), `Step` and `execute` (Task 7), `ApplyConfig` (Task 1), `crate::bridge::validate_path_against_roots` and `crate::bridge::uri_to_path` (both already re-exported from `bridge/mod.rs` at lines 20 to 24).
- Produces:

```rust
pub struct FileChange { pub path: PathBuf, pub edits: usize }
pub struct ApplySummary {
    pub files_changed: Vec<FileChange>,
    pub resource_operations: Vec<crate::bridge::translator::ResourceOperation>,
}
#[derive(Debug)]
pub struct Applier { /* roots, config */ }
impl Applier {
    pub fn new(roots: Vec<PathBuf>, config: ApplyConfig) -> Self;
    pub const fn config(&self) -> &ApplyConfig;
    pub async fn apply(&self, plan: EditPlan, encoding: PositionEncoding) -> Result<ApplySummary>;
}
```

The encoding is a per-call parameter, not a field. `Translator::position_encoding_for(&ServerId)` exists because the negotiated encoding differs per server, and the default configuration (`position_encodings = ["utf-8", "utf-16"]`) gets UTF-8 from rust-analyzer and UTF-16 from taplo, marksman, and the TypeScript servers. One encoding baked into the applier misplaces every edit after a non-ASCII character for whichever group it guessed wrong about, which is the highest-consequence bug this module can have.

`Applier` derives `Debug` because `Translator` does and Task 9 gives it an `Applier` field.

Planning walks the operations in array order against an **overlay**: a map from resolved path to what that path holds *at this point in the plan*, seeded from disk on first touch. An `Edit` of a file an earlier `Create` in the same plan produced reads the overlay, not the disk, which is what makes create-then-edit work. Every operation appends its journal steps as it goes, so execution order is array order.

`apply` runs planning and execution inside `spawn_blocking`. Both do file I/O, and the translator holds its apply mutex across the call, so doing this work on a runtime thread would stall every other task behind it. It also gives the `async fn` a real `.await`, which `clippy::unused_async` requires.

- [ ] **Step 1: Write the failing tests**

Add to `crates/mcpls-core/src/bridge/apply/mod.rs`:

```rust
#[cfg(test)]
#[allow(clippy::expect_used)]
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
    use crate::bridge::PositionEncoding;
    use crate::bridge::path_to_uri;
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
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri,
                        version: None,
                    },
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
        assert_eq!(fs::read_to_string(&new).expect("read"), "fn new_name() {}\n");
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib bridge::apply::tests`
Expected: FAIL to compile, `unresolved import 'super::Applier'`.

- [ ] **Step 3: Write the implementation**

Replace the head of `crates/mcpls-core/src/bridge/apply/mod.rs` with:

```rust
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
    pub async fn apply(
        &self,
        plan: EditPlan,
        encoding: PositionEncoding,
    ) -> Result<ApplySummary> {
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
```

Then add the planner to the same file:

```rust
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
        let path = uri_to_path(uri)
            .ok_or_else(|| Error::InvalidUri(uri.as_str().to_string()))?;
        if path.exists() {
            return validate_path_against_roots(&path, self.roots);
        }
        let parent = path.parent().ok_or_else(|| {
            Error::ApplyRefused(format!("{} has no parent directory", path.display()))
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            Error::ApplyRefused(format!("{} has no file name", path.display()))
        })?;
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
            .ok_or_else(|| {
                Error::ApplyRefused(format!("{} has no file name", path.display()))
            })?
            .to_string_lossy()
            .into_owned();
        let index = self.steps.len();
        Ok(parent.join(format!(".{file_name}.mcpls-trash{index}")))
    }
}
```

`crate::bridge::translator` must re-export `ResourceOperation` and `resource_operations_from_plan` for the import above to resolve. `translator/mod.rs` line 39 already carries `pub use dto::*`, so Task 5's additions to `dto.rs` come through it; confirm with:

```bash
rg -n "pub use dto" /home/lev/Git/lev/mcpls/crates/mcpls-core/src/bridge/translator/mod.rs
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib bridge::apply`
Expected: PASS, all tests across the four apply modules.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/apply/
git commit -m "feat(apply): plan workspace edits into a journal"
```

---

### Task 9: Wire the applier into the translator and into `serve_with`

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/mod.rs` (the `Translator` struct at line 58, `Translator::new` at line 126)
- Modify: `crates/mcpls-core/src/lib.rs:620-625` (the translator construction inside `serve_with`)
- Test: `crates/mcpls-core/src/bridge/translator/mod.rs` and `crates/mcpls-core/src/lib.rs` (both have `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Applier` and `ApplyConfig` (Tasks 8 and 1), `Error::ApplyDisabled` (Task 2).
- Produces: `Translator::with_applier(self, Arc<Applier>) -> Self`, `Translator::applier_for(&self, ToolKind, &'static str, &'static str) -> Result<Arc<Applier>>`, the private fields `apply_lock: Arc<Mutex<()>>` and `applier: Arc<Applier>`, and the private free function `build_translator(...)` in `lib.rs`.

Without this task every later one is dead code: `serve_with` builds its translator with four `with_*` calls and no applier, so `apply: true` would return `ApplyDisabled` no matter what `mcpls.toml` said, and nothing in the suite would notice until the end-to-end test in Task 15.

`applier` is not an `Option`. A translator with no writable tools carries an applier whose config permits nothing, which is exactly what a default `ApplyConfig` means, and `permits` is then the single gate rather than one gate plus a `None` branch at every call site.

`apply_lock` serializes every apply-enabled call for its whole duration, so two concurrent applies to one file cannot both plan against the same pre-edit content and have the second overwrite the first. Task 14 also uses that window to decide when an inbound `workspace/applyEdit` is honored.

`serve_with`'s translator construction moves into a named function so a test can build the same translator the server runs on. Asserting on a helper that `serve_with` might not call would prove nothing.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/mcpls-core/src/bridge/translator/mod.rs`:

```rust
#[test]
#[allow(clippy::expect_used)]
fn test_applier_for_refuses_a_tool_its_config_forbids() {
    let applier = std::sync::Arc::new(crate::bridge::apply::Applier::new(
        Vec::new(),
        crate::config::ApplyConfig {
            rename: true,
            ..crate::config::ApplyConfig::default()
        },
    ));
    let translator = Translator::new().with_applier(applier);

    assert!(
        translator
            .applier_for(ToolKind::Rename, "rename_symbol", "apply.rename")
            .is_ok()
    );

    let error = translator
        .applier_for(
            ToolKind::FormatDocument,
            "format_document",
            "apply.format_document",
        )
        .expect_err("format_document is not permitted");
    assert!(
        error.to_string().contains("apply.format_document"),
        "the error names the key that would permit it: {error}"
    );
}

#[test]
fn test_a_default_translator_permits_no_writes() {
    let translator = Translator::new();
    for (tool, name, key) in [
        (ToolKind::Rename, "rename_symbol", "apply.rename"),
        (
            ToolKind::FormatDocument,
            "format_document",
            "apply.format_document",
        ),
        (ToolKind::CodeActions, "apply_code_action", "apply.code_actions"),
    ] {
        assert!(
            translator.applier_for(tool, name, key).is_err(),
            "{name} must be refused with no [apply] table"
        );
    }
}
```

Add to the `tests` module in `crates/mcpls-core/src/lib.rs`:

```rust
#[test]
#[allow(clippy::expect_used)]
fn test_serve_translator_carries_the_configured_apply_permissions() {
    let config: ServerConfig = toml::from_str("[apply]\nrename = true\n")
        .expect("config parses");
    let translator = build_translator(
        &config,
        Vec::new(),
        HashMap::new(),
        ToolRouter::default(),
        Arc::new(Mutex::new(NotificationCache::new())),
    );

    assert!(
        translator
            .applier_for(
                crate::config::ToolKind::Rename,
                "rename_symbol",
                "apply.rename"
            )
            .is_ok(),
        "`[apply] rename = true` must reach the translator serve_with runs on"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib applier_for; cargo test -p mcpls-core --lib serve_translator`
Expected: FAIL to compile, `no method named 'with_applier'` and `cannot find function 'build_translator'`.

- [ ] **Step 3: Write the implementation**

Add the two fields to `Translator` in `crates/mcpls-core/src/bridge/translator/mod.rs`, beside `notification_cache`:

```rust
    /// Serializes every apply-enabled tool call. Two concurrent applies to
    /// one file would otherwise both plan against the same pre-edit
    /// content and the second would overwrite the first.
    apply_lock: Arc<Mutex<()>>,

    /// Writes a permitted `WorkspaceEdit` to the working tree. Always
    /// present; a translator that may not write carries one whose
    /// `ApplyConfig` permits nothing.
    applier: Arc<Applier>,
```

In `Translator::new`, add:

```rust
            apply_lock: Arc::new(Mutex::new(())),
            applier: Arc::new(Applier::new(Vec::new(), ApplyConfig::default())),
```

Add the builder and the gate, beside `with_notification_cache`:

```rust
    /// Install the applier that permitted tools write through.
    ///
    /// Only called during single-owner setup (mirrors
    /// [`Self::with_router`]), before the translator is shared.
    #[must_use]
    pub fn with_applier(mut self, applier: Arc<Applier>) -> Self {
        self.applier = applier;
        self
    }

    /// The applier, if `tool` is permitted to write.
    ///
    /// Called before any LSP request, so a refused call costs nothing and
    /// reports the config key that would allow it rather than a generic
    /// permission error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyDisabled`] naming `config_key` when the tool's
    /// key is `false`.
    pub(crate) fn applier_for(
        &self,
        tool: ToolKind,
        tool_name: &'static str,
        config_key: &'static str,
    ) -> Result<Arc<Applier>> {
        if self.applier.config().permits(tool) {
            Ok(Arc::clone(&self.applier))
        } else {
            Err(Error::ApplyDisabled {
                tool: tool_name,
                config_key,
            })
        }
    }
```

Add to the imports at the top of `translator/mod.rs`:

```rust
use crate::bridge::apply::Applier;
use crate::config::ApplyConfig;
```

`Mutex` in that module already refers to `tokio::sync::Mutex` (see `respawn_locks`), and `Error`, `Result`, `ToolKind`, and `Arc` are already imported.

In `crates/mcpls-core/src/lib.rs`, extract the construction at lines 620 to 625 into a free function beside `serve_with`:

```rust
/// Build the translator `serve_with` runs on.
///
/// A named function rather than an inline chain so a test can construct
/// the same translator the server does, and so a new `with_*` call cannot
/// be added to one and forgotten in the other.
fn build_translator(
    config: &ServerConfig,
    workspace_roots: Vec<PathBuf>,
    extension_map: HashMap<String, String>,
    router: ToolRouter,
    notification_cache: Arc<Mutex<NotificationCache>>,
) -> Translator {
    let applier = Arc::new(Applier::new(workspace_roots.clone(), config.apply.clone()));
    let mut translator = Translator::new()
        .with_resource_limits(config.workspace.resource_limits())
        .with_extensions(extension_map)
        .with_router(router)
        .with_notification_cache(notification_cache)
        .with_applier(applier);
    translator.set_workspace_roots(workspace_roots);
    translator
}
```

Replace lines 620 to 625 with the call:

```rust
    let mut translator = build_translator(
        &config,
        workspace_roots.clone(),
        extension_map,
        router,
        Arc::clone(&notification_cache),
    );
```

The `translator.set_workspace_roots(workspace_roots.clone());` line that followed is now inside `build_translator`; delete it. `translator` stays `mut` because `set_expected_servers` and the later registration calls still take it that way.

Add `use crate::bridge::apply::Applier;` to `lib.rs`'s imports. The new test also needs `ServerConfig`, `ToolRouter`, `NotificationCache`, `HashMap`, `Arc`, and `Mutex` in scope inside `lib.rs`'s `tests` module; `serve_with` already imports all six at module level, so a `use super::*;` in the test module covers them.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib`
Expected: PASS, 3 new tests plus everything already green.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/
git commit -m "feat(apply): wire the applier into serve_with"
```

---

### Task 10: Apply from `rename_symbol`

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs` (`handle_rename`)
- Modify: `crates/mcpls-core/src/bridge/translator/dto.rs` (`RenameResult`)
- Modify: `crates/mcpls-core/src/mcp/tools.rs` (`RenameParams`)
- Modify: `crates/mcpls-core/src/mcp/server.rs:182-201` (the annotation default), `:290-315` (the `rename_symbol` tool), `:1609` (the classification table)
- Modify: `crates/mcpls-core/src/mcp/tool_surface.json`
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs`

**Interfaces:**
- Consumes: `Applier::apply` (Task 8), `Translator::applier_for` and `apply_lock` (Task 9).
- Produces: `handle_rename(&self, file_path: String, line: u32, character: u32, new_name: String, apply: bool) -> Result<RenameResult>`, and `RenameResult` gains `applied: bool` and `files_written: Vec<String>`.

`mcp/server.rs:182-192` stamps read-only annotations onto every tool and carries a doc comment asserting that every mcpls tool is a read-only query, with a comment above `rename_symbol` itself saying "mcpls has no write-back path today; revisit if that changes". This is that change. Give `rename_symbol` explicit annotations and rewrite both comments to state the rule that now holds.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/mcpls-core/src/bridge/translator/edits.rs`:

```rust
#[tokio::test]
#[allow(clippy::expect_used)]
async fn test_rename_with_apply_is_refused_when_config_forbids_it() {
    let translator = Translator::new();
    let error = translator
        .handle_rename("/w/a.rs".to_string(), 1, 1, "new".to_string(), true)
        .await
        .expect_err("apply must be refused by a read-only translator");
    let message = error.to_string();
    assert!(
        message.contains("apply.rename"),
        "the error names the config key that would permit it: {message}"
    );
}
```

`Translator::new()` builds an offline translator with no servers registered, and the gate runs before any routing, so this fails on the permission check rather than on "no server configured".

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mcpls-core --lib test_rename_with_apply_is_refused`
Expected: FAIL to compile, `handle_rename` takes 4 arguments.

- [ ] **Step 3: Write the implementation**

Add the parameter to `handle_rename` and gate before any LSP work:

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

Task 5 left the tail of `handle_rename` building `changes` from `plan.operations()` by reference, so the plan is still alive here. Replace its final `Ok(RenameResult { ... })` with:

```rust
        let (changes, resource_operations, applied, files_written) = if let Some(edit) = response
        {
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

            let (applied, files_written) = if let Some(applier) = applier {
                // Held for the whole apply, so a second apply-enabled call
                // cannot plan against content this one is about to replace.
                let _guard = self.apply_lock.lock().await;
                let summary = applier
                    .apply(plan, self.position_encoding_for(&server_id))
                    .await?;
                (
                    true,
                    summary
                        .files_changed
                        .iter()
                        .map(|change| change.path.display().to_string())
                        .collect(),
                )
            } else {
                (false, Vec::new())
            };

            (result_changes, resource_operations, applied, files_written)
        } else {
            (vec![], vec![], false, vec![])
        };

        Ok(RenameResult {
            changes,
            resource_operations,
            applied,
            files_written,
        })
```

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
    /// Write the edits to disk instead of only describing them.
    #[schemars(
        description = "Write the edits to disk instead of only describing them. \
                       Requires apply.rename = true in mcpls.toml."
    )]
    #[serde(default)]
    pub apply: bool,
```

Update the `rename_symbol` tool in `mcp/server.rs`: drop the two-line "read-only" comment above it, destructure `apply`, forward it, and declare annotations so the router's read-only default no longer covers it:

```rust
    /// Rename a symbol across the workspace, optionally writing the edits.
    #[tool(
        description = "Rename symbol across workspace. Returns text edits for all files \
                       where symbol is used. With apply=true, and apply.rename enabled in \
                       config, writes those edits to disk.",
        title = "Rename Symbol",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false)
    )]
    async fn rename_symbol(
        &self,
        Parameters(RenameParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            new_name,
            apply,
        }): Parameters<RenameParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_rename(file_path, line, character, new_name, apply)
                .await,
        )
    }
```

Rewrite the `tool_router` doc comment at `mcp/server.rs:182-192` so it describes the rule that now holds rather than the one that no longer does:

```rust
    /// Router for every MCP tool, with the read-only classification applied
    /// as a default.
    ///
    /// Most mcpls tools are read-only LSP queries, so applying that once
    /// here replaces an identical `annotations(...)` block on each `#[tool]`
    /// attribute. A tool that can write to disk declares its own
    /// annotations and keeps them;
    /// `test_tool_annotation_classifications_match_intent` forces any such
    /// tool to write down an explicit classification rather than inherit
    /// this default silently.
```

Update `test_tool_annotation_classifications_match_intent`'s table row:

```rust
            ("rename_symbol", false, true, false),
```

Regenerate the golden tool surface: the description, input schema, and annotations of `rename_symbol` all changed.

```bash
cargo test -p mcpls-core --lib dump_tool_surface -- --ignored --nocapture > /tmp/tool_surface.txt
```

Copy the JSON array out of `/tmp/tool_surface.txt` into `crates/mcpls-core/src/mcp/tool_surface.json`, then read the diff before staging it: only `rename_symbol` should have moved.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core`
Expected: PASS. The compiler will name every other call site of `handle_rename`; pass `false` at each.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/
git commit -m "feat(rename): apply rename edits when config allows"
```

---

### Task 11: Apply from `format_document`

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs` (`handle_format_document`)
- Modify: `crates/mcpls-core/src/bridge/translator/dto.rs` (`FormatDocumentResult`)
- Modify: `crates/mcpls-core/src/mcp/tools.rs` (`FormatDocumentParams`)
- Modify: `crates/mcpls-core/src/mcp/server.rs:359-380` (the `format_document` tool) and `:1609` (the classification table)
- Modify: `crates/mcpls-core/src/mcp/tool_surface.json`
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs`

**Interfaces:**
- Consumes: everything Task 10 consumes.
- Produces: `handle_format_document(&self, file_path: String, tab_size: u32, insert_spaces: bool, apply: bool) -> Result<FormatDocumentResult>`, and `FormatDocumentResult` gains `applied: bool`.

A formatting response is a `Vec<TextEdit>` for one document rather than a `WorkspaceEdit`, so it is wrapped into one before reaching the applier. That keeps a single normalization and a single write path, and it is where the same-position insert ordering from Task 4 earns its keep: formatters routinely emit several inserts at one point.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
#[allow(clippy::expect_used)]
async fn test_format_with_apply_is_refused_when_config_forbids_it() {
    let translator = Translator::new();
    let error = translator
        .handle_format_document("/w/a.rs".to_string(), 4, true, true)
        .await
        .expect_err("apply must be refused by a read-only translator");
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

Gate at the top of `handle_format_document`, before `prepare_gated_document`:

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

The handler currently consumes `edits` while building `result_edits`. Keep the LSP edits so they can be applied, and return `applied`:

```rust
        let edits = response.unwrap_or_default();

        let mut result_edits = Vec::with_capacity(edits.len());
        for edit in &edits {
            result_edits.push(TextEdit {
                range: ctx.normalize_range(&response_uri, edit.range).await,
                new_text: edit.new_text.clone(),
            });
        }

        let applied = if let Some(applier) = applier {
            let mut changes = std::collections::HashMap::new();
            changes.insert(response_uri.clone(), edits);
            let plan = EditPlan::from_workspace_edit(lsp_types::WorkspaceEdit {
                changes: Some(changes),
                ..lsp_types::WorkspaceEdit::default()
            })?;
            let _guard = self.apply_lock.lock().await;
            applier
                .apply(plan, self.position_encoding_for(&server_id))
                .await?;
            true
        } else {
            false
        };

        Ok(FormatDocumentResult {
            edits: result_edits,
            applied,
        })
```

Add to `FormatDocumentResult` in `dto.rs`:

```rust
    /// Whether the edits were written to disk.
    #[serde(default)]
    pub applied: bool,
```

Add to `FormatDocumentParams` in `mcp/tools.rs`:

```rust
    /// Write the edits to disk instead of only describing them.
    #[schemars(
        description = "Write the edits to disk instead of only describing them. \
                       Requires apply.format_document = true in mcpls.toml."
    )]
    #[serde(default)]
    pub apply: bool,
```

Update the `format_document` tool the way Task 10 updated `rename_symbol`: drop the "read-only" comment above it, destructure and forward `apply`, extend the description, and add `annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true)`. Formatting is idempotent in a way renaming is not: running it twice on the same file produces the same file.

Update the classification table row:

```rust
            ("format_document", false, true, true),
```

Regenerate `tool_surface.json` with the command from the Global Constraints section and check that only `format_document` moved.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core`
Expected: PASS. Pass `false` at every existing `handle_format_document` call site the compiler names.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/
git commit -m "feat(format): apply formatting edits when config allows"
```

---

### Task 12: The `apply_code_action` tool

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/dto.rs` (`CodeAction`, plus a new `ApplyCodeActionResult`)
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs` (`convert_code_action`, `handle_code_actions`, and a new `handle_apply_code_action`)
- Modify: `crates/mcpls-core/src/mcp/tools.rs` (a new `ApplyCodeActionParams`)
- Modify: `crates/mcpls-core/src/mcp/server.rs` (register the tool, extend the classification table)
- Modify: `crates/mcpls-core/src/mcp/tool_surface.json`
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs`

**Interfaces:**
- Consumes: `Applier` (Task 8), `applier_for` and `apply_lock` (Task 9).
- Produces: `handle_apply_code_action(&self, file_path: String, start_line: u32, start_character: u32, end_line: u32, end_character: u32, action_index: Option<usize>, action_title: Option<String>) -> Result<ApplyCodeActionResult>`, the private `CodeActionSelector { Index(usize), Title(String) }` and `select_action`, and `ApplyCodeActionResult { title: String, applied: bool, files_written: Vec<String>, executed_command: Option<String> }`. `CodeAction` gains `data: Option<serde_json::Value>` and `index: usize`.

`get_code_actions` returns a list, so `apply: true` on it has no defined meaning. Selection is a separate call that re-issues `textDocument/codeAction` for the same range, picks by index or exact title, resolves, and applies. Stateless, so there is no pending-edit cache to keep fresh.

**The index must count the same entries the listing numbered.** `handle_code_actions` pushes both `CodeActionOrCommand::CodeAction` and `CodeActionOrCommand::Command` entries into the list it returns. Filtering commands out before enumerating here would shift every index after the first command and apply the wrong action with no error at all. So this tool enumerates the unfiltered response, and a `Command` entry is selectable: it goes to `workspace/executeCommand` rather than through the applier.

`resolve_support.properties = ["edit"]` and `data_support = true` are already advertised (`lsp/lifecycle.rs`), but the DTO drops `data`, which `codeAction/resolve` requires the client to send back unchanged. That is why `data` joins the DTO here.

The selector reaches MCP as two flat optional fields rather than one untagged enum. An untagged `CodeActionSelector` does derive a working schema through rmcp, but it lands as a `$def` with a `$ref` from `properties.action`, the only one in the whole tool surface, and it makes `"3"` a title rather than an index. Two optional fields validated in the handler keep the schema flat and the meaning unambiguous; the enum stays as an internal type so `select_action` and its tests read the way they should.

An action carrying both an edit and a command applies the edit and then runs the command, which is what the LSP specification requires.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/mcpls-core/src/bridge/translator/edits.rs`:

```rust
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

#[test]
#[allow(clippy::expect_used)]
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
#[allow(clippy::expect_used)]
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
#[allow(clippy::expect_used)]
fn test_code_action_selector_rejects_an_ambiguous_title() {
    let actions = vec![
        stub_action(0, "Fill match arms"),
        stub_action(1, "Fill match arms"),
    ];
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

#[test]
#[allow(clippy::expect_used)]
fn test_selector_from_params_requires_exactly_one_field() {
    assert!(matches!(
        selector_from_params(Some(2), None).expect("index alone is valid"),
        CodeActionSelector::Index(2)
    ));
    assert!(matches!(
        selector_from_params(None, Some("Fill".to_string())).expect("title alone is valid"),
        CodeActionSelector::Title(_)
    ));
    assert!(selector_from_params(Some(0), Some("Fill".to_string())).is_err());
    assert!(selector_from_params(None, None).is_err());
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn test_apply_code_action_is_refused_when_config_forbids_it() {
    let translator = Translator::new();
    let error = translator
        .handle_apply_code_action("/w/a.rs".to_string(), 1, 1, 1, 5, Some(0), None)
        .await
        .expect_err("apply must be refused by a read-only translator");
    assert!(
        error.to_string().contains("apply.code_actions"),
        "the error names the config key"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib test_code_action_selector; cargo test -p mcpls-core --lib test_apply_code_action_is_refused`
Expected: FAIL to compile, `cannot find function 'select_action'`.

- [ ] **Step 3: Write the implementation**

Add the two fields to `CodeAction` in `dto.rs`:

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

Add the result type to `dto.rs`:

```rust
/// Result of applying one code action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyCodeActionResult {
    /// Title of the action that ran.
    pub title: String,
    /// Whether an edit reached the working tree.
    pub applied: bool,
    /// Files written, when `applied`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub files_written: Vec<String>,
    /// Command dispatched to the server, when the action carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_command: Option<String>,
}
```

In `convert_code_action`, set `data: action.data.clone()` and `index: 0`; `handle_code_actions` overwrites the index. Give the `Command` branch of `handle_code_actions` the same two fields (`data: None`, `index: 0`), and number the whole list as it is built:

```rust
        for (index, action_or_command) in response_vec.into_iter().enumerate() {
            let mut action = match action_or_command {
                lsp_types::CodeActionOrCommand::CodeAction(action) => {
                    convert_code_action(action, &ctx, &response_uri).await
                }
                lsp_types::CodeActionOrCommand::Command(cmd) => {
                    let arguments = cmd.arguments.unwrap_or_else(Vec::new);
                    CodeAction {
                        title: cmd.title.clone(),
                        kind: None,
                        diagnostics: Vec::new(),
                        edit: None,
                        command: Some(CommandDescription {
                            title: cmd.title,
                            command: cmd.command,
                            arguments,
                        }),
                        is_preferred: false,
                        data: None,
                        index: 0,
                    }
                }
            };
            action.index = index;
            actions.push(action);
        }
```

Add the selector and its parsing to `edits.rs`:

```rust
/// How a caller names the code action to apply.
///
/// Internal: the MCP surface takes two flat optional fields, which
/// [`selector_from_params`] narrows to this.
#[derive(Debug, Clone)]
enum CodeActionSelector {
    /// Position in the list `get_code_actions` returned.
    Index(usize),
    /// Exact title, which must match exactly one action.
    Title(String),
}

/// Narrow the tool's two optional selector fields to exactly one selector.
///
/// # Errors
///
/// Returns [`Error::InvalidToolParams`] when neither or both are given.
fn selector_from_params(
    action_index: Option<usize>,
    action_title: Option<String>,
) -> Result<CodeActionSelector> {
    match (action_index, action_title) {
        (Some(index), None) => Ok(CodeActionSelector::Index(index)),
        (None, Some(title)) => Ok(CodeActionSelector::Title(title)),
        (Some(_), Some(_)) => Err(Error::InvalidToolParams(
            "give either action_index or action_title, not both".to_string(),
        )),
        (None, None) => Err(Error::InvalidToolParams(
            "give one of action_index or action_title to name the action to apply"
                .to_string(),
        )),
    }
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

Add `handle_apply_code_action` to the `impl Translator` block in `edits.rs`. It repeats the request `handle_code_actions` makes but keeps the raw LSP values, because `codeAction/resolve` needs the server's own `data` payload back unchanged:

```rust
    /// Apply one of the code actions available for a range.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyDisabled`] when `apply.code_actions` is false,
    /// [`Error::InvalidToolParams`] when the selector names no action or an
    /// ambiguous one, and whatever the applier returns when the edit itself
    /// cannot be written.
    pub async fn handle_apply_code_action(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        action_index: Option<usize>,
        action_title: Option<String>,
    ) -> Result<ApplyCodeActionResult> {
        validate_code_action_params(start_line, start_character, end_line, end_character, None)?;
        let selector = selector_from_params(action_index, action_title)?;
        let applier =
            self.applier_for(ToolKind::CodeActions, "apply_code_action", "apply.code_actions")?;

        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CodeActions,
                "codeActionProvider",
                |caps| {
                    matches!(
                        caps.code_action_provider,
                        Some(
                            lsp_types::CodeActionProviderCapability::Simple(true)
                                | lsp_types::CodeActionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);

        let range = lsp_types::Range {
            start: ctx.to_lsp(&uri, start_line, start_character).await,
            end: ctx.to_lsp(&uri, end_line, end_character).await,
        };
        let params = lsp_types::CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::CodeActionResponse> = client
            .request("textDocument/codeAction", params, client.request_timeout())
            .await?;
        let entries = response.unwrap_or_default();

        // Numbered exactly as `handle_code_actions` numbers its output, so
        // an index the caller read from that listing names the same entry
        // here. Legacy `Command` entries occupy positions in that list, so
        // they are numbered too rather than filtered out.
        let selectable: Vec<CodeAction> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let title = match entry {
                    lsp_types::CodeActionOrCommand::CodeAction(action) => action.title.clone(),
                    lsp_types::CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
                };
                CodeAction {
                    title,
                    kind: None,
                    diagnostics: Vec::new(),
                    edit: None,
                    command: None,
                    is_preferred: false,
                    data: None,
                    index,
                }
            })
            .collect();
        let chosen_index = select_action(&selectable, &selector)?.index;

        let (title, edit, command) = match entries[chosen_index].clone() {
            lsp_types::CodeActionOrCommand::Command(cmd) => (cmd.title.clone(), None, Some(cmd)),
            lsp_types::CodeActionOrCommand::CodeAction(action) => {
                let resolved = if action.edit.is_none() && action.data.is_some() {
                    client
                        .request::<_, lsp_types::CodeAction>(
                            "codeAction/resolve",
                            action,
                            client.request_timeout(),
                        )
                        .await?
                } else {
                    action
                };
                (resolved.title, resolved.edit, resolved.command)
            }
        };

        // Held across both halves: an action can carry an edit and a
        // command, and the command may itself send `workspace/applyEdit`.
        let _guard = self.apply_lock.lock().await;

        let mut files_written = Vec::new();
        let mut applied = false;
        if let Some(edit) = edit {
            let plan = EditPlan::from_workspace_edit(edit)?;
            let summary = applier
                .apply(plan, self.position_encoding_for(&server_id))
                .await?;
            applied = true;
            files_written.extend(
                summary
                    .files_changed
                    .iter()
                    .map(|change| change.path.display().to_string()),
            );
        }

        let executed_command = if let Some(command) = command {
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
            Some(command.command)
        } else {
            None
        };

        if !applied && executed_command.is_none() {
            return Err(Error::ApplyRefused(format!(
                "code action {title:?} resolved to neither an edit nor a command"
            )));
        }

        Ok(ApplyCodeActionResult {
            title,
            applied,
            files_written,
            executed_command,
        })
    }
```

Extend `edits.rs`'s imports for this task: add `ApplyCodeActionResult` to the `super::dto` list, and `use std::sync::Arc;` (Task 14 needs it too).

A command-only action reaches the server but the server's `workspace/applyEdit` is still answered `{"applied": false}` until Task 14, so such an action returns `applied: false` with `executed_command` set. That is honest about what happened and it is what Task 14 changes.

Add `ApplyCodeActionParams` to `mcp/tools.rs`:

```rust
/// Parameters for the `apply_code_action` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for applying one code action from a range.")]
pub struct ApplyCodeActionParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Range in the file to operate on.
    #[serde(flatten)]
    pub range: RangeParams,
    /// Position of the action in the `get_code_actions` list.
    #[schemars(
        description = "Position of the action in the get_code_actions list for this same \
                       range. Give this or action_title, not both."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_index: Option<usize>,
    /// Exact title of the action.
    #[schemars(
        description = "Exact title of the action, which must match exactly one. Give this \
                       or action_index, not both."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_title: Option<String>,
}
```

Register the tool in `mcp/server.rs`, next to `get_code_actions`:

```rust
    /// Apply one of the code actions available for a range.
    #[tool(
        description = "Apply one code action from get_code_actions for the same range, by \
                       index or exact title. Requires apply.code_actions = true in config. \
                       Writes the action's edits to disk.",
        title = "Apply Code Action",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false)
    )]
    async fn apply_code_action(
        &self,
        Parameters(ApplyCodeActionParams {
            file_path,
            range:
                RangeParams {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
            action_index,
            action_title,
        }): Parameters<ApplyCodeActionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_apply_code_action(
                    file_path,
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                    action_index,
                    action_title,
                )
                .await,
        )
    }
```

Add the row to `test_tool_annotation_classifications_match_intent`. The table's length assertion checks it against the registered tool count, so the count rises by one:

```rust
            ("apply_code_action", false, true, false),
```

Regenerate `tool_surface.json` with the command from the Global Constraints section. This time a whole tool entry is new; confirm the diff adds exactly one.

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

### Task 13: Let the client route an inbound `workspace/applyEdit`

**Files:**
- Modify: `crates/mcpls-core/src/lsp/client.rs`: the `LspClient` struct and its hand-written `Clone` (lines 99 to 113), `ClientCommand` (116 to 130), `new` (137 to 152), `from_transport` (158 to 180), `from_transport_with_notifications` (186 to 203), `message_loop` (488), `message_loop_inner` (521 to 639), `server_request_response` (641), `server_request_result` (657)
- Test: `crates/mcpls-core/src/lsp/client.rs` (the existing `#[cfg(test)] mod tests` at line 693)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub type ApplySink = mpsc::Sender<(lsp_types::WorkspaceEdit, oneshot::Sender<bool>)>`, `LspClient::set_apply_sink(&self, sink: Option<ApplySink>)`, and `ClientCommand::SendResponse { response: JsonRpcResponse }`.

`client.rs:672` currently answers `workspace/applyEdit` with `{"applied": false}` unconditionally. Four of the servers in use send that request: gopls, vtsls, typescript-language-server, and lua-language-server all deliver some assists as `workspace/executeCommand` followed by an inbound edit. Without this path, those assists silently do nothing.

**The message loop must never await the applier inline.** It is a single `select!` over the command channel and the transport, and the applier's own follow-up notifications go back through that same command channel, which has capacity 100. A rename touching 51 files would fill it and block the applier against the very loop that drains it. So the request is handed to a spawned task, which writes its answer later through `ClientCommand::SendResponse`.

The sink is `None` by default and Task 14 installs one only while a code-action apply holds the translator's apply mutex. Outside that window the answer stays `{"applied": false}`, so a server cannot write to the tree at a moment of its own choosing.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/mcpls-core/src/lsp/client.rs`:

```rust
#[tokio::test]
async fn test_apply_edit_is_refused_when_no_sink_is_installed() {
    let request = JsonRpcRequest {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: RequestId::Number(1),
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
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: RequestId::Number(1),
        method: "workspace/applyEdit".to_string(),
        params: Some(serde_json::json!({ "edit": { "changes": {} } })),
    };

    let response = LspClient::server_request_response(request, Some(tx)).await;

    assert_eq!(response.result, Some(serde_json::json!({ "applied": true })));
}

#[tokio::test]
async fn test_apply_edit_is_refused_when_the_sink_is_gone() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);

    let request = JsonRpcRequest {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: RequestId::Number(1),
        method: "workspace/applyEdit".to_string(),
        params: Some(serde_json::json!({ "edit": { "changes": {} } })),
    };

    let response = LspClient::server_request_response(request, Some(tx)).await;

    assert_eq!(
        response.result,
        Some(serde_json::json!({ "applied": false })),
        "a dropped receiver answers no rather than hanging"
    );
}
```

Also change the existing `test_unknown_server_request_returns_method_not_found` (line 819) from `#[test]` to `#[tokio::test]`, make its body `async`, and pass `None` as the second argument. It keeps asserting exactly what it asserted before.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mcpls-core --lib lsp::client`
Expected: FAIL to compile, `server_request_response` takes 1 argument and is not `async`.

- [ ] **Step 3: Write the implementation**

Add the sink type near `ClientCommand`:

```rust
/// Channel a code-action apply installs so an inbound `workspace/applyEdit`
/// can reach the applier. The `oneshot` carries the LSP `applied` answer
/// back to the server.
pub type ApplySink = mpsc::Sender<(lsp_types::WorkspaceEdit, oneshot::Sender<bool>)>;
```

Add the command variant:

```rust
    /// Write a response the message loop did not produce itself, so a
    /// request that needs async work does not block the loop.
    SendResponse {
        /// Response to write to the transport.
        response: JsonRpcResponse,
    },
```

Add the field to `LspClient`:

```rust
    /// Installed only while an apply-enabled code action is in flight. When
    /// `None`, an inbound `workspace/applyEdit` is answered `applied:
    /// false`, so a server cannot write to the tree at a moment of its own
    /// choosing.
    apply_sink: Arc<Mutex<Option<ApplySink>>>,
```

Initialize it in `new`, `from_transport`, and `from_transport_with_notifications` with `Arc::new(Mutex::new(None))`, and clone it in the hand-written `Clone` impl alongside `pending_requests`:

```rust
            apply_sink: Arc::clone(&self.apply_sink),
```

Every clone must share one `Arc`: Task 14 installs the sink through a clone the translator holds while the message loop reads it through the clone it captured.

Add the setter:

```rust
    /// Install or remove the sink an inbound `workspace/applyEdit` reaches.
    pub async fn set_apply_sink(&self, sink: Option<ApplySink>) {
        *self.apply_sink.lock().await = sink;
    }
```

Both spawn sites hand the loop the new arguments. In `from_transport` and `from_transport_with_notifications`, build the sink `Arc` before the spawn and pass a clone of `command_tx` as well:

```rust
        let (command_tx, command_rx) = mpsc::channel(100);
        let apply_sink = Arc::new(Mutex::new(None));

        let receiver_task = tokio::spawn(Self::message_loop(
            transport,
            command_rx,
            command_tx.clone(),
            Arc::clone(&pending_requests),
            Arc::clone(&apply_sink),
            None,
        ));
```

`message_loop` and `message_loop_inner` take the two new parameters and pass them through:

```rust
    async fn message_loop(
        mut transport: LspTransport,
        mut command_rx: mpsc::Receiver<ClientCommand>,
        command_tx: mpsc::Sender<ClientCommand>,
        pending_requests: Arc<Mutex<PendingRequests>>,
        apply_sink: Arc<Mutex<Option<ApplySink>>>,
        notification_tx: Option<mpsc::Sender<LspNotification>>,
    ) -> Result<()> {
```

Replace the `InboundMessage::Request` arm (line 599) with a spawn:

```rust
                        InboundMessage::Request(request) => {
                            debug!(
                                "Received server request: {} (id={:?})",
                                request.method, request.id
                            );
                            // Answered off the loop: the applier this may
                            // reach sends its own notifications back
                            // through `command_tx`, so awaiting it here
                            // would deadlock against a full channel.
                            let sink = apply_sink.lock().await.clone();
                            let responder = command_tx.clone();
                            tokio::spawn(async move {
                                let response =
                                    Self::server_request_response(request, sink).await;
                                let _ = responder
                                    .send(ClientCommand::SendResponse { response })
                                    .await;
                            });
                        }
```

Add the matching command arm beside `SendNotification`. The existing request arm serialized with `serde_json::to_value` before sending, so this does the same:

```rust
                        ClientCommand::SendResponse { response } => {
                            let value = serde_json::to_value(&response)?;
                            transport.send(&value).await?;
                        }
```

Make `server_request_response` async and sink-aware, keeping every other method on the existing synchronous path:

```rust
    async fn server_request_response(
        request: JsonRpcRequest,
        apply_sink: Option<ApplySink>,
    ) -> JsonRpcResponse {
        if request.method == "workspace/applyEdit" {
            let applied = Self::forward_apply_edit(request.params.as_ref(), apply_sink).await;
            return JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: Some(serde_json::json!({ "applied": applied })),
                error: None,
            };
        }
        match Self::server_request_result(&request.method, request.params.as_ref()) {
            Ok(result) => JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }

    /// Hand an inbound edit to whatever apply is in flight, or refuse it.
    ///
    /// Every failure answers `false` rather than erroring: the server asked
    /// a yes-or-no question and an unparseable edit, an absent sink, and a
    /// dropped receiver are all "no".
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

Remove the `"workspace/applyEdit" => Ok(serde_json::json!({ "applied": false }))` arm from `server_request_result` (line 672). The new path handles that method before `server_request_result` is reached, so leaving the arm in would be dead code that also reads as the live answer.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core --lib lsp::client`
Expected: PASS, 3 new tests plus the module's existing ones, including the converted `test_unknown_server_request_returns_method_not_found`.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/lsp/client.rs
git commit -m "feat(lsp): route inbound applyEdit to a sink"
```

---

### Task 14: Honor an inbound `workspace/applyEdit` during a code action

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/edits.rs` (`handle_apply_code_action`)
- Test: `crates/mcpls-core/src/bridge/translator/edits.rs`

**Interfaces:**
- Consumes: `LspClient::set_apply_sink` and `ApplySink` (Task 13), `Applier` (Task 8).
- Produces: no new public API. `ApplyCodeActionResult::applied` becomes true for a command-driven action that wrote files.

Task 12 dispatches a command-only action and reports `applied: false`, because the server's answering `workspace/applyEdit` had nowhere to go. This closes that loop: for the duration of the `executeCommand` call, and only then, a sink forwards inbound edits to the same applier the direct path uses.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
#[allow(clippy::expect_used)]
async fn test_inbound_apply_edit_pump_writes_and_reports_files() {
    use std::collections::HashMap;
    use std::fs;

    use crate::bridge::PositionEncoding;
    use crate::bridge::apply::Applier;
    use crate::bridge::path_to_uri;
    use crate::config::ApplyConfig;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("target.rs");
    fs::write(&path, "fn old() {}\n").expect("seed");

    let applier = std::sync::Arc::new(Applier::new(
        vec![dir.path().to_path_buf()],
        ApplyConfig {
            code_actions: true,
            ..ApplyConfig::default()
        },
    ));

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let pump = spawn_apply_edit_pump(rx, std::sync::Arc::clone(&applier), PositionEncoding::Utf16);

    let mut changes = HashMap::new();
    changes.insert(
        path_to_uri(&path).expect("uri"),
        vec![lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 3),
                lsp_types::Position::new(0, 6),
            ),
            new_text: "new".to_string(),
        }],
    );
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send((
        lsp_types::WorkspaceEdit {
            changes: Some(changes),
            ..lsp_types::WorkspaceEdit::default()
        },
        reply_tx,
    ))
    .await
    .expect("the pump is listening");

    assert!(reply_rx.await.expect("the pump answers"));

    drop(tx);
    let written = pump.await.expect("the pump finishes");

    assert_eq!(written.len(), 1);
    assert_eq!(fs::read_to_string(&path).expect("read"), "fn new() {}\n");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mcpls-core --lib test_inbound_apply_edit_pump`
Expected: FAIL to compile, `cannot find function 'spawn_apply_edit_pump'`.

- [ ] **Step 3: Write the implementation**

Add the pump to `edits.rs` as a free function, so it can be tested without an LSP server:

```rust
/// Drain inbound `workspace/applyEdit` requests until the channel closes,
/// applying each and answering the server.
///
/// Returns every file written across the whole run, so the caller can
/// report them without tracking the pump's progress.
fn spawn_apply_edit_pump(
    mut rx: tokio::sync::mpsc::Receiver<(
        lsp_types::WorkspaceEdit,
        tokio::sync::oneshot::Sender<bool>,
    )>,
    applier: std::sync::Arc<crate::bridge::apply::Applier>,
    encoding: crate::bridge::PositionEncoding,
) -> tokio::task::JoinHandle<Vec<String>> {
    tokio::spawn(async move {
        let mut written = Vec::new();
        while let Some((edit, reply)) = rx.recv().await {
            let summary = match EditPlan::from_workspace_edit(edit) {
                Ok(plan) => applier.apply(plan, encoding).await.ok(),
                Err(_) => None,
            };
            let applied = summary.is_some();
            if let Some(summary) = summary {
                written.extend(
                    summary
                        .files_changed
                        .iter()
                        .map(|change| change.path.display().to_string()),
                );
            }
            let _ = reply.send(applied);
        }
        written
    })
}
```

In `handle_apply_code_action`, wrap the `executeCommand` call. Replace the `executed_command` block Task 12 wrote with:

```rust
        let executed_command = if let Some(command) = command {
            let (sink_tx, sink_rx) = tokio::sync::mpsc::channel(4);
            let pump = spawn_apply_edit_pump(
                sink_rx,
                Arc::clone(&applier),
                self.position_encoding_for(&server_id),
            );
            client.set_apply_sink(Some(sink_tx.clone())).await;

            let command_result = client
                .request::<_, serde_json::Value>(
                    "workspace/executeCommand",
                    lsp_types::ExecuteCommandParams {
                        command: command.command.clone(),
                        arguments: command.arguments.unwrap_or_default(),
                        work_done_progress_params: WorkDoneProgressParams::default(),
                    },
                    client.request_timeout(),
                )
                .await;

            // Both senders must go before the pump's receiver sees the
            // channel close: the client holds one and this scope holds the
            // other, and either alone keeps the pump waiting forever.
            client.set_apply_sink(None).await;
            drop(sink_tx);
            let pumped = pump.await.unwrap_or_default();

            command_result?;

            applied = applied || !pumped.is_empty();
            files_written.extend(pumped);
            Some(command.command)
        } else {
            None
        };
```

`applied` and `files_written` are already `mut` from Task 12's edit half.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: Check lints and commit**

```bash
cargo clippy -p mcpls-core --all-targets
git add crates/mcpls-core/src/bridge/translator/edits.rs
git commit -m "feat(actions): apply edits a command sends back"
```

---

### Task 15: End-to-end rename against rust-analyzer

**Files:**
- Modify: `crates/mcpls-core/tests/ra_e2e.rs` (`E2eConfig` at line 138, `write_config` at line 157, the sub-case registry at line 1425)
- Modify: `crates/mcpls-core/tests/fixtures/rust_workspace/src/lib.rs`

**Interfaces:**
- Consumes: everything above, through the MCP tool surface.
- Produces: `sc_rename_symbol_apply`, a new sub-case.

The unit tests prove each piece. This proves the pieces compose against a real server: that rust-analyzer's actual `WorkspaceEdit` normalizes correctly, that the files it names are rewritten, and that the offset conversion uses the encoding rust-analyzer actually negotiated.

`ra_e2e.rs` is one `#[test] fn ra_e2e_suite()` that spawns the mcpls binary, drives it through `McpClient`, and runs a table of `sc_*` sub-cases against one staged workspace shared for the whole process. This is a sub-case in that table, not a standalone test.

**It must be registered last.** Earlier sub-cases anchor on text with `find_line`, including `pub fn add(`, and this one renames `add`. Anything registered after it would look for text that is no longer there.

The fixture gains a line with a non-ASCII character before a reference to `add`. rust-analyzer negotiates UTF-8 under mcpls's default `position_encodings = ["utf-8", "utf-16"]`, so that line is what proves the applier used the negotiated encoding rather than assuming UTF-16. On a pure-ASCII fixture both choices produce the same bytes and the bug hides.

- [ ] **Step 1: Write the failing test**

Add to `crates/mcpls-core/tests/fixtures/rust_workspace/src/lib.rs`, after `caller`:

```rust
/// Non-ASCII text ahead of an `add` reference on the same line.
///
/// An applier that ignores the encoding rust-analyzer negotiated resolves
/// this line's columns against the wrong offsets and corrupts it, which no
/// pure-ASCII fixture would catch.
#[allow(dead_code)]
pub fn unicode_caller() -> String {
    format!("ü {}", add(1, 2))
}
```

Add the sub-case to `crates/mcpls-core/tests/ra_e2e.rs`:

```rust
/// Tool 25: `rename_symbol` with `apply` — rename `add` → `plus` and check
/// that the files really changed on disk.
///
/// Registered last: it rewrites `pub fn add(`, which earlier sub-cases
/// anchor on.
fn sc_rename_symbol_apply(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");

    let resp = client
        .call_tool(
            "rename_symbol",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
                "new_name": "plus",
                "apply": true,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    if inner["applied"] != json!(true) {
        return Err(format!("expected applied=true, got {inner}"));
    }
    let written = inner["files_written"]
        .as_array()
        .ok_or_else(|| format!("expected files_written array, got {inner}"))?;
    if written.is_empty() {
        return Err("rename reported applied with no files written".to_owned());
    }

    let after = fs::read_to_string(&lib).map_err(|e| format!("read lib.rs: {e}"))?;
    if !after.contains("pub fn plus(") {
        return Err("the definition was not rewritten on disk".to_owned());
    }
    if after.contains("pub fn add(") {
        return Err("the old definition is still on disk".to_owned());
    }
    if !after.contains("plus(1, 2)") {
        return Err("a reference in the same file was not rewritten".to_owned());
    }
    if !after.contains(r#"format!("ü {}", plus(1, 2))"#) {
        return Err(format!(
            "the non-ASCII line was corrupted or missed; \
             the applier used the wrong position encoding. Line reads: {:?}",
            after
                .lines()
                .find(|l| l.contains("ü"))
                .unwrap_or("<line gone>")
        ));
    }

    Ok(())
}
```

Register it as the last entry in the sub-case table:

```rust
        sub_case!(sc_subscribe_no_replay_without_cached_diagnostics),
        // Last: this one writes to the staged workspace, and every anchor
        // above it looks for text this rename moves.
        sub_case!(sc_rename_symbol_apply),
    ];
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p mcpls-core --test ra_e2e -- --ignored --nocapture`
Expected: the suite runs and `sc_rename_symbol_apply` fails with `expected applied=true`, because the e2e config has no `[apply]` table yet. Every earlier sub-case still passes. If rust-analyzer is not on PATH, install it with `rustup component add rust-analyzer` rather than skipping: this sub-case is the only proof the pieces compose.

- [ ] **Step 3: Turn on apply in the e2e config**

Give `E2eConfig` an apply table and set it in `write_config`:

```rust
#[derive(Serialize, Deserialize)]
struct E2eConfig {
    workspace: WorkspaceConfig,
    lsp_servers: Vec<LspServerConfig>,
    apply: ApplyTable,
}

#[derive(Serialize, Deserialize)]
struct ApplyTable {
    rename: bool,
}
```

```rust
    let cfg = E2eConfig {
        workspace: WorkspaceConfig {
            roots: vec![workspace_root.to_string_lossy().into_owned()],
        },
        lsp_servers: vec![LspServerConfig {
            language_id: "rust".to_owned(),
            command: ra_binary.to_string_lossy().into_owned(),
            args: vec![],
            file_patterns: vec!["**/*.rs".to_owned()],
        }],
        // Only `rename` is on: the earlier read-only sub-cases must keep
        // proving that a tool called without `apply` writes nothing.
        apply: ApplyTable { rename: true },
    };
```

`toml::to_string` emits tables after scalar keys, so `[apply]` serializes correctly wherever the field sits in the struct.

- [ ] **Step 4: Run the suite**

Run: `cargo test -p mcpls-core --test ra_e2e -- --ignored --nocapture`
Expected: PASS, every sub-case including the new one.

If `sc_rename_symbol_apply` fails now, the failure is a real integration bug in Tasks 3 through 10, not a reason to weaken an assertion. Two likely causes:

- The non-ASCII assertion fails while the ASCII ones pass. The applier is using the wrong encoding: check that `handle_rename` passes `self.position_encoding_for(&server_id)` rather than a constant, and print what that returns for this server.
- `files_written` is empty while `applied` is true. `Applier::apply` returned a summary with no `files_changed`, which means the plan produced no `Operation::Edit`. Print the `changes` array the same response carries: if it is populated and `files_written` is not, `Planner::record_change` is not being reached.

Then run the whole suite:

Run: `cargo test -p mcpls-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mcpls-core/tests/
git commit -m "test(e2e): apply a real rename across a file"
```

---

## Self-review notes

Checked against the spec's Part 1, section by section.

Configuration: Task 1. Client capabilities: Task 6. Tool surface: Tasks 10, 11, 12. Normalization: Task 4. Encoding: Tasks 3 and 8. Confinement: Task 8. Atomicity and ordering: Tasks 7 and 8. Return value: Tasks 10, 11, 12. Inbound applyEdit: Tasks 13 and 14. Production wiring: Task 9. Staleness, tracker updates, and resync: deferred, see below.

**Three spec requirements are deliberately deferred**, because each needs the document-synchronization work the file-watching plan builds:

1. **Staleness checks against `DocumentTracker`.** Task 8 plans against current disk content, which is the spec's rule for untracked targets. The tracked-target rule, requiring disk to equal what the tracker holds, needs `disk_phase` wired into the planner.
2. **Tracker updates after a write.** Closing a renamed document under its old path and dropping its `NotificationCache` entry needs the same wiring.
3. **`didChange` and `didSave` after a write.** The applier writes but does not tell the servers. Until then, diagnostics after an apply come from whatever the server notices itself.

Deferring these is mostly safe because `DocumentTracker::disk_phase` re-stats and re-reads every tracked file on each `ensure_open`, so a file resyncs on its next tool call. One hole survives that: a file an earlier call opened and a later apply rewrote without querying. For every server except rust-analyzer, which watches the filesystem itself, the server still holds pre-edit text under `didOpen`, and a second apply would compute edits against that stale text and land them on the new disk content.

**Close that hole cheaply in whichever task ships first after this plan**, rather than building the resync path twice: after a successful apply, for each written path where `document_tracker.close(path)` returns `Some`, send `textDocument/didClose`. The server then falls back to disk truth and the next `ensure_open` reopens cleanly. Without the `didClose`, calling `close` alone would produce a second `didOpen` for a document the server still considers open.

## Unresolved questions

1. **Directory renames.** `Planner::plan_rename` moves whatever the source is, directory included, and `Presence::Opaque` covers the case where its content is not readable text. gopls emits directory renames for package moves. Nothing in this plan tests one against a real server.
2. **`ignore_if_exists` on a rename whose source is also missing.** The specification does not say which check wins. This plan checks the source first and errors, on the grounds that a rename with no source is a plan the server got wrong, not a no-op it asked for.
3. **Trash entries left behind by a crash.** `purge_trash` runs only on the success path, and a process killed mid-run leaves `.name.mcpls-trash0` siblings. Nothing sweeps them. A later run whose trash path collides does not fail: `fs::rename` onto an existing file replaces it on both Unix and Windows, so the orphan is silently consumed by the new run's `Trash` step. That is harmless in itself -- the orphan was already unreferenced -- but it means a collision leaves no trace at all, and anyone who later adds a sweeper should not expect the collision to have surfaced.
