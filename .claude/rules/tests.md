---
paths:
  - "tests/**"
  - "crates/**/tests/**"
---
Tests are deterministic unless explicitly stress/performance. Random failures print reproducible seeds. Never weaken assertions to fit an implementation or claim a skipped external test passed.
