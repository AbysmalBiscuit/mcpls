---
applyTo: "crates/mcpls-core/src/config/**,Cargo.toml,crates/*/Cargo.toml"
---

## Type safety and idiomatic config

Config option enums must derive `Default` with `#[default]` on the intended default
variant rather than implementing `Default` manually. This keeps the default co-located
with the type definition and is less error-prone.

Boolean config fields that represent a choice between more than two states should be
`enum`, not `bool`. A `bool` cannot be extended without a breaking change; an enum can
gain variants under `#[non_exhaustive]`.

New `#[serde(default)]` fields must have a test that deserialises a config with the
field omitted and asserts the default value. Cover both JSON and TOML — parsers can
behave differently on edge cases like empty strings and absent vs. null keys.

Enum config options must have a round-trip test for every variant. A typo in
`#[serde(rename_all = "...")]` silently changes the on-disk format and breaks existing
user configs without a compile error.

## MSRV and dependencies

When introducing a new dependency, check whether the functionality is already available
in `std` for the current MSRV. Prefer `std` over external crates to reduce
compile time and supply-chain surface.

A new crate needs no license entry: this fork does not upstream and keeps no allow-list.
What it does need is to be clean under the Security Audit job, which runs
`rustsec/audit-check` over `Cargo.lock`.

If a new stable Rust API (e.g. `OnceLock` at 1.70 replacing `once_cell::sync::OnceCell`)
makes an existing dependency redundant, suggest removing the dependency and bumping
`rust-version` instead. Document the bump in CHANGELOG.
