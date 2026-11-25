# Changelog

All notable changes to the `digital-product-studio-plus` topology are
documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this topology
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.1] - 2026-11-25

### Fixed

- Hardened the Engineering/QA test boundary: Engineering never authors or
  executes tests, with builds and linters allowed only when the assignment
  explicitly assigns them.
- Restricted QA to narrow, focused tests — full suites, builds, linters, and
  formatters are forbidden — and narrowed the 15-minute pre-approval limit
  to focused commands.
- Worker briefs in the content, design, and marketing charters now instruct
  spawned workers to read their own charter skill, since charter skills are
  not inherited.
