# Project docs

Shared, project-wide guidance and templates for contributors. Topology-specific
design records and dossiers do **not** live here — they belong under
`topologies/<name>/docs/` next to the topology they describe.

## Contents

- [`installer.md`](installer.md) — how the installer works: topologies,
  registry install IDs, profiles, and agent directories; the install/update/
  status/list/remove lifecycle; shell shortcuts via `--alias`; dry runs;
  source, ref, and exact-version selection; the name preflights; ownership
  collisions; and troubleshooting.
- [`v0.3-migration.md`](v0.3-migration.md) — migrating to the v0.3.0 CLI
  contract: registry `--alias` becomes `--install-id`, `--alias` becomes an
  OMP shell shortcut, and JSON `alias` becomes `install_id`.
- [`topology-readme-template.md`](topology-readme-template.md) — the required
  starting point for the README of every new topology. See the
  [Adding a topology](../CONTRIBUTING.md#adding-a-topology) section of
  `CONTRIBUTING.md` for the full checklist (manifest, `agent/` payload rules,
  verification).

## Conventions

- Keep documents here project-wide: they must apply to every topology or to
  the installer/contributor workflow, not to a single topology's behavior.
- Ground every claim in source (`topologies/`, `src/`, `tests/`,
  `scripts/install-release.sh`) and link to it; do not document intended
  behavior that is not implemented.
- Prefer updating an existing document over adding a near-duplicate.
