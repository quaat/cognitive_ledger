---
name: differential-testing
description: Use when comparing Cognitive Ledger RDF behavior with Fluree or another reference.
---

# Design and assess differential tests

Fluree is reference implementation only: not source-code dependency, runtime dependency, or release-performance target. Use a digest-pinned official image. Compare normalized semantic graph states, never internal IDs. Classify every result: equivalent semantics, intentional Sculpin divergence, reference difference, or test defect. Record versions, inputs, and reproducible commands; timings are diagnostic.
