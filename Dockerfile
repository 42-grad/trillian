# Multi-stage build for the Trillian SPARQL server.
#
# Build:   docker build -t trillian .
# Run:     docker run --rm -p 9080:9080 -v "$PWD/data:/data" trillian <data.nt> 9080
#   or, with a prebuilt snapshot:
#          docker run --rm -p 9080:9080 -v "$PWD/data:/data" trillian load /data/kg.bin 9080
#
# The server listens on 0.0.0.0:<port> (default 9080); mount your data under /data.

FROM rust:1.97-slim AS builder
WORKDIR /build
# Copy the full source (Cargo.lock pins dependencies for a reproducible build).
COPY . .
RUN cargo build --release --bin server --locked

FROM debian:bookworm-slim
RUN useradd --system --create-home --uid 10001 trillian
COPY --from=builder /build/target/release/server /usr/local/bin/trillian-server
WORKDIR /data
USER trillian
EXPOSE 9080
# Pass server args at `docker run` time, e.g. `load /data/kg.bin 9080`.
ENTRYPOINT ["trillian-server"]
