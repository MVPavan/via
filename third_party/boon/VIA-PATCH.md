# boon, vendored for VIA

- Upstream: boon 0.6.1 from crates.io (repository
  https://github.com/santhosh-tekuri/boon, commit
  9b7b7bffc2f4fd064baca141e3557849c88e0b4f per `.cargo_vcs_info.json`).
- crates.io checksum (the `Cargo.lock` entry before vendoring):
  `baa187da765010b70370368c49f08244b1ae5cae1d5d33072f76c8cb7112fe3e`.
- This directory is the published `.crate` archive, extracted unchanged and
  checked against that checksum, with this file added. The upstream licence
  files (`LICENSE-MIT`, `LICENSE-APACHE`) are kept.
- The workspace uses it through `[patch.crates-io]` in the root `Cargo.toml`
  and excludes it from the workspace members.

## Patch

None yet: this commit is the unmodified upstream copy.
