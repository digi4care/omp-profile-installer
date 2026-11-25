# Project docs

Shared, project-wide guidance and templates for contributors. Topology-specific
design records and dossiers do **not** live here — they belong under
`topologies/<name>/docs/` next to the topology they describe.

## Contents

- [`installer.md`](installer.md) — how the installer works: topologies,
  registry aliases, profiles, and agent directories; the install/update/
  status/list/remove lifecycle; dry runs; source, ref, and exact-version
  selection; the alias name preflight; ownership collisions; and
  troubleshooting.
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
