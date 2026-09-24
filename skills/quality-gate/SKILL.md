---
name: quality-gate
description: Use before claiming task or milestone completion and when diagnosing CI gate failures.
---

# Run and report quality gates

Read `docs/quality/quality-gates.md` and the active plan. Run the exact applicable scripts without rewriting files or hiding failures. Separate passed, failed, environment-unavailable, and unimplemented checks. Preserve full command/output evidence as appropriate. Never infer a pass, weaken assertions, or claim Docker execution when only static validation ran.
