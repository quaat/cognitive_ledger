# Rust and service boundaries

## Status
Accepted

## Context
The service needs protocol-safe core logic and replaceable infrastructure without becoming a graph database.

## Decision
Use stable Rust edition 2024. Keep IDs, commits, errors, and traits in infrastructure-free `ledger-core`; RDF normalization in `ledger-rdf`; persistence in `ledger-store`; HTTP in `ledger-api`; composition in `ledger-server`.

## Alternatives considered
A monolith was simpler initially but makes dependency direction unenforceable. Java/Jena integration was rejected because semantics remain an external Sculpin boundary.

## Consequences
The workspace has more explicit seams, but core logic can be tested without databases, HTTP, or Docker.
