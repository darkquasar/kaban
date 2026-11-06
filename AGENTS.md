# Agent instructions

## Local instructions

If a `REPOLOCAL.md` file exists at the repository root, read it before starting
work. It holds machine-local instructions and is not tracked.

## Rust style

Follow the globally installed Patronus `rust-style-reference` guidance when writing
or reviewing Rust code and Cargo manifests. In Codex, load it through
`rust-style-cx`. Read only the relevant rules, preserve the repository's toolchain,
lint and safety requirements, and avoid unrelated style refactors.
