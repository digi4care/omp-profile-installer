# Repository Guidelines

## Project Overview

`omp-topology` is a Rust CLI for discovering and installing versioned topology overlays into OMP agent profiles. The catalog is `topologies/<slug>/topology.yml`; each topology supplies an `agent/` overlay. The tool supports listing, install/dry-run, status, update, removal, profile discovery, and signed self-update. There is no server component.

## Architecture & Data Flow

CLI parsing and orchestration in `src/main.rs` resolve a topology source (`src/source.rs`), validate its manifest and overlay (`src/topology.rs`), resolve the target profile (`src/profiles.rs`), then plan and apply filesystem changes (`src/install.rs` / `src/remove.rs`). Ownership manifests in `src/state.rs` support safe removal; `src/registry.rs` stores alias/source metadata. `src/update.rs` and `src/status.rs` use this state for refresh and freshness checks.

Important invariants: installation plans before mutation and rejects collisions; apply rechecks live files and rolls back failures. Removal preserves user edits. Filesystem operations reject symlinks and use descriptor-anchored operations in `src/anchored/`, the only scoped `unsafe` boundary. External processes are invoked with argv, not shell strings. Keep JSON output to one document on stdout; errors go to stderr.

## Key Directories

- `src/` — CLI and installer implementation; `src/anchored/` contains anchored filesystem operations and their tests.
- `tests/` — end-to-end CLI, installer contract, and regression suites.
- `topologies/<slug>/` — topology manifest, `agent/` payload, README, and changelog. The source tree is the catalog; no separate index is required.
- `scripts/` — Rust quality gate and signed-release installer.
- `docs/` — project-wide installer guidance and topology README template. Topology-specific docs under `topologies/*/docs/` are gitignored; do not treat them as tracked deliverables.
- `.github/workflows/` — Rust gates, topology validation, and release automation.

## Development Commands

Use the pinned Rust toolchain and lockfile. The canonical gate is:

```sh
scripts/rust-gates.sh
```

It runs formatting, Clippy with warnings denied, and the full test suite in the pinned Docker image. For the warm-cache development container:

```sh
docker compose up -d --build
docker compose exec rust ./scripts/rust-gates.sh
docker compose down
```

Focused installer tests: `cargo test --locked --test installer`. Run locally with `cargo run --locked -- list --json`; dry-run an install with `cargo run --locked -- install -t <slug> --dry-run --agent-dir "$(mktemp -d)/agent"` (target directory basename must be `agent`). Release builds use `cargo build --release --locked --target <target>`.

## Code Conventions & Common Patterns

- Follow existing Rust module patterns; modules document guarantees and failure behavior. Keep changes small and avoid dependencies or abstractions without a concrete need.
- Use the shared `CliError` and path-aware I/O error helpers; expected failures should be returned, not panicked.
- Preserve deterministic ordering (`BTreeMap`/`BTreeSet`), atomic writes, locking, rollback, and owner-only state permissions where applicable.
- Validate untrusted input at boundaries. Fail closed on malformed state, symlinks, and signature/checksum failures.
- Keep unsafe code confined to `src/anchored.rs`, with safety comments. The crate denies unsafe code by default.
- Command handlers capture output for human/JSON rendering rather than printing ad hoc. Parse SemVer with the existing parser; do not replace it with string splitting.
- Rust tests use descriptive snake_case names, standard `#[test]` assertions, isolated temporary directories, and controlled HOME/XDG environment. Follow local test helpers rather than adding a test dependency.

## Important Files

- `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` — crate metadata, locked dependencies, and Rust 1.100.0 toolchain (edition 2024).
- `README.md` — user-facing features, architecture, and CLI usage.
- `CONTRIBUTING.md`, `DEVELOPMENT_RUST.md` — contribution workflow, verification, and engineering practices.
- `docs/installer.md` — installer behavior and troubleshooting.
- `scripts/rust-gates.sh` — canonical local/CI quality gate.
- `scripts/install-release.sh`, `keys/omp-topology-release-public.pem` — signed bootstrap trust material; keep embedded and file keys synchronized when rotating.
- `compose.yaml`, `Dockerfile` — pinned development container and toolchain.

## Runtime/Tooling Preferences

Rust 1.100.0 is pinned in `rust-toolchain.toml`; use Cargo with `--locked` for checks and builds. Docker is the supported path for the canonical gate; Compose caches Cargo artifacts. This is a Unix-focused project (Linux and macOS release targets); Windows is not supported. Runtime integrations use `git` for remote sources and `omp` for profile resolution. Release installation requires POSIX `sh`, `curl`, `openssl`, `tar`, and platform checksum tools (`sha256sum` on Linux, `shasum` on macOS).

## Testing & QA

Tests use Rust's built-in harness: unit tests live beside modules and integration tests are `tests/cli.rs`, `tests/installer.rs`, and `tests/regressions.rs`. Run the focused installer contract suite for installer/topology-discovery changes, and `scripts/rust-gates.sh` for the full format, lint, and test gate. No enforced coverage percentage is configured. Keep filesystem tests deterministic and isolated; assert observable behavior, including failure and rollback cases. Report the exact checks run; CI jobs are path-filtered, so a skipped job does not verify its domain.

Topology payload changes also require a version bump in `topology.yml` and a dated entry in that topology's `CHANGELOG.md` in the same change. Topology versions are independent of the CLI crate version.
