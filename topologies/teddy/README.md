# Teddy Topology

Six-domain digital product studio for OMP with Teddy-derived planning
practices. The main session routes work across six domain leads, each backed
by a lazy charter skill; subagent spawning is bounded by
`task.maxRecursionDepth: 2` (main → domain lead → leaf worker).

## When to choose it

Choose Teddy when you want the six-domain studio with stronger planning
discipline before build: Product Strategy challenges premises and separates
strategic requirements from technical planning, Engineering plans architecture
only on defined triggers, and QA reviews plan traceability before
implementation. Choose [`digital-product-studio`](../digital-product-studio/)
for the same routing with a lighter scope guard and no planning gates, or
[`digital-product-studio-plus`](../digital-product-studio-plus/) for traceable
requirements-to-delivery planning with a read-only acceptance-scope advisor.

## How it works

- **Routing by domain** — new or ambiguous product ideas go to
  `product-strategy` first; its user-approved brief drives everything else.
  Approved implementation goes to `engineering`, verification to `qa`,
  experience to `design`, text to `content`, market work to `marketing`.
  Cross-domain decomposition and integration stay with the main session.
- **Scope guard** — the user's requested outcome is the fixed reference for
  planning, delegation, and final review; the smallest complete scope is
  chosen, and additions require explicit user approval.
- **Conditional architecture planning** — Engineering enters
  architecture-planning mode only on defined triggers (cross-component seams,
  new boundaries, significant quality attributes, material technical uncertainty,
  hard-to-reverse decisions), then inspects the as-built state and maps requirements
  → components/files → work items → verification. Small, well-understood work skips it.
- **Traceability** — when routed a plan pre-build, QA reviews
  requirement → acceptance → verification traceability; the final review
  reconciles the whole briefing criteria inventory before completion.

## Domains

| Domain lead | Charter skill | Owns |
|---|---|---|
| `product-strategy` | `product-strategy-charter` | Requirements/intent briefs: criteria, scope, non-goals |
| `engineering` | `engineering-charter` | Production code; conditional architecture planning |
| `qa` | `qa-charter` | Verification against approved criteria; focused tests |
| `design` | `design-charter` | UX, flows, visual/interaction design |
| `content` | `content-charter` | Copy, docs, and other text |
| `marketing` | `marketing-charter` | Market research, positioning, campaigns |

## Installation

Requires the `omp-topology` CLI (see the repository README for install
instructions). `topology.yml` defines no default profile, so pass your own:

```sh
# Preview the planned changes first
omp-topology install -t teddy \
  --source github:digi4care/omp-topologies \
  --profile teddy --dry-run

# Install for real
omp-topology install -t teddy \
  --source github:digi4care/omp-topologies \
  --profile teddy
```

## Limits

- Spawning is bounded to main → domain lead → leaf worker
  (`task.maxRecursionDepth: 2`); only domain leads may spawn generic `task`
  workers, and only within their own domain (`agent/RULES.md`).
- Model routes in `agent/config.yml` are machine-specific; adjust them when
  applying the overlay on another machine.
- Worktree isolation applies only when an assignment explicitly requests it
  (`agent/config.yml`).
- Architecture planning is conditional, not a default gate: small,
  well-understood work skips it (`agent/agents/engineering.md`).

## Files

- [`topology.yml`](topology.yml) — topology metadata and description
- [`agent/RULES.md`](agent/RULES.md) — top-level orchestration policy
- [`agent/config.yml`](agent/config.yml) — task and model-role overlay
- [`agent/agents/`](agent/agents/) — domain-lead agent definitions
- [`agent/skills/`](agent/skills/) — autoloaded domain charters
