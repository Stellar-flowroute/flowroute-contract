# Contributing

Thanks for contributing to FlowRoute.

## Setting up the development environment

Follow the Quick Start in the [README](README.md#quick-start): install the Stellar CLI and the Rust nightly toolchain pinned in `rust-toolchain.toml`, including the `wasm32v1-none` target. Run `stellar contract build` once to verify the toolchain works end to end.

## Git workflow

- Never run `git add .`. Stage only the files that belong to the change you are about to commit.
- One commit per logical unit. A fix, a feature, or a documentation section each get their own commit; do not bundle unrelated changes.
- Push immediately after committing so the branch and CI stay up to date.
- Use conventional commit messages: `feat`, `fix`, `refactor`, `docs`, `ci`, `test`, or `chore`, with an optional scope. Examples: `feat(router): implement execute_batch`, `docs: add SECURITY.md`.

## Running tests

Run the whole test suite from the repository root:

```bash
cargo test
```

Build the release wasm, which is what gets deployed:

```bash
stellar contract build
# or
cargo build --target wasm32v1-none --release
```

## Opening a pull request

1. Create a branch from `main` with a short, descriptive name.
2. Make your change, keeping it as small as the task allows.
3. Commit with a conventional message and push the branch.
4. Open a pull request against `main`, describing what changed and why.
5. Make sure the CI checks pass. A maintainer will review and merge.
