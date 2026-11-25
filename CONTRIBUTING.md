# Contributing

Contributions should be focused, evidence-backed, and easy to review. Keep changes within the agreed scope; do not combine unrelated fixes or silently broaden a topology's behavior.

## Before changing files

- Read the relevant topology, its `topology.yml`, agent rules, and charter skills. Reuse established conventions.
- For behavior or policy changes, identify the user-visible problem and the acceptance criteria the change must satisfy. Ask the maintainer when scope or a material decision is unclear; do not resolve it by assumption.
- Keep topology-specific changes under `topologies/<name>/`. Update the corresponding `docs/` material when the topology's requirements, behavior, or verification status changes.
- Do not edit generated or unrelated files. Preserve existing user changes and report any unrelated workspace state you encounter.

## Adding a topology

A topology is discoverable by the installer only when
`topologies/<directory>/topology.yml` exists, parses, and sets
`installable: true`. Follow this checklist so a new topology is
installer-compatible and understandable to humans and agents:

1. **Justify it.** State in the PR how the topology differs from every
   existing one — its routing model, roles, or approval boundaries. Do not
   duplicate an existing topology with cosmetic changes; extend the existing
   one instead when the difference is a variant of the same workflow.
2. **Name it.** Use a lowercase kebab-case slug for the directory, and set
   `topology.yml` `name` to the same slug. The manifest `name` is what users
   pass to `-t` and must match the selected topology name.
3. **Write the manifest.** Populate `version` and `description` (the
   installer renders them; fallbacks exist but produce poor listings), set
   `installable: true`, and set `profile` only if the topology needs a
   non-default profile suggestion. Start `version` at `1.0.0` and create the
   topology's own `topologies/<name>/CHANGELOG.md` (see "Versioning topology
   changes" below).
4. **Structure the payload.**
   - `topologies/<name>/agent/` is the only directory the installer copies.
     Nested files are copied recursively; `agent/config.yml` is not copied
     verbatim — it is handled by the installer's config mode. In the
     default candidate mode the existing active config is preserved
     byte-for-byte and the topology's version is published beside it as
     `config.yml.new` (it becomes the active `config.yml` only when no
     config exists). A structural merge into the profile config requires
     an explicit `--config-merge`; a full replacement requires an
     explicit `--config-replace`, which can overwrite unrelated profile
     settings (it backs up first).
   - The installer rejects symlinks and unsafe relative paths anywhere under
     `agent/` — use plain files and directories only.
   - Keep human documentation out of `agent/`: put the README at
     `topologies/<name>/README.md` and longer-lived design or decision
     records under `topologies/<name>/docs/`.
5. **Write the README** from [`docs/topology-readme-template.md`](docs/topology-readme-template.md).
   Every claim must be grounded in the topology's own source files; keep it
   concise — it is an orientation aid, not promotional material.
6. **Verify before opening the PR.**
   - Run the installer contract suite: `cargo test --locked --test installer`.
   - Dry-run the install from a source checkout and inspect the planned file
     set:

     ```sh
     # Default config mode: an existing config is preserved and the
     # topology's config is published as config.yml.new (created as the
     # active config.yml only when none exists). Add --config-merge or
     # --config-replace here if the dry-run should exercise those modes.
     # The /agent basename matters: the default registry alias is the
     # lowercased basename of the agent directory and must match
     # [a-z0-9][a-z0-9-]{0,63}, which a bare mktemp basename (dots,
     # uppercase letters) violates.
     cargo run -- install -t <name> --dry-run --agent-dir "$(mktemp -d)/agent"
     ```

   - Confirm `cargo run -- list` shows the topology with the expected
     name, version, and description.

## Versioning topology changes

Every topology is independently versioned, and the version is load-bearing:
`update` and the CLI's exact `--version` assertion compare against the
`version` declared in the selected source's `topology.yml`. (`status` never
compares versions; it reports source commit/digest freshness.)

- **Every change bumps the version.** Any PR that changes a topology's
  payload or metadata — `agent/` files, `topology.yml` fields, shipped docs —
  must bump that topology's `version` in the same PR. There is no "too small
  to bump" topology change.
- **Bump semantics** (full contract in the repository README's "Topology
  Versioning and Updates" section): `major` for changes to the topology's
  contract with its users (routing model, role boundaries, approval gates,
  removed or renamed agents/skills/config keys); `minor` for additive,
  backward-compatible changes; `patch` for fixes and clarifications that
  leave the contract intact.
- **One changelog per topology.** Each topology keeps its own
  `topologies/<name>/CHANGELOG.md` in Keep-a-Changelog style, with one dated
  section per version describing what actually changed. Do not invent
  features or credit fixes the diff does not contain.
- **Topology versions are not CLI releases.** The `omp-topology` crate
  version and its signed releases move independently of every topology
  version; never couple a topology bump to a CLI release.
- **Verify the update path when it changes.** If a PR changes versioning,
  update, or source-selection behavior, verify `update` against an alias
  that already exists in a real registry — `update --alias <existing>`
  does not create installs. Run `--dry-run` first, inspect the plan, then
  run the real update, and report both under the PR's Verification section.

## Implementation and verification

- Make the smallest complete change that satisfies the agreed scope. Avoid new dependencies and abstractions unless required.
- Choose checks that exercise the changed behavior. The installer contract
  suite lives in `tests/installer.rs`; the focused run for installer and
  topology-discovery changes is:

  ```sh
  cargo test --locked --test installer
  ```

  The full local gate is `cargo test --locked` plus `scripts/rust-gates.sh`
  (the same entry point the Rust gates workflow runs).

- Continuous integration runs one workflow per input domain on every pull
  request, each gated to the files it verifies:

  - `.github/workflows/rust-gates.yml` (check `fmt, clippy, tests`) runs
    `scripts/rust-gates.sh` only when the PR changes Rust inputs (Rust
    sources, Cargo manifests or lockfile, the toolchain file, `.cargo/`,
    or the gate script/workflow itself). On a topology-only PR the job
    succeeds without running the Rust gates.
  - `.github/workflows/topology.yml` (check `topology`) verifies
    `topologies/<slug>/` changes only: it fails on topology payload files
    without a valid manifest, confirms `cargo run --locked -- list --json`
    shows each changed topology's exact name/version/description from its
    `topology.yml`, runs `cargo test --locked --test installer` once, and
    dry-runs each changed topology into a fresh temporary agent directory.
    On a PR with no topology change the job succeeds explicitly without
    running topology checks.

  CI never replaces local verification: run the applicable checks locally
  before opening the PR and report them in the PR description. Both checks
  must pass before merging; a job that skipped its domain gates is not
  evidence for that domain.

- For topology or prompt changes, inspect the resulting files and compare them with the source topology or approved requirements as applicable. Do not claim tests, installation, or runtime behavior were verified unless those checks were actually run.
- In reports and documentation, distinguish verified results from implementation-reported claims, assumptions, pending work, and unverified criteria. A passing narrow check proves only the behavior it exercised.
- Update `CHANGELOG.md` for user-visible changes when appropriate. Do not change versions or release metadata unless the change requires it.

## Pull requests

Use one logical change per PR. Keep the title specific and use the repository's conventional style, for example `fix(installer): preserve config comments` or `docs(teddy): clarify verification status`.

Write the PR description for a reviewer who has not followed the discussion. Include:

1. **Why** — the problem or requirement and relevant context.
2. **Changes** — concrete behavior and important files; call out intentional non-goals.
3. **Verification** — exact commands or review performed and their observed results. Say `Not run` with the reason when checks were not run; never imply a check passed from historical evidence.
4. **Limitations / follow-up** — known gaps, unverified criteria, dependencies on another PR, or pending decisions.

Keep the description aligned with the actual PR diff. For stacked PRs, link the prerequisite and state the merge order. Do not include unrelated branch history as if it were part of the change.

For PRs that change a topology's payload or metadata, the description must also confirm each of the following:

- **Version bumped.** `topologies/<name>/topology.yml` `version` was bumped per SemVer (`major`/`minor`/`patch`) for every topology whose files changed.
- **Changelog entry added.** `topologies/<name>/CHANGELOG.md` has a dated section for the new version describing the actual change — no invented features.
- **Update verified.** If the PR touches versioning, update, or source-selection behavior, `update --alias <existing-alias>` was run against a real existing alias — `--dry-run` first, then the real update — with the observed results reported under Verification.

## Review and merging

- Resolve review feedback with focused changes and update the PR description when scope or evidence changes.
- Do not bypass required checks or branch protection. If direct merge or push is blocked, report the exact required check and use the repository's PR workflow.
- Merge only after the required reviews and checks pass. Do not claim a change is merged to the remote default branch until the merge is visible there.

## Licensing and attribution

This repository is licensed under Apache-2.0; contributions are subject to the terms in `LICENSE`. Preserve existing copyright and `NOTICE` information. Credit third-party projects and authors accurately, with a source link, and distinguish inspiration from code copied or adapted under a license.
