# Digital Product Studio Plus Topology

Planning-focused variant of the six-domain digital product studio for OMP,
with traceable requirements-to-architecture-to-delivery planning and a
read-only OMP acceptance-scope advisor (per `topology.yml`).

## When to choose it

Choose Plus when you want planning depth that scales with scope, uncertainty,
impact, and reversibility, durable ID-anchored traceability from requirements
through architecture to verification, and an independent read-only advisor
that audits acceptance-scope coverage. Choose
[`digital-product-studio`](../digital-product-studio/) for the lighter
baseline studio, or [`teddy`](../teddy/) for conditional architecture
planning without the advisor and traceability model.

## How it works

- **Domain routing** — the main session routes work across six domain leads
  (product-strategy, engineering, qa, design, content, marketing), each
  backed by a lazy charter skill; cross-domain decomposition and integration
  stay with the main session (`agent/RULES.md`).
- **Proportional planning tiers** — small/known changes get a compact record;
  medium and large/high-risk work get progressively fuller requirements,
  architecture, and traceability artifacts. Depth scales with evidence; no
  full document set is required for every task (`agent/RULES.md`).
- **Approval boundary** — the user approves the product brief before
  downstream work; after that, domain planning and execution proceed without
  per-artifact or per-slice approval. Only material user-owned or
  out-of-scope decisions escalate (`agent/RULES.md`).
- **Traceability and change impact** — durable project docs with stable IDs
  are the source of truth; when a requirement, interface, decision, or
  observed behavior changes, owners append or supersede only affected records
  and preserve history (`agent/RULES.md`).
- **Acceptance-scope advisor** — a read-only OMP advisor (main scope,
  `agent-end`, `read`/`grep`/`glob`) audits final acceptance-scope coverage;
  it does not approve work (`agent/WATCHDOG.yml`, `agent/config.yml`).

## Installation

Requires the `omp-topology` CLI (see the repository README for install
instructions):

```sh
# Preview the planned changes first
omp-topology install -t digital-product-studio-plus \
  --source github:digi4care/omp-topologies \
  --profile digital-product-studio-plus --dry-run

# Install for real
omp-topology install -t digital-product-studio-plus \
  --source github:digi4care/omp-topologies \
  --profile digital-product-studio-plus
```

## Limits

- Spawning is bounded to main → domain lead → leaf worker
  (`task.maxRecursionDepth: 2` in `agent/config.yml`).
- The advisor is configured (`advisor.enabled: true`, `agent/WATCHDOG.yml`),
  but this repository ships no runtime verification of advisor behavior;
  confirm advisor status in your own profile after install.
- Model routes in `agent/config.yml` are machine-specific; adjust them when
  applying the overlay on another machine.
- Worktree isolation applies only when an assignment explicitly requests it
  (`agent/config.yml`).

## Source of truth

- [Topology manifest](topology.yml) — name, profile, `installable: true`.
- [Orchestration rules](agent/RULES.md) — routing, proportional planning
  tiers, main-session responsibilities, worker-brief constraints.
- [Agent config](agent/config.yml) — task concurrency/recursion limits,
  `advisor.enabled: true`, model roles.
- [Advisor roster](agent/WATCHDOG.yml) — read-only acceptance-scope auditor
  (main scope, `agent-end`, `read`/`grep`/`glob`).
- Domain definitions: [agents](agent/agents/) and
  [charter skills](agent/skills/) for Product Strategy, Engineering, QA,
  Design, Content, and Marketing.
