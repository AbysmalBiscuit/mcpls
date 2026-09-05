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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::mutable_key_type)]
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
