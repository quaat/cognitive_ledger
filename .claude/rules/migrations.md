---
paths:
  - "migrations/**"
---
Released migrations are immutable. Add new migrations monotonically and test clean install plus supported upgrade paths. Ref CAS changes require a real concurrent PostgreSQL test.
