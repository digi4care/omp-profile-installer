#!/bin/sh
# Run the Rust quality gates in a pinned Docker toolchain.
#
# This is the single gate entry point: it is what developers run locally and
# what the GitHub Actions pull-request workflow (.github/workflows/rust-gates.yml)
# invokes, so local and CI results always come from the same pinned toolchain.
#
# Requires: docker.
#
# Toolchain: rust:1.100.0 (matches rust-toolchain.toml). rustfmt and clippy
# are installed inside the container; no host Rust toolchain is needed.
#
# Gates (any failure fails the script):
#   cargo fmt --all -- --check
#   cargo clippy --locked --all-targets -- -D warnings
#   cargo test --locked
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="rust:1.100.0"

# Build artifacts go to a container-local scratch dir, never the host
# repository's (possibly root-owned or reused) target/ directory.
exec docker run --rm \
    --volume "$ROOT":/repo \
    --workdir /repo \
    --env CARGO_TARGET_DIR=/tmp/omp-topology-target \
    "$IMAGE" sh -c '
        set -eu
        rustup component add rustfmt clippy
        cargo fmt --all -- --check
        cargo clippy --locked --all-targets -- -D warnings
        cargo test --locked
    '
