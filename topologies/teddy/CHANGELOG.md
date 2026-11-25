# Changelog

All notable changes to the `teddy` topology are documented here. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this topology adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.1] - 2026-11-25

### Fixed

- Made Engineering's verification boundary internally consistent:
  Engineering never runs tests, and builds/linters run only when explicitly
  assigned; the Scout restriction now applies only to broad read-only
  inventory, not to inspecting code paths relevant to an approved briefing.
- Made QA's full-suite ban absolute — full suites are not run, and
  pre-approval applies only to focused commands over 15 minutes — and
  required QA to report out-of-slice acceptance criteria as not verified.
- Product Strategy now requires the user to approve the finalized execution
  brief (success criteria, acceptance criteria, scope, non-goals) after
  choosing a direction and before any work is distributed.
