# Storage boundaries

`ObjectStore` accepts bytes addressed by their digest. Repeating the same write succeeds; different bytes under an ID are corruption. Filesystem objects live below digest-derived paths and are published by atomic rename after sync.

`CommitStore` validates a commit's ID and parent existence before storage. `RefStore` exposes read and compare-and-set. The filesystem adapter serializes writers with an OS file lock (released on process exit) and atomically replaces the `main` file; its guarantees assume one host and one filesystem. PostgreSQL is the intended horizontally safe mutable-ref store, not yet implemented.

Objects are written before the ref. Files and their containing directories are synchronized after atomic rename before acknowledgement; crash leftovers may be unreachable but cannot make the ref invalid. The API never acknowledges an advanced ref before durable publication. Platform/filesystem durability assumptions still require deployment qualification and fault testing.
