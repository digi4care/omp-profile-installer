<div align="center">

<br>

### omp-topologies
**Versioned, self-contained topology overlays for [OMP](https://github.com/can1357/oh-my-pi) agent profiles**<br>
The `omp-topology` CLI discovers topologies in a source tree and installs them into
named OMP profiles: a signed-release bootstrap for the CLI itself, dry-runnable
and reversible installs, an alias registry, and signature-verified self-update —
with no server to run and no sudo required.

<br>

<a href="#-quick-start"><img src="https://img.shields.io/badge/Quick_Start-06B6D4?style=for-the-badge&logoColor=white" alt="Quick Start"></a>
<a href="#-features"><img src="https://img.shields.io/badge/Features-D98324?style=for-the-badge&logoColor=white" alt="Features"></a>
<a href="#-architecture"><img src="https://img.shields.io/badge/Architecture-0D1117?style=for-the-badge&logoColor=white" alt="Architecture"></a>
<a href="#️-installing-and-updating-the-cli"><img src="https://img.shields.io/badge/Install_CLI-22C55E?style=for-the-badge&logoColor=white" alt="Install CLI"></a>

<br><br>

![Rust](https://img.shields.io/badge/Rust-1.100.0-CE422B?style=flat-square&logo=rust&logoColor=white)
![crate](https://img.shields.io/badge/crate-0.2.0-orange?style=flat-square)
![License](https://img.shields.io/badge/License-Apache_2.0-blue?style=flat-square)
![Platforms](https://img.shields.io/badge/platforms-Linux_x86_64_%7C_macOS-2C5F2E?style=flat-square)

</div>

---

## 📑 Table of Contents

<table>
<tr>
<td valign="top" width="33%">

**Getting Started**
- [✨ Features](#-features)
- [🧠 Architecture](#-architecture)
- [🚀 Quick Start](#-quick-start)
- [🎮 Interactive Workflow](#-interactive-workflow)

</td>
<td valign="top" width="33%">

**Operations**
- [⚙️ Installing and Updating the CLI](#️-installing-and-updating-the-cli)
- [🧱 Project Structure](#-project-structure)
- [🔧 Configuration Matrix](#-configuration-matrix)

</td>
<td valign="top" width="33%">

**Reference**
- [💡 Design Decisions](#-design-decisions--reliability)
- [⚠️ Known Limitations](#️-known-limitations)
- [📜 License & Credits](#-license--credits)

</td>
</tr>
</table>

---

## ✨ Features

<table>
<tr>
<td width="33%" valign="top">

### 🗂️ Topology discovery
`omp-topology list` reads every `topologies/<name>/topology.yml`
in a source tree and reports name, version, and description —
with `--json` for machine-readable output.

</td>
<td width="33%" valign="top">

### 🔍 Plan before you apply
`install`, `remove`, `update`, and `self-update` accept `--dry-run`:
the plan is printed (`would write: ...`). Dry runs skip topology,
registry, and persistent source-cache writes; named-profile resolution
may invoke OMP and create missing profile directories, which the CLI
removes only if empty, on a best-effort basis.

</td>
<td width="33%" valign="top">

### 🔒 Signed installs, fail closed
The bootstrap and `self-update` verify the RSA/SHA-256 signature of
`SHA256SUMS` and the asset checksum before replacing anything.
There is no unsigned fallback.

</td>
</tr>
<tr>
<td width="33%" valign="top">

### 🧩 Config modes that preserve yours
Candidate mode (default) creates an absent active config from the topology.
With a pre-existing or user-edited active config it publishes `config.yml.new`
without rewriting the active file; an installer-created, unedited config can
refresh in place. `--config-merge` (profile or topology precedence) and
`--config-replace` (backup + restore on remove) are explicit.

</td>
<td width="33%" valign="top">

### ♻️ Edit-aware removal
`remove` restores what the topology wrote, but preserves files you
edited after install and config keys you changed — then reports
them, so a retry can finish the job.

</td>
<td width="33%" valign="top">

### 🏷️ Alias registry
Each install is registered under a stable alias (default: the
profile name) in a versioned JSON registry under the XDG state
root. Writes are locked; interrupted runs stay honestly `Partial`.

</td>
</tr>
<tr>
<td width="33%" valign="top">

### 🖥️ Profile-aware targeting
Targets resolve through `omp config path` /
`omp --profile NAME config path`; profiles are discovered from
`~/.omp`, honoring `OMP_PROFILE`/`PI_PROFILE`. Supported
main→profile symlinks are followed with identity checks.

</td>
<td width="33%" valign="top">

### 🔄 In-place updates
`update --alias <name>` re-applies the recorded install against
the current source pin — a no-op when current, with `--ref` and
`--source` overrides for deliberate retargeting.

</td>
<td width="33%" valign="top">

### 📊 Status without side effects
`status` reports manifest health and freshness. GitHub freshness
uses `git ls-remote` only — never a clone or cache write — and an
unresolvable source is reported `unknown`, never guessed.

</td>
</tr>
<tr>
<td width="33%" valign="top">

### 🗄️ Bounded source cache
GitHub sources are pinned to full 40-hex commit SHAs. Per source,
the 10 most recent unreferenced checkouts are retained, and a
checkout in use by a running command is never pruned.

</td>
<td width="33%" valign="top">

### 🤖 Machine-readable output
`--json` on every command emits exactly one JSON document on
stdout. Errors go to stderr as `error: <message>` with exit code
1; clap usage errors exit 2.

</td>
<td width="33%" valign="top">

### 🛡️ Descriptor-anchored filesystem
Every mutation re-walks paths from pinned `openat(O_NOFOLLOW)`
descriptors, so a symlink swapped mid-command fails closed instead
of being followed. It is the crate's only `unsafe` boundary.

</td>
</tr>
</table>

---

## 🧠 Architecture

The CLI is a single Rust crate. `src/main.rs` parses subcommands with clap and
orchestrates; per-topology state lives in a versioned ownership manifest per
agent directory (SHA-256 per installed file, prior config values), and a
cross-agent alias registry lives under the XDG state root. One install request
flows like this:

```
$ omp-topology install -t digital-product-studio \
    --source github:digi4care/omp-topologies --profile digital-product-studio
        │
        ▼
src/main.rs       clap parse; resolve target profile via
                  `omp --profile NAME config path` (argv, never a shell)
        │
        ▼
src/source.rs     resolve github: source → git checkout pinned to a
                  40-hex commit (persistent cache, or ephemeral for --dry-run)
        │
        ▼
src/topology.rs   locate topologies/<name>, parse topology.yml,
                  walk the agent/ overlay (symlinks rejected)
        │
        ▼
src/install.rs    plan every change, refuse collisions, then apply in one
                  transaction with timestamped backups and rollback
        │
        ▼
src/state.rs      write the ownership manifest (files → SHA-256,
                  config prior/installed leaf values)
        │
        ▼
src/registry.rs   stage the alias entry as Partial, run the install under
                  the registry lock, flip it to Installed
```

---

## 🚀 Quick Start

Install the CLI with the signed-release bootstrap (needs `curl`, `openssl`,
`tar`, and `sha256sum`/`shasum`; installs into `~/.local/bin` by default, no sudo):

```sh
curl -fsSL https://raw.githubusercontent.com/digi4care/omp-topologies/main/scripts/install-release.sh | sh
```

> **Release availability.** The bootstrap requires a signed GitHub Release with
> assets, and **no release has been published yet** — check the
> [releases page](https://github.com/digi4care/omp-topologies/releases) before
> running it. Until a signed release exists, run the CLI from a checkout
> instead (`cargo run -- <args>`), as shown below.

Once the CLI is available, discover and install a topology into an OMP profile:

```sh
# List installable topologies in a source (an installed binary has no
# checkout, so pass the source explicitly)
omp-topology list --source github:digi4care/omp-topologies

# Preview the plan; change nothing
omp-topology install -t digital-product-studio \
  --source github:digi4care/omp-topologies \
  --profile digital-product-studio --dry-run

# Apply it, then start OMP with that profile
omp-topology install -t digital-product-studio \
  --source github:digi4care/omp-topologies \
  --profile digital-product-studio
omp --profile digital-product-studio
```

From a checkout of this repository, drop `--source` (the checkout root is the
default source) and prefix commands with `cargo run --`:

```sh
git clone https://github.com/digi4care/omp-topologies.git
cd omp-topologies
cargo run -- list
cargo run -- install -t digital-product-studio --profile digital-product-studio --dry-run
```

More guidance: [topologies/README.md](topologies/README.md) (payload layout and
discovery), [CONTRIBUTING.md](CONTRIBUTING.md) (adding a topology, verification).

---

## 🎮 Interactive Workflow

The day-to-day lifecycle is `list → install → status → update → remove`:

1. **Discover.** `omp-topology list --source github:digi4care/omp-topologies`
   prints each installable topology's name, version, and description as declared
   in its manifest. `omp-topology profiles list` shows the profiles found under
   `~/.omp` and the installs managed under each.
2. **Preview.** `install --dry-run` prints the planned changes (`would write: ...`,
   including paths that go through a supported profile symlink) and exits 0
   without touching the target, the registry, or the cache. Profile resolution
   through `omp --profile NAME config path` can create a missing profile as an
   OMP side effect; explicit `--agent-dir` and ambient `omp config path`
   resolution do not create a target directory.
3. **Install.** The real run applies the overlay in one transaction and
   registers the alias. Re-installing under the same alias is an upsert; an
   alias pointing elsewhere, or a second alias for an already-managed agent
   directory, is refused with an actionable error.
4. **Inspect.** `status --alias <name>` reports what the registry records,
   whether the ownership manifest is healthy, and whether the source has moved
   on (`up to date` / `update available` / `unknown`).
5. **Update.** `update --alias <name>` re-applies the recorded install against
   the current pin; it is a no-op (no clone) when already current.
6. **Remove.** `remove --alias <name>` restores prior config values and deletes
   what the topology added. Files you edited after install are preserved and
   listed, and the removal stays retryable until everything is resolved.

Every command accepts `--json`, which emits exactly one JSON document on stdout;
in human mode the captured command lines are printed unchanged. Failures print
`error: <message>` on stderr and exit 1.

---

## ⚙️ Installing and Updating the CLI

There is no server to deploy — the release workflow builds a single static
Linux binary and native macOS binaries per platform, installed per user and
updated in place.

**From the repository root** (runs the checked-in bootstrap; downloads, verifies, installs):

```sh
# Latest signed release into ~/.local/bin
sh scripts/install-release.sh

# Or pipe it directly:
curl -fsSL https://raw.githubusercontent.com/digi4care/omp-topologies/main/scripts/install-release.sh | sh

# Pinned release and/or custom location
sh scripts/install-release.sh --version v0.2.0 --bin-dir ~/.local/bin
```

The script selects the asset for the current platform (Linux x86_64 static
musl, macOS Intel, macOS Apple Silicon), downloads `SHA256SUMS` and its
detached RSA/SHA-256 signature from the same release, verifies the signature
with the embedded public key, verifies the asset's checksum entry, and only
then replaces the binary — atomically, in the same directory. Verification,
download, and staging failures before the final atomic rename leave the
previous installation untouched. See [RELEASING.md](RELEASING.md) for the
full trust model and manual verification commands.

> Until a signed release is actually published, the bootstrap fails closed and
> installs nothing — check the [releases page](https://github.com/digi4care/omp-topologies/releases)
> first, and run from a checkout (`cargo run -- ...`) in the meantime.

**Self-update** (from an installed CLI; same signature verification):

```sh
omp-topology self-update            # latest release
omp-topology self-update --version 0.2.0 --dry-run   # plan a pinned update
```

**From source** (any checkout; requires a Rust toolchain):

```sh
cargo build --release   # binary at target/release/omp-topology
cargo test --locked     # full local test run
```

The pinned quality gates (`fmt`, `clippy -D warnings`, `cargo test --locked`
in a Docker toolchain) live in [scripts/rust-gates.sh](scripts/rust-gates.sh);
CI runs them on pull requests that change Rust inputs
([.github/workflows/rust-gates.yml](.github/workflows/rust-gates.yml)).

---

## 🧱 Project Structure

```
├── Cargo.toml               # crate manifest: name, version, edition, license
├── src/
│   ├── main.rs              # CLI entry point: subcommands, output, exit codes
│   ├── install.rs           # install transaction: plan, apply, backups, rollback
│   ├── remove.rs            # edit-aware, retryable uninstallation
│   ├── update.rs            # per-alias refresh against the current source pin
│   ├── status.rs            # read-only alias status and freshness
│   ├── profiles.rs          # OMP profile discovery and CLI target resolution
│   ├── registry.rs          # alias → install metadata registry (locked, versioned)
│   ├── state.rs             # XDG state root, ownership manifest, snapshots
│   ├── source.rs            # github:/local source resolution + bounded cache
│   ├── topology.rs          # topology.yml parsing and overlay discovery
│   ├── symlinks.rs          # supported main→profile symlink handling
│   ├── self_update.rs       # signature-verified in-place self-update
│   ├── config_mode.rs       # candidate / merge / replace config handling
│   └── anchored/            # descriptor-anchored FS (the only unsafe boundary)
├── topologies/
│   ├── README.md            # payload layout and discovery contract
│   └── digital-product-studio/   # the installable topology (agent/ + topology.yml)
├── scripts/
│   ├── install-release.sh   # fail-closed signed-release bootstrap
│   └── rust-gates.sh        # pinned fmt/clippy/test gate entry point
├── tests/
│   ├── installer.rs         # installer contract suite
│   ├── cli.rs               # CLI surface tests
│   └── regressions.rs       # regression tests
├── keys/omp-topology-release-public.pem   # public half of the release signing key
├── .github/workflows/       # release.yml (signed release), rust-gates.yml (Rust PR gates),
│                            # topology.yml (changed-topology PR gates)
└── docs/                    # contributor templates and project-wide notes
```

A topology payload is `topologies/<name>/` with a `topology.yml` manifest
(`name`, `version`, `description`, `installable: true`, optional `profile`)
plus an `agent/` overlay; the selected config mode handles `agent/config.yml`
separately. Candidate may publish raw bytes as `config.yml.new`; replacement
installs raw bytes as the active config; merge performs structural merging.
`agent/skills/*` can be routed to a
separate root with `--skills-dir`. The installer rejects symlinks and unsafe
relative paths inside the overlay. The source tree itself is the catalog:
there is no hand-maintained index file.

---

## 🔧 Configuration Matrix

The CLI's configuration surface is its flags plus a few environment variables.
Defaults below are what the code actually applies when the option is absent.

| Variable / flag | Applies to | What it does | Default |
|---|---|---|---|
| `--source` | `list`, `install`, `update` | Topology source: `github:OWNER/REPO`, an HTTPS GitHub URL, or a local path containing `topologies/` | current checkout root (installed binaries must pass it) |
| `--ref` | `list`, `install`, `update` | For list/install, a branch, tag, or full 40-hex SHA requiring a GitHub `--source`. For update, optionally overrides the recorded GitHub source's ref without a new `--source`; invalid with a local source. | the remote's default `HEAD` |
| `--profile` | `install`, `remove`, `status` | Target profile, resolved via `omp --profile NAME config path` (mutually exclusive with `--agent-dir`) | ambient profile from `omp config path` |
| `--agent-dir` | `install`, `remove`, `status` | Explicit target agent directory | — |
| `--alias` | `install`, `remove`, `status`, `update` | Stable registry alias selecting the install | profile name, else lowercased agent-dir basename; `update` requires it |
| `--skills-dir` | `install`, `remove` | Explicit root that receives the overlay's `skills/*` | skills install under the agent directory |
| `--config-merge` + `--config-precedence` | `install`, `update` | Structural merge into the active `config.yml`; `profile` (default) or `topology` wins | candidate mode: create an absent active config; otherwise preserve a pre-existing/user-edited active config and publish `config.yml.new` |
| `--config-replace` | `install`, `update` | Replace the active config; the pre-replacement config is restored on `remove` | candidate mode |
| `--dry-run` | `install`, `remove`, `update`, `self-update` | Print the plan without topology, registry, or persistent source-cache writes; named-profile resolution may invoke OMP and create missing profile directories | off |
| `--json` | all commands | Exactly one JSON document on stdout instead of human lines | human mode |
| `--version` | `self-update` | Release to install | latest published release |
| `OMP_PROFILE`, `PI_PROFILE` | target resolution | Active profile name for the ambient target | `default` |
| `XDG_STATE_HOME` | state location | Root for the registry, manifests, snapshots, and source cache | `~/.local/state/omp-topology` |
| `OMP_TOPOLOGY_BIN_DIR` / `--bin-dir` | `install-release.sh` | Install directory for the CLI binary | `~/.local/bin` |
| `OMP_TOPOLOGY_VERSION` / `--version` | `install-release.sh` | Release tag to install | `latest` |
| `OMP_TOPOLOGY_REPO` | `install-release.sh` | `owner/repo` override for the release source | `digi4care/omp-topologies` |

---

## 💡 Design Decisions / Reliability

- **Fail-closed release trust** — bootstrap and `self-update` verify the
  RSA/SHA-256 signature of `SHA256SUMS`, then the asset's SHA-256 entry;
  verification, download, and staging failures before the final atomic rename
  leave the previous binary untouched. The release workflow signs and
  then re-verifies before publishing, and aborts if the signing secret is
  missing or mismatched. There is no unsigned fallback anywhere.
- **Plan, then apply in one transaction** — planning reads every target exactly
  once and refuses all collisions before anything is mutated; the apply phase
  re-verifies live bytes just before each replacement, with timestamped backups
  and rollback that undoes only what the run changed.
- **Edit-aware removal** — files edited after install and config keys changed
  by the user are preserved and reported, not clobbered; the ownership manifest
  keeps the unresolved state so a later retry finishes the job.
- **One topology per agent directory** — a different topology, conflicting
  alias, or second alias for the same agent directory is refused. Reinstalling
  the same topology under the same alias and target is an upsert.
- **Config modes are never remembered** — `update` and `remove` derive their
  behavior from what the ownership manifest recorded, not from flags a previous
  command happened to pass.
- **Bounded caches and snapshots** — per GitHub source, the 10 most recent
  unreferenced checkouts are retained; per agent, the 10 most recent snapshots
  are normally retained, with recovery-pinned snapshots retained additionally
  until reconciliation succeeds.
  A checkout a running command is using holds a lease that pruning respects.
- **Honest partial state** — a registry entry is staged as `Partial` before the
  install and flipped to `Installed` only after it lands, so an interrupted run
  never claims more than what exists.

---

## ⚠️ Known Limitations

- **No signed release published yet** — the bootstrap and `self-update` require
  a signed GitHub Release with assets. Check the
  [releases page](https://github.com/digi4care/omp-topologies/releases) first;
  until one exists, run the CLI from a checkout (`cargo run -- ...`) and use
  `scripts/install-release.sh` only once a release is available.
- **Platform coverage** — Linux x86_64 only, as a static musl build (no Linux
  aarch64/armv7/glibc-specific builds); macOS Intel and Apple Silicon. No
  Windows builds.
- **External tools** — profile targeting invokes `omp` (`config path`); GitHub
  sources invoke `git`; release installs and self-update need `curl`, `openssl`,
  and `tar`; the bootstrap needs `sha256sum` (Linux) or `shasum` (macOS).
- **One topology per agent directory** — installing a different topology into
  the same agent directory requires removing the current one first.
- **Freshness can be `unknown`** — when a source cannot be resolved (network
  failure, moved or deleted local checkout, removed topology), `status` says so
  instead of guessing.
- **Installed binaries carry no checkout** — outside this repository, pass
  `--source github:digi4care/omp-topologies` explicitly (or run from a checkout).

---

## 📜 License & Credits

Apache License 2.0. See [LICENSE](LICENSE); attribution notes are in
[NOTICE](NOTICE) (Copyright 2026 Chris Engelhard). Release signing and the
trust model are documented in [RELEASING.md](RELEASING.md); the public half of
the release key is committed at
[keys/omp-topology-release-public.pem](keys/omp-topology-release-public.pem).
Built for [OMP](https://github.com/can1357/oh-my-pi) agent configurations.
