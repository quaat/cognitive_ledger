---
name: architecture-review
description: Use for architecture review, dependency changes, new crates/services, or protocol boundary changes.
---

# Review architecture boundaries

Inspect actual manifests and code. Check service ownership, inward dependency direction, infrastructure leakage into core, accidental graph-database functionality, speculative abstractions, projection/history coupling, and protocol compatibility. Classify output as confirmed defect, risk requiring decision, optional improvement, or no issue found. Require an ADR for expensive-to-reverse changes.
