//! Shared deterministic builders for integration tests.
use ledger_rdf::{Operation, OperationKind, Patch};
pub fn add(quad: &str) -> Operation {
    Operation {
        kind: OperationKind::Add,
        quad: quad.parse().expect("test quad is valid"),
    }
}
pub fn delete(quad: &str) -> Operation {
    Operation {
        kind: OperationKind::Delete,
        quad: quad.parse().expect("test quad is valid"),
    }
}
pub fn patch(operations: impl IntoIterator<Item = Operation>) -> Patch {
    Patch::new(operations).expect("test patch is valid")
}
