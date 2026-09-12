# Build the mcspacetime bot. We build on stable Rust with RUSTC_BOOTSTRAP=1 (set in
# client/.cargo/config.toml) because azalea 0.10.3 — the only release for Minecraft 1.21.1 —
# uses a couple of nightly-only features; the vendored simdnbt/azalea-chat under client/vendor
# carry the small edits that let the tree compile on a 2026 toolchain (see their PATCH notes).
FROM rust:1-bookworm AS build
WORKDIR /build
COPY client/ ./client/
WORKDIR /build/client
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /build/client/target/release/mcspacetime-client /usr/local/bin/mcspacetime-client
# data/ (blockstates.json, neoforge_channels.json, ms-auth.json cache) is bind-mounted at /app/data
ENTRYPOINT ["mcspacetime-client"]
