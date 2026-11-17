# cardinal

A single Rust binary (CLI and per-job runner) that arbitrates local compute resources for parallel coding agents on macOS (arm64), Linux (x64), and Windows (x64).

## Commands

Rust and the development tools are pinned with mise (`mise.toml`, exact versions in `mise.lock`). Run `cargo` through `mise exec --` to get the pinned toolchain.

```sh
mise install
mise exec -- lefthook install
mise exec -- cargo build
mise exec -- cargo test
mise exec -- cargo clippy --all-targets -- -D warnings
mise exec -- cargo fmt --all --check
```

The hooks in `lefthook.yml` check formatting, workflows (actionlint, ghalint), and staged secrets (betterleaks) on commit, the commit message (pommitlint, Conventional Commits) on commit-msg, and run clippy and the tests on push. CI (`.github/workflows/ci.yml`) runs formatting, clippy, and the tests on all three platforms; its `Check` job is the required status check.

## Conventions

- Commit messages follow Conventional Commits.
- Pin every GitHub Action by full commit SHA with a version comment, and add any new action or reusable workflow to `selected_actions.patterns_allowed` in `.github/infra.yaml`.
- `mise.lock` is updated by `.github/workflows/mise-update.yml`, not by Renovate.
