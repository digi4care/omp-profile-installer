# Topology README template

Copy this template to `topologies/<name>/README.md` when adding a topology,
replacing every `<...>` placeholder. Keep it concise: the README helps a
human choose and install the topology and helps a coding agent orient to its
role, boundaries, and source-of-truth files. Every claim must be verifiable
in the topology's own source (`topology.yml`, `agent/RULES.md`,
`agent/config.yml`, `agent/agents/`, `agent/skills/`) — do not describe
behavior the files do not implement. Delete sections that do not apply rather
than leaving placeholders.

---

# `<Topology Name>` Topology

<One or two sentences: what the topology is and the problem it solves, drawn
from `topology.yml` `description`.>

## When to choose it

<The concrete situations this topology fits, and — just as important — when
to pick a different topology instead. Name the difference from the closest
existing topology so users and agents do not have to diff them.>

## Roles

<Who this is for: the human's role (e.g. product owner approving briefs) and
each agent role the topology defines, with a one-line responsibility each.
Derive roles from `agent/agents/` and `agent/RULES.md`, not aspiration.>

## Routing and coordination

<How work actually flows: which agent receives which kind of task, how
subagents are spawned or bounded (quote real limits such as
`task.maxRecursionDepth` from `agent/config.yml`), and where decomposition or
integration happens. Describe the routing that is implemented, not an ideal.>

## Approval boundaries

<What the topology may do autonomously and what requires explicit user
approval (scope changes, merges, external side effects, …). Cite the rule or
config that enforces each boundary.>

## Installation

Requires the `omp-topology` CLI (see the repository README for install
instructions). Replace `<name>` with the topology slug and `<profile>` with
the profile from `topology.yml` (or your own).

```sh
# Preview the planned changes first
omp-topology install -t <name> \
  --source github:digi4care/omp-topologies \
  --profile <profile> --dry-run

# Install for real
omp-topology install -t <name> \
  --source github:digi4care/omp-topologies \
  --profile <profile>
```

<Note any topology-specific install considerations: required `--profile`,
`--skills-dir`, or follow-up configuration. Omit if none.>

## Limits

<Known boundaries: what the topology deliberately does not do, unverified
behavior, and constraints agents must respect. Honest limits here prevent
misuse; an empty "Limits" section on a non-trivial topology is a smell.>

## Source of truth

| File | Contents |
|---|---|
| `topology.yml` | Manifest: `name`, `version`, `description`, optional `profile` |
| `agent/RULES.md` | Top-level orchestration policy |
| `agent/config.yml` | Task and model-role overlay |
| `agent/agents/` | Agent definitions |
| `agent/skills/` | Charter skills (if present) |

When copied to `topologies/<name>/README.md`, convert each included path to
a relative link and remove rows for files that do not exist.

## Docs

A `topologies/<name>/docs/` directory next to this README is a local-only
scratch area for design records, plans, and decision documents:
`.gitignore` excludes `/topologies/*/docs/`, so it does not ship in a clean
clone. Do not link to that directory from the tracked topology README.

Shared project guidance lives under root `docs/`. Add a repository-relative
link to a specific shared file only when that target exists in the repository;
for example, a topology README could link to
`../../docs/topology-readme-template.md` if that guidance is relevant.
