# Differential testing

Fluree is a reference implementation only: never source, runtime, compiled dependency, or release-performance target. The optional `differential` Compose profile uses the official `fluree/server` image after a real digest is pinned in `test/reference-images.lock`.

Scenarios compare normalized graph states after initial data, addition, deletion/replacement, and historical lookup. Results are classified as: **equivalent semantics**, **intentional Sculpin divergence**, **reference difference**, or **test defect**. Timings are diagnostic only.
