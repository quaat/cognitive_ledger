//! Phase-6A benchmark subsystem (Plan 0010; `docs/benchmarks/BENCHMARK_ARCHITECTURE.md`).
//!
//! - [`dataset`]: dataset lifecycle, profiles and manifest verification.
//! - [`synthetic`]: the `synthetic-ledger-*` generator and its independent oracle.
//! - [`workload`]: the dataset-independent workload the runner executes.
//! - [`runner`]: execution against a running ledger, with correctness assertions and timing.
//! - [`result`]: the JSON result schema and the report rendered from it.
//!
//! Qualification tooling only: never shipped in the runtime image, and no production crate
//! depends on it.

pub mod dataset;
pub mod result;
pub mod runner;
pub mod synthetic;
pub mod workload;
