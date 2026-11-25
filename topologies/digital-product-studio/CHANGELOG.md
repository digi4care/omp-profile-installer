# Changelog

All notable changes to the `digital-product-studio` topology are documented
here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this topology
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.0.1] - 2026-11-25

### Fixed

- Restored the discovery-approval boundary in the orchestration policy:
  discovery and research proceed without approval, while downstream/build
  work waits for approval of the execution brief.
- Limited brief approval to new or ambiguous product work; direct, bounded
  requests follow the scope-note process without an extra approval step.
- Required explicit user approval of the finalized execution brief before
  work is distributed (product-strategy charter).
- Moved ownership of all documentation, including technical documentation,
  from Engineering to Content.
- Narrowed Engineering's verification boundary: Engineering never writes or
  runs tests; builds and linters run only when the assignment explicitly
  assigns them.
- Defined the QA production-fault handoff: QA isolates the smallest failing
  reproduction and classifies a probable production fault, while Engineering
  owns root-cause diagnosis.
