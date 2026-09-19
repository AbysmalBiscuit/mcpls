---
applyTo: ".github/workflows/**,.cargo/**"
---

## Feature flag parity

Feature flag sets in CI jobs must match the pre-commit commands in `CLAUDE.md` exactly.
Divergence causes "passes locally, fails CI" failures where neither side is obviously
wrong.

The rustdoc job must set `RUSTDOCFLAGS="-D warnings"`. Without it, broken intra-doc
links and missing `///` on public items pass silently in CI.

Nightly toolchain is required only for `cargo fmt`. All other jobs must use stable to
avoid failures caused by nightly regressions unrelated to this project.

## Dependency policy

This fork keeps no license allow-list. It does not upstream, so a dependency's license
is the author's call rather than a gate, and there is no `deny.toml` to add one to.

The Security Audit job runs `rustsec/audit-check` over `Cargo.lock` and must be clean.
An advisory with no fixed version available is the one case worth discussing in the PR
rather than bumping past.

When a newer stable `std` API covers functionality provided by a dependency, suggest
removing the dependency and bumping `rust-version` instead. Fewer dependencies reduce
compile time and supply-chain risk. Document the MSRV bump in CHANGELOG.

## MSRV tracking

`rust-version` in `Cargo.toml` is the enforced minimum. The CI matrix must include a
job that builds and tests on exactly that version to catch accidental use of newer APIs.
Without such a job, MSRV guarantees are not verifiable.
