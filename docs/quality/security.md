# Security

Treat RDF, metadata, IDs, paths, and HTTP bodies as untrusted. Reject malformed IDs/terms and blank nodes; derive object paths only from parsed digests; cap input and ancestry work before production. Preserve object digest verification and atomic ref updates. Never commit secrets. Authentication belongs at the service boundary and is not implemented in bootstrap.

Dependency and license findings must be classified rather than ignored. Fluree's BSL image is optional test infrastructure and is not shipped. The intended runtime dependency policy permits common permissive licenses; strong copyleft additions require review.
