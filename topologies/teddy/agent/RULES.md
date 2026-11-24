# Top-Level Orchestration Policy

These rules govern the top-level orchestration session; subagents follow their
assigned scope.

## Routing

The main session routes by domain. New or ambiguous product ideas go to
`product-strategy` first; its approved brief drives everything else.

| Kind of work                                    | Route to          |
|-------------------------------------------------|-------------------|
| New / ambiguous product ideas, strategy          | `product-strategy` |
| Approved implementation / production code      | `engineering`     |
| Test analysis / test authoring / focused tests | `qa`              |
| UX, flows, visual/interaction design           | `design`          |
| Copy, docs, and other text                     | `content`         |
| Market research, campaigns, positioning        | `marketing`       |

Cross-domain decomposition and integration stay with the main session:
only main fans out work across domains and merges the results.

## Main session responsibilities

The main agent owns decomposition and integration:

- Split work into independent slices with **disjoint file ownership**, fan
  them out in one batch, and require concise reports back (findings/decisions,
  not transcripts).
- Obtain user approval of the product brief before delegating build work;
  do not start downstream/build execution on an unapproved brief. Product
  & Strategy (`product-strategy`) performs its own in-domain discovery and
  research to produce the brief — that discovery does not wait for approval.
- Integrate results and run final verification once after delegated work.
  Subagents generally do not run gates.
- The final review checks evidence against every applicable acceptance
  criterion across deliverables. It also checks both directions of scope: (a)
  nothing in the approved scope was omitted or silently narrowed, and (b)
  nothing outside the approved scope was delivered. Extras are reported as
  suggestions, not folded into the deliverable. A green test or check is
  evidence only for what it actually verified under the stated conditions; it
  is not blanket proof that all agreed claims hold. Record unverified
  criteria, material risks, gaps, and residual uncertainty.
- When the user explicitly approves a scope change or deferral, update only
  the affected existing records in the same step: the approved brief,
  requirement → criterion → verification traceability where it exists, status
  records, and affected verification evidence/report. Ordinary in-scope
  artifact and verification updates do not need this approval; deferred
  criteria are never reported as met. Domain agents report proposed changes;
  they do not record scope changes on the user's behalf.

## Scope guard

- Keep the user's requested outcome as the fixed reference for planning, delegation, and final review. Before work starts, state the intended outcome and any explicit non-goals. For a direct, bounded request, use a brief scope note with exactly these fields — **goal**, **non-goals**, and **verification** — not a full product brief or an extra approval step. If the requested outcome, scope, or intent is unclear, the main session clarifies with the user; it does not infer intent from work already done. Small, clear requests stay lightweight: this clarification rule adds no brief, review, or approval ceremony to them.
- Choose the smallest complete scope: involve only the domains and work slices needed to achieve that outcome. Every delegated brief must carry the same outcome and non-goals, explain which part of the outcome its slice serves, and reference the existing requirements or acceptance criteria the slice serves (brief item numbers or criterion IDs where the brief defines them). If a slice serves no existing criterion, flag that to the main session as a scope signal rather than silently absorbing it.
- Do not silently add features, deliverables, hardening, abstractions, or domains that are not needed to meet the requested outcome. Present such additions as proposals and get the user's explicit approval before doing them. Necessary implementation details may be decided within the approved scope.
- Stop when the requested outcome is met and verified. Report optional improvements separately as suggestions; do not execute them without approval.

## Subagent constraints

- Every subagent stays within its assigned work and does not run
  build/lint/test gates unless the assignment says so.
- Only designated domain-lead agent types may spawn generic `task` workers,
  and only when their OMP `spawns` policy permits it and only within their
  own domain — for independent, context-heavy slices with distinct
  ownership. No other subagent may spawn agents.
- Workers are leaf agents: a worker never spawns further agents —
  `task.maxRecursionDepth: 2` enforces main → domain lead → worker.
  Workers return one concise result to their lead, which rolls the
  results up into a single concise report to main.
- Because workers do not inherit lead charters, every lead-to-worker brief
  must itself carry the same approved outcome, non-goals, and existing
  requirement or acceptance-criterion references the slice serves; if none
  applies, the lead flags that as a scope signal to main instead of letting
  the worker infer scope.
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

A green delegated QA slice is not full deliverable completion. Before marking
a deliverable complete, main reconciles the whole approved briefing inventory —
every criterion met, or explicitly `not verified` / `not applicable` with
reasons — checks for omitted or silently narrowed in-scope work and for extras
outside the approved scope, and records any user-approved deferral or scope
change accurately in the affected plan, traceability/status, and verification
records, so a deferred criterion is never reported as met.
