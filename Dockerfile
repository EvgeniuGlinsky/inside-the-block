# Build stage: the whole tool is one static-ish Rust binary.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# Runtime stage: only the binary and CA roots (RPC and websocket are TLS).
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /src/target/release/itb /usr/local/bin/itb
ENV DATA_DIR=/app/data KEYS_DIR=/app/keys
ENTRYPOINT ["itb"]
# Dry run by default: plan, follow the real leader schedule, build and sign probes, send nothing.
CMD ["pipeline"]
