# digital-product-studio

Six-domain digital product studio topology for OMP (v2.0.0, per `topology.yml`). The main session routes work across six domain leads — product-strategy, engineering, QA, design, content, marketing — each backed by a lazy charter skill, with subagent spawning bounded by `task.maxRecursionDepth: 2`.

## When to choose it

Choose `digital-product-studio` as the baseline six-domain studio: domain
routing with a scope guard and QA verification rules, without extra planning
gates. Choose [`teddy`](../teddy/) when you want premise challenge and
conditional architecture planning added, or
[`digital-product-studio-plus`](../digital-product-studio-plus/) for traceable
requirements-to-delivery planning with a read-only acceptance-scope advisor.

## Topology files

| File | Role |
|------|------|
| [`topology.yml`](topology.yml) | Topology metadata: name, version, description, install profile |
| [`agent/config.yml`](agent/config.yml) | Agent overlay: eager spawning, auto-parallelism, concurrency cap 4, recursion depth 2, model roles |
| [`agent/RULES.md`](agent/RULES.md) | Top-level orchestration policy: routing table, brief approval, final review, scope guard, subagent constraints, QA verification rules |
| [`agent/agents/`](agent/agents/) | Domain-lead agent definitions (one file per domain) |
| [`agent/skills/`](agent/skills/) | Domain charters, autoloaded per domain lead (one `SKILL.md` per domain) |

## How scope is governed

- The user's requested outcome is the fixed reference for planning, delegation, and final review; before work starts the intended outcome and explicit non-goals are stated — a brief scope note for direct bounded requests, not a full brief or extra approval step (`agent/RULES.md`, `## Scope guard`).
- Every delegated brief carries the same outcome and non-goals and names the part of the outcome its slice serves; additions beyond approved scope are proposals that need explicit user approval.
- The approved briefing is the authoritative full inventory of acceptance criteria; QA reports every criterion and the main session's final review reconciles evidence against all of them.

## Installation

Requires the `omp-topology` CLI (see the repository README for install
instructions):

```sh
# Preview the planned changes first
omp-topology install -t digital-product-studio \
  --source github:digi4care/omp-topologies \
  --profile digital-product-studio --dry-run

# Install for real
omp-topology install -t digital-product-studio \
  --source github:digi4care/omp-topologies \
  --profile digital-product-studio
```

## Limits

- Spawning is bounded to main → domain lead → leaf worker
  (`task.maxRecursionDepth: 2` in `agent/config.yml`).
- Model routes in `agent/config.yml` are machine-specific; adjust them when
  applying the overlay on another machine.
- Worktree isolation applies only when an assignment explicitly requests it
  (`agent/config.yml`).
