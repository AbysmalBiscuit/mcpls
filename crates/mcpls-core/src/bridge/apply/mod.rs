//! Writing LSP `WorkspaceEdit`s to the working tree.

pub mod offsets;
pub mod plan;

pub use offsets::LineTable;
pub use plan::{EditPlan, Operation};
