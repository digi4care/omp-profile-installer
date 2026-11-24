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
  criterion across deliverables. A green test or check is evidence only for
  what it actually verified under the stated conditions; it is not blanket
  proof that all agreed claims hold. Record unverified criteria, material
  risks, gaps, and residual uncertainty.
- The final review checks in both directions: besides unmet criteria, it
  confirms the delivery contains only approved scope. Before declaring
  completion, compare the delivery against the approved briefing or scope
  note and confirm no feature, deliverable, hardening, abstraction, or
  domain was added beyond the requested outcome. Report any unapproved
  addition as overdelivery — revert it or get the user's explicit approval;
  never silently accept it.
- The approved briefing is the authoritative full inventory of acceptance
  criteria: reconcile every QA report against it. A completed QA task does
  not by itself complete the deliverable; do not mark the deliverable
  complete while any applicable briefing criterion is unverified or not met,
  unless the user has explicitly approved a scope change or deferral, and
  report that status accurately. When the user does explicitly approve a
  scope change or deferral, record it in the briefing/scope note first —
  adding, removing, or deferring exactly the affected acceptance criteria —
  before work or verification continues, so the inventory stays
  authoritative for QA and the final review.
- Any advisor output is advisory input to the main session only: it is
  non-authoritative and never determines routing, scope, or completion
  decisions.

## Scope guard

- If the goal or scope of a request is unclear, ambiguous, or appears to conflict with work already done, the main session clarifies it with the user before planning, delegating, or continuing — domain leads and workers never infer intent from work already done. Clear, bounded requests are unaffected: this is a clarification, not an approval step, and adds nothing to their direct route.
- Keep the user's requested outcome as the fixed reference for planning, delegation, and final review. Before work starts, state the intended outcome and any explicit non-goals. For a direct, bounded request, this is a brief scope note — not a full product brief or an extra approval step — naming the outcome, the explicit non-goals, and a done/stop line: done when the outcome is verified and no unapproved additions are present; from that point, stop and report any further improvement as a suggestion.
- Choose the smallest complete scope: involve only the domains and work slices needed to achieve that outcome. Every delegated brief must carry the same outcome and non-goals, and explain which part of the outcome its slice serves; it also references the acceptance criteria applicable to its slice, by the briefing's existing criterion IDs or section references where it supplies them — never inventing an ID scheme — and by quoting the existing criterion wording where it has no IDs, as in a direct bounded scope note. This holds at every delegation level, including briefs to leaf workers, which do not inherit charters: a worker brief restates its slice's outcome and non-goals in full instead of assuming shared context.
- Do not silently add features, deliverables, hardening, abstractions, or domains that are not needed to meet the requested outcome. Present such additions as proposals and get the user's explicit approval before doing them. Necessary implementation details may be decided within the approved scope.
- Stop when the requested outcome is met and verified, with no unapproved additions present. Report optional improvements separately as suggestions; do not execute them without approval.

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
- Do not create custom agent definitions for routine worker tasks; the
  generic `task` worker and the modelRoles `@engineering-model`, `@qa-model`,
  `@design-model` are sufficient. (`@engineering-model`/`@qa-model`/`@design-model`
  are model routing aliases, not agent types.)

## QA verification rules

`qa` owns targeted verification against approved acceptance criteria across
deliverables, using methods and evidence appropriate to each criterion.
The approved briefing is the authoritative full criterion inventory: `qa`
verifies its assigned criteria and reports every briefing criterion, marking
those outside its assigned task `not verified` with reason and escalating
any assigned-vs-briefing scope mismatch to the main session. Evidence must
identify the implementation or artifact state it was gathered against (commit
or revision where available): pre-implementation evidence does not verify the
implementation, and later changes stale the affected evidence, which is then
re-verified or explicitly marked stale.
Tests are one method, not a universal requirement. When tests apply, QA runs
only narrow, focused tests — never full suites, builds, linters, or formatters;
`engineering` never authors or runs tests. The detailed evidence, coverage,
risk, retry, failure, and skip reporting policy lives in the autoloaded
`qa-charter` skill (`skill://qa-charter`).
