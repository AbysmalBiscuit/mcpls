//! Turning a `WorkspaceEdit` into an ordered, validated operation list.

use std::cmp::Ordering;
use std::collections::HashMap;

use lsp_types::{ChangeAnnotation, ChangeAnnotationIdentifier, TextEdit, Uri, WorkspaceEdit};
use tracing::warn;

use crate::bridge::uri_to_path;
use crate::error::{Error, Result};

/// The `changeAnnotations` map of the `WorkspaceEdit` being normalized.
type Annotations = HashMap<ChangeAnnotationIdentifier, ChangeAnnotation>;

/// Identity of the document an entry addresses, for comparing two entries.
///
/// The filesystem path the URI resolves to, so two spellings of one file
/// that differ only in percent-encoding compare equal; the URI itself for a
/// URI that names no file, which keeps it comparable with itself.
fn document_key(uri: &Uri) -> String {
    uri_to_path(uri).map_or_else(
        || uri.as_str().to_string(),
        |path| path.to_string_lossy().into_owned(),
    )
}

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
    /// have overlapping ranges, or when one document is addressed by two
    /// entries with no resource operation between them.
    pub fn from_workspace_edit(edit: WorkspaceEdit) -> Result<Self> {
        let annotations = edit.change_annotations.unwrap_or_default();
        let mut operations = Vec::new();
        // Each entry's ranges are computed against the document as it was
        // before the whole edit, so a second entry for a document already
        // edited would splice its ranges into text the first entry already
        // changed. The LSP specification says entries address distinct
        // documents; a resource operation on *that* path clears it, since
        // the document there is a different one afterwards.
        let mut already_edited: Vec<String> = Vec::new();

        match edit.document_changes {
            Some(lsp_types::DocumentChanges::Edits(edits)) => {
                for tde in edits {
                    Self::claim_document(&mut already_edited, &tde.text_document.uri)?;
                    operations.push(Self::text_document_edit(tde, &annotations)?);
                }
            }
            Some(lsp_types::DocumentChanges::Operations(ops)) => {
                for op in ops {
                    operations.push(match op {
                        lsp_types::DocumentChangeOperation::Edit(tde) => {
                            Self::claim_document(&mut already_edited, &tde.text_document.uri)?;
                            Self::text_document_edit(tde, &annotations)?
                        }
                        lsp_types::DocumentChangeOperation::Op(resource) => {
                            Self::release_documents(&mut already_edited, &resource);
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

    /// Record that `uri` is being edited, refusing a second entry for a
    /// document already edited and not since replaced.
    fn claim_document(already_edited: &mut Vec<String>, uri: &Uri) -> Result<()> {
        let key = document_key(uri);
        if already_edited.contains(&key) {
            return Err(Error::ApplyRefused(format!(
                "{} is addressed by two entries of one workspace edit: the second's ranges \
                 were computed against the text before the first, so applying both would \
                 corrupt it",
                uri.as_str()
            )));
        }
        already_edited.push(key);
        Ok(())
    }

    /// Forget the paths `resource` replaces, so a later entry may address
    /// them again.
    ///
    /// Only the paths this operation actually touches: an unrelated create
    /// or delete elsewhere in the edit says nothing about the document a
    /// previous entry edited, and clearing everything would let
    /// `edit a.rs; create b.rs; edit a.rs` through.
    fn release_documents(already_edited: &mut Vec<String>, resource: &lsp_types::ResourceOp) {
        let replaced: Vec<String> = match resource {
            lsp_types::ResourceOp::Create(create) => vec![document_key(&create.uri)],
            lsp_types::ResourceOp::Rename(rename) => {
                vec![document_key(&rename.old_uri), document_key(&rename.new_uri)]
            }
            lsp_types::ResourceOp::Delete(delete) => vec![document_key(&delete.uri)],
        };
        already_edited.retain(|key| !replaced.contains(key));
    }

    /// The normalized operations, in execution order.
    #[must_use]
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    fn text_document_edit(
        tde: lsp_types::TextDocumentEdit,
        annotations: &Annotations,
    ) -> Result<Operation> {
        let uri = tde.text_document.uri;
        let edits = tde
            .edits
            .into_iter()
            .map(|one_of| match one_of {
                lsp_types::OneOf::Left(te) => te,
                lsp_types::OneOf::Right(ate) => {
                    // The annotation asks the client to put this edit to the
                    // user before applying it. mcpls has no user to ask, so
                    // it applies it anyway; saying so is the most it can do.
                    if annotations
                        .get(&ate.annotation_id)
                        .is_some_and(|annotation| annotation.needs_confirmation == Some(true))
                    {
                        warn!(
                            "applying an edit to {} without confirming it: the server marked \
                             it with the annotation {:?}, which asks for confirmation",
                            uri.as_str(),
                            ate.annotation_id
                        );
                    }
                    ate.text_edit
                }
            })
            .collect();
        Self::edit_operation(uri, tde.text_document.version, edits)
    }

    fn edit_operation(uri: Uri, version: Option<i32>, edits: Vec<TextEdit>) -> Result<Operation> {
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
                let (overwrite, ignore_if_exists) = create.options.map_or((false, false), |o| {
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
                let (overwrite, ignore_if_exists) = rename.options.map_or((false, false), |o| {
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
            Operation::Edit {
                version: Some(3),
                ..
            }
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

    /// Each `TextDocumentEdit`'s ranges are computed against the document
    /// as it was before the whole edit, so two entries for one document
    /// would splice the second's ranges into text the first already
    /// changed. A server sending this is out of spec, but the failure mode
    /// is silent corruption.
    #[test]
    fn test_two_entries_for_one_document_are_refused() {
        let entry = |text: &str| TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: uri("/w/a.rs"),
                version: None,
            },
            edits: vec![OneOf::Left(edit(0, 0, text))],
        };

        let result = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![
                entry("first"),
                entry("second"),
            ])),
            ..WorkspaceEdit::default()
        });
        assert!(result.is_err(), "the second entry would corrupt the file");
    }

    /// A resource operation on some *other* path says nothing about the
    /// document an earlier entry edited, so it must not license a second
    /// entry for that document.
    #[test]
    fn test_an_unrelated_resource_operation_does_not_license_a_second_entry() {
        let entry = |path: &str| {
            DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri(path),
                    version: None,
                },
                edits: vec![OneOf::Left(edit(0, 0, "x"))],
            })
        };

        let result = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                entry("/w/a.rs"),
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri("/w/b.rs"),
                    options: None,
                    annotation_id: None,
                })),
                entry("/w/a.rs"),
            ])),
            ..WorkspaceEdit::default()
        });
        assert!(
            result.is_err(),
            "creating b.rs does not make a second entry for a.rs safe"
        );
    }

    /// Two URIs that differ only in percent-encoding name one file, so the
    /// second entry corrupts it exactly as an identical spelling would.
    #[test]
    fn test_two_spellings_of_one_path_are_still_two_entries() {
        let entry = |path: &str| TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: uri(path),
                version: None,
            },
            edits: vec![OneOf::Left(edit(0, 0, "x"))],
        };

        let result = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![
                entry("/w/a%2Eb.rs"),
                entry("/w/a.b.rs"),
            ])),
            ..WorkspaceEdit::default()
        });
        assert!(
            result.is_err(),
            "the two URIs resolve to the same path, so this is one document twice"
        );
    }

    /// A resource operation between two entries makes the second address a
    /// different document at that path, so it is legitimate.
    #[test]
    fn test_a_resource_operation_between_two_entries_is_allowed() {
        let entry = |path: &str| {
            DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri(path),
                    version: None,
                },
                edits: vec![OneOf::Left(edit(0, 0, "x"))],
            })
        };

        let plan = EditPlan::from_workspace_edit(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                entry("/w/a.rs"),
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri("/w/a.rs"),
                    options: None,
                    annotation_id: None,
                })),
                entry("/w/a.rs"),
            ])),
            ..WorkspaceEdit::default()
        })
        .expect("plan builds");
        assert_eq!(plan.operations().len(), 3);
    }

    #[test]
    fn test_empty_edit_produces_no_operations() {
        let plan = EditPlan::from_workspace_edit(WorkspaceEdit::default()).expect("plan builds");
        assert!(plan.operations().is_empty());
    }
}
