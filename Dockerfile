FROM rust:1.89-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p ledger-server
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 ledger && mkdir /data && chown ledger:ledger /data
COPY --from=build /src/target/release/ledger-server /usr/local/bin/ledger-server
COPY --from=build /src/target/release/ledger-admin /usr/local/bin/ledger-admin
USER ledger
ENV LEDGER_DATA_DIR=/data LEDGER_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["ledger-server"]
