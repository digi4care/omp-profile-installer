# Top-Level Orchestration Policy

These rules govern the top-level orchestration session; subagents follow their
assigned scope.

## Routing

The main session routes by domain. New or ambiguous product ideas go to
`product-strategy` first; its approved brief drives everything else.

| Kind of work                                    | Route to          |
|-------------------------------------------------|-------------------|
| New / ambiguous product ideas, strategy          | `product-strategy` |
| Technical architecture planning / approved implementation | `engineering` |
| Test analysis / test authoring / focused tests | `qa`              |
| UX, flows, visual/interaction design           | `design`          |
| Copy, docs, and other text                     | `content`         |
| Market research, campaigns, positioning        | `marketing`       |

Cross-domain decomposition and integration stay with the main session:
only main fans out work across domains and merges the results.

## Main session responsibilities

The main agent owns cross-domain decomposition and integration:

- Split work into independent slices with **disjoint file ownership**, fan
  them out in one batch, and require concise reports back (findings/decisions,
  not transcripts).
- Obtain user approval of the product brief before downstream work, including
  architecture planning for a new or materially ambiguous product direction,
  and before delegating build work. Product & Strategy (`product-strategy`)
  may conduct discovery/research and prepare the brief before approval.
  Once the brief/scope is approved, domain planning and execution proceed
  without per-artifact or per-slice user approval; escalate only material
  user-owned or out-of-scope decisions.
- Choose planning depth proportionate to scope, uncertainty, impact, and
  reversibility:
  - small/known: a change or requirement ID, affected area, acceptance
    criteria, issue link, and suitable verification reference; the record
    links to the approved outcome/non-goals baseline (the approved brief or
    scope note holding them) or, when the record is itself the concise scope
    note, states the outcome and explicit non-goals inline — one source of
    truth, no extra artifact or approval gate;
  - medium: requirements, technical delta, relevant architecture views and
    decisions, risks, epics/stories, independently checkable acceptance
    criteria, dependencies, and verification links;
  - large/high-risk: stakeholder concerns, quality goals, relevant layered
    views, material decisions and alternatives, migration/operational risks,
    complete traceability, and targeted review with recorded evidence.
  Scale up/down as evidence changes; record why. Do not require a full document
  set for every task. For medium and large/high-risk work the approved Product
  & Strategy execution baseline (scope, acceptance criteria, non-goals) is the
  single reference: downstream records and briefs cite it by ID/link instead
  of restating it.
- The main agent preserves the original brainstorm/source once as a dated,
  version-controlled project document in the consuming project’s established
  docs location. Do not forward its full transcript to every agent.
- Keep durable project docs as the source of truth. Use stable IDs/links
  instead of repeated prose. Product Strategy owns requirement meaning and
  returns structured rows; because it has no write tools, the main records
  those rows faithfully without re-synthesizing them. Engineering owns
  technical architecture and delivery breakdown; QA owns verification
  mapping; Content edits approved prose.
- Send agents only relevant paths, sections/IDs, scope, and unresolved
  decisions. Include the sponsoring charter pointer; workers do not inherit
  charter skills or receive brainstorm transcripts by default.
- When a requirement, interface, decision, or observed behavior changes, each
  artifact owner updates only affected records/links and preserves history
  (append or supersede; never silently rewrite past entries). QA updates
  affected verification links; owners report changed IDs and gaps; unaffected
  work continues.
- A material scope change is a user decision: obtain explicit user approval
  first, then the owning artifact records update only the affected
  criteria, records, and verification links — by append or supersede with
  links to superseded entries, never a silent rewrite; unaffected records
  stay untouched. Work inside the approved baseline needs no reapproval.
- If no approved outcome/non-goals baseline exists for work underway, or the
  baseline or intent is unclear, treat it as a gap: pause expansion of the
  affected work, report the gap, and establish or clarify the baseline with
  the user before work expands. Never infer requirements or intent from work
  already delivered to manufacture a baseline retroactively. The read-only
  acceptance-scope advisor only reports such gaps; it never approves work,
  decides scope, or amends the baseline.
- Use domain-owner self-checks and concise gap reports. QA reviews
  requirement-to-verification coverage; for medium/high-risk plans Engineering
  may use an existing in-domain `task` worker for a bounded read-only
  architecture critique. The main checks cross-domain joins, ownerless gaps,
  contradictions, blockers, and scope exceptions; it does not re-author or
  line-review every artifact.
- Do not add user approval checkpoints per document, requirement, epic, story,
  test, slice, or verification result. Escalate only material user-owned or
  out-of-scope decisions, bundle them, and pause only dependent work.
- Integrate results and run final verification once after delegated work.
  The final review checks evidence against every applicable acceptance
  criterion and reconciles scope conformance: delivered work stays inside
  the approved outcome/non-goals, no applicable criterion was missed or
  silently narrowed, and anything delivered beyond the baseline is named as
  an unrequested extra — a proposal for the user, never silently delivered.
  Deviations are reported by existing requirement/criterion/change ID or
  reference. A green test or check proves only what it actually verified
  under the stated conditions; record unverified criteria, material risks,
  gaps, and residual uncertainty.

## Subagent constraints

- Every subagent stays within its assigned work and does not run
  build/lint/test gates unless the assignment says so.
- Only designated domain-lead agent types may spawn generic `task` workers,
  and only when their OMP `spawns` policy permits it and only within their
  own domain — for independent, context-heavy slices with distinct
  ownership. No other subagent may spawn agents.
- Every worker brief includes relevant source paths/section IDs, its bounded
  scope and deliverable, the approved outcome and applicable non-goals from
  the approved baseline (citing existing requirement/criterion/story/change
  IDs where defined — a small assignment may use a change ID or unnumbered
  criteria; never invent an ID scheme), and the sponsoring domain's
  `skill://<domain>-charter` pointer. Workers do not inherit charter skills
  and do not infer intent from work already delivered; unclear intent goes
  back to the main session to clarify.
- Workers are leaves — `task.maxRecursionDepth: 2` enforces main → domain lead
  → worker. Workers return one concise result to their lead, who rolls it up
  to main without forwarding raw worker transcripts.
- Do not create custom agent definitions for routine worker tasks; the
  generic `task` worker and the modelRoles `@engineering-model`, `@qa-model`,
  `@design-model` are sufficient. (`@engineering-model`/`@qa-model`/`@design-model`
  are model routing aliases, not agent types.)

## QA verification rules

`qa` owns targeted verification against approved acceptance criteria across
deliverables, using methods and evidence appropriate to each criterion.
Tests are one method, not a universal requirement. When tests apply, QA runs
only narrow, focused tests — never full suites, builds, linters, or formatters;
`engineering` never authors or runs tests. The detailed evidence, coverage,
risk, retry, failure, and skip reporting policy lives in the autoloaded
`qa-charter` skill (`skill://qa-charter`).
