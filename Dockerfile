# syntax=docker/dockerfile:1
# Builder pinned by digest as well (it produces the shipped binaries); refresh deliberately.
FROM rust:1.89-bookworm@sha256:948f9b08a66e7fe01b03a98ef1c7568292e07ec2e4fe90d88c07bb14563c84ff AS build
# cmake: required by aws-lc-sys (jsonwebtoken's aws-lc-rs crypto backend).
RUN apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p ledger-server -p ledger-projector \
 && mkdir -p /out/data && chown 65532:65532 /out/data

# Minimal runtime (Plan 0005 slice 2): distroless glibc base, no shell, no package manager,
# no curl, non-root uid 65532. Health probes use `ledger-admin probe` (loopback HTTP GET).
# Pinned by digest; refresh deliberately with the container scan (docs/quality/security.md).
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /src/target/release/ledger-server /usr/local/bin/ledger-server
COPY --from=build /src/target/release/ledger-admin /usr/local/bin/ledger-admin
COPY --from=build /src/target/release/ledger-projector /usr/local/bin/ledger-projector
COPY --from=build --chown=65532:65532 /out/data /data
USER 65532:65532
ENV LEDGER_DATA_DIR=/data LEDGER_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/ledger-server"]
