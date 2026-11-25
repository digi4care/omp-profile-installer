# Pinned Rust toolchain for the persistent local dev container.
#
# Compose bind-mounts the repository at /workspace and keeps Cargo caches on
# named volumes (see compose.yaml), so this image carries only the toolchain.
# CARGO_HOME/CARGO_TARGET_DIR are set by compose.yaml, not here, so the
# image's own cargo/rustup installation under /usr/local stays intact.
FROM rust:1.100.0

RUN rustup component add rustfmt clippy

WORKDIR /workspace
