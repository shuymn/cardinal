# cardinal

A single Rust binary (CLI and per-job runner) that arbitrates local compute resources for parallel coding agents on macOS (arm64), Linux (x64), and Windows (x64).

## Platforms

Develop on macOS or Linux. Windows (x64) is a supported runtime target, built and tested in CI, but not a development environment: the development tools and hooks are not set up for it.

## Commands

Rust and the development tools are pinned with mise (`mise.toml`, exact versions in `mise.lock`). Run `cargo` through `mise exec --` to get the pinned toolchain.

```sh
mise install
mise exec -- lefthook install
mise exec -- cargo build
mise exec -- scripts/test
```

`scripts/test` is the completion check of every change: formatting, clippy with `-D warnings`, and the tests, run by cargo-nextest from a constructed environment with each test process confined to a per-run temp root, between a snapshot and a verify of `scripts/repo-guard`, which fails the run when the tests changed the repository they run in; processes a test leaves behind fail the run too (`docs/architecture.md` section 13.4). `scripts/test --self-check` shows each of these mechanisms holding. The hooks in `lefthook.yml` check formatting, workflows (actionlint, ghalint), and staged secrets (betterleaks) on commit, the commit message (pommitlint, Conventional Commits) on commit-msg, and run `scripts/test` on push. CI (`.github/workflows/ci.yml`) runs `scripts/test`, its self-check, and a run with `BASH_ENV` and decoy `GIT_DIR`, `GIT_WORK_TREE`, and `GIT_INDEX_FILE` exported, on all three platforms; its `Check` job is the required status check.

## Conventions

- Commit messages follow Conventional Commits.
- `docs/architecture.md` fixes the module layout, contracts, and test strategy; `tests/layering.rs` enforces its module dependency table.
- Tests start processes and run git only through `crates/test-support` (`Scope::command`, typed `Fixture` operations); `clippy.toml` rejects any other spawn, and a product spawn site carries `#[expect(clippy::disallowed_methods, reason = "…")]`.
- Pin every GitHub Action by full commit SHA with a version comment, and add any new action or reusable workflow to `selected_actions.patterns_allowed` in `.github/infra.yaml`.
- `mise.lock` is updated by `.github/workflows/mise-update.yml`, not by Renovate.
