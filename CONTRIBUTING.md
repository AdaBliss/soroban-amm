# Contributing to Soroban AMM

Thanks for your interest in contributing! Soroban AMM is an open-source AMM
protocol built on Stellar's [Soroban](https://developers.stellar.org/docs/build/smart-contracts/overview)
smart contract platform. This guide explains how to get set up, the standards we
hold contributions to, and how to get a change merged.

This is a **security-critical DeFi protocol**. Correctness and test coverage
matter more than speed — please read this guide before opening a pull request.

---

## Table of Contents

- [Ways to Contribute](#ways-to-contribute)
- [Good First Issues](#good-first-issues)
- [Prerequisites](#prerequisites)
- [Project Layout](#project-layout)
- [Local Development](#local-development)
- [Coding Standards](#coding-standards)
- [Testing](#testing)
- [Releasing contracts](#releasing-contracts)
- [Pull Request Process](#pull-request-process)
- [Commit Messages](#commit-messages)
- [Reporting Bugs & Security Issues](#reporting-bugs--security-issues)
- [Code of Conduct](#code-of-conduct)
- [License](#license)

---

## Ways to Contribute

- **Development** — implement features, fix bugs, and help complete the V3-style
  concentrated liquidity engine, router, and aggregator contracts.
- **Testing** — add unit tests, extend the property-based fuzz suite
  (`amm-fuzz`), and harden edge-case coverage.
- **Documentation** — improve the README, contract docs, and client examples so
  other Stellar teams can build on the protocol.
- **Research** — concentrated-liquidity math, tick pricing, TWAP oracle design,
  and gas/size optimization.
- **Design** — UI/UX for the planned web frontend (swap, liquidity, and
  governance dashboards).

## Good First Issues

New to the project? Look for issues labeled
[`good first issue`](https://github.com/promisszn/soroban-amm/labels/good%20first%20issue)
and [`help wanted`](https://github.com/promisszn/soroban-amm/labels/help%20wanted).
These are scoped so you can make a meaningful contribution without needing deep
knowledge of the entire codebase. If nothing fits, open an issue describing what
you'd like to work on and we'll help you find a starting point.

---

## Prerequisites

- **Rust** — install via [rustup](https://rustup.rs/). Nothing else to do: the
  exact version, the `wasm32v1-none` target and the `clippy`/`rustfmt`
  components are all pinned in `rust-toolchain.toml`, and rustup installs them
  on your first `cargo` command in this repo. CI reads the same file, so a
  green run locally means the same compiler ran in CI.

    To move the toolchain, bump `channel` in that file in its own PR rather
    than upgrading locally — otherwise your build and CI's diverge.

- **Stellar CLI** (for building optimized WASM and deploying) — the exact
  version is pinned in [`.stellar-version`](.stellar-version) and is read by the
  `Dockerfile`. Install that version rather than the latest release:

    ```bash
    cargo install --locked stellar-cli --version "$(cat .stellar-version)"
    ```

    The pin is deliberate — see
    [Why the Stellar CLI version is pinned](#why-the-stellar-cli-version-is-pinned).
    To move it, bump the single line in `.stellar-version` in its own PR; the
    container follows from it, and `release.yml` pins the same version
    through the `stellar/stellar-cli` action.
- `make` (optional but recommended — the `Makefile` wraps the common commands).

### Why the Stellar CLI version is pinned

`stellar contract optimize` turns the workspace WASM into the artifacts published
in a GitHub release, so the CLI version decides the bytes users deploy. It used
to be written out in three places — `Dockerfile` (25.1.0), `release.yml`
(23.0.0) and `smoke-test.yml` (27.1.0) — so the optimizer that produced a
release artifact and the CLI the smoke test deployed it with were four major
versions apart, and `stellar contract optimize` output changing between them
would have gone unnoticed. That is the same hazard `rust-toolchain.toml` guards
against for the compiler, so the CLI version now lives in one file that the
container reads, with `release.yml` pinning the same version explicitly.

The version is chosen deliberately rather than by taking the newest release.
`27.1.0` is what this image can build: `--locked` compiles the CLI's own
`Cargo.lock` with the toolchain above, and `25.1.0` locks `ethnum 1.5.2`,
which Rust 1.98.1 rejects with E0512 (`cannot transmute between types of
different sizes`). `27.1.0` locks `ethnum 1.5.3`, the version this workspace
already builds with, so the container, the release optimizer and the smoke
test all agree on a CLI that compiles here.

---

## Project Layout

The repository is a Cargo workspace. Each contract lives under `contracts/`:

| Crate                                                              | Purpose                                     |
| ------------------------------------------------------------------ | ------------------------------------------- |
| `amm`                                                              | V2 constant-product AMM pool                |
| `token`                                                            | SEP-41 LP token                             |
| `factory`                                                          | Pool factory and registry                   |
| `governance`                                                       | On-chain LP-governed governance             |
| `twap_consumer`                                                    | TWAP oracle consumer                        |
| `concentrated_liquidity`                                           | V3-style tick-based AMM (in progress)       |
| `router`, `dex_aggregator`, `batch_router`, `batch_auction`        | Routing & execution                         |
| `staking`, `incentive_campaigns`, `pol_vesting`, `reserve_manager` | Protocol economics                          |
| `amm-fuzz`                                                         | Property-based invariant fuzzing            |
| `integration-tests`                                                | Cross-contract integration tests            |
| `amm-sdk`                                                          | Rust SDK for interacting with the contracts |

Supporting directories: `scripts/` (deploy & e2e), `benches/` (hot-path
benchmarks), `examples/` (TypeScript & Python clients), and `docs/`.

---

## Local Development

Common tasks are wrapped in the `Makefile`:

```bash
make help       # list every target with a one-line description (default goal)
make build      # cargo build --release --target wasm32v1-none
make optimize   # optimize every WASM artifact from `make build` via the Stellar CLI
make test       # build, then run the workspace test suite
make test-all   # run tests for the whole workspace
make fmt        # cargo fmt --all
make lint       # cargo clippy --all -- -D warnings
make check-docs # verify docs/error-codes.md matches #[contracterror] enums
make abi       # regenerate docs/abi.json from deployable contract WASM
make check-abi # check docs/abi.json against committed contract interfaces
make check-deploy-scripts # fail if a contract has no scripts/deploy/ module
make size       # print a WASM size report for all built contracts
make size-check # fail if any contract WASM exceeds the size limit
make doc        # build workspace docs with warnings denied
make audit      # run a security audit of dependencies (cargo-audit)
make check      # fmt + lint + test + check-docs + check-deploy-scripts + size-check + doc (run before pushing)
make bench      # hot-path benchmarks
make deploy     # deploy contracts via scripts/deploy.sh
make e2e        # run the end-to-end test suite
make clean      # cargo clean
```

### Contract interfaces and ABI

When changing a public contract interface, regenerate `docs/abi.json` with
`make abi` and include the updated file in the same pull request. CI runs
`make check-abi` to catch stale ABI documentation.

Before opening a PR, run:

```bash
make check
```

This is intended to track the same set of checks CI enforces in
`.github/workflows/ci.yml`, so passing it locally means your PR should pass CI.

---

## Coding Standards

- **Formatting** is enforced by `rustfmt` using the repo's `rustfmt.toml`
  (edition 2021, 100-column width, crate-granularity imports). Run
  `make fmt` before committing.
- **Linting**: `cargo clippy` must pass with **no warnings** (`-D warnings`).
- **JavaScript/TypeScript linting**: every JS workspace (`packages/sdk`,
  `packages/ui-components`, `packages/ts-advanced-client`, `services/*`,
  `examples/client`) is linted by the shared `eslint.config.mjs` at the repo
  root, which extends typescript-eslint's `recommended-type-checked` set, and
  CI fails on any error. Run `npm ci` once at the root and in the workspace,
  build it, then `npm run lint` there (or `make lint-js` for all of them).
  Rules are errors or deliberately off with a reason in the config; there is
  no warn-only tier.
- **Node version**: `.nvmrc` pins the Node major that CI runs every JS package
  on. Each package declares it as `engines.node`, and any `@types/node`
  dependency uses the same major, so `tsc` cannot accept an API the CI runtime
  lacks. `make check-node-versions` (run in CI) fails on any mismatch. Moving to
  a new Node major means changing `.nvmrc`, every `@types/node` and every
  `engines.node` together, in one PR, with the lockfiles regenerated.
- **WASM size**: contract binaries are size-constrained — CI fails any WASM over
  the configured limit. Prefer minimal dependencies and avoid unnecessary
  allocations in hot paths.
- Keep changes focused. A PR should do one thing; split unrelated work.
- Match the style, naming, and structure of the surrounding code.

---

## Testing

Every behavioral change **must** come with tests.

### Local artifacts and repository hygiene

Soroban `test_snapshots/` directories are generated local artifacts, not test
fixtures. Tests may create or rewrite them, but do not commit them. All such
directories are ignored, along with `.soroban-amm.deploy.env` and
`optimized-artifacts/`. To stop tracking an accidentally committed artifact
without deleting your local copy, use `git rm -r --cached -- <path>` and commit
the removal.

CI runs `bash scripts/check_repo_hygiene.sh tracked` before building/testing and
`bash scripts/check_repo_hygiene.sh clean` afterward. The first rejects tracked
snapshots, `.DS_Store`, `__pycache__` and `.pyc` artifacts; the second requires
`git status --porcelain` to be empty, including unignored new files. Tests must
not depend on or rewrite tracked generated output. Run the guard's regression
tests with `bash scripts/check_repo_hygiene.test.sh`.

For an existing clone, `git gc` can compact Git's object storage without
rewriting history. It cannot remove snapshot blobs still reachable in old
commits, and it does not delete ignored local snapshots. Untracking snapshots
therefore reduces future checkouts and diffs, but does not promise an immediate
reduction of historical clone size. Do not rewrite published history to purge
these artifacts.

- **Unit tests** live alongside each contract's `src/`.
- **Property-based tests** for pool invariants (swap, fee accounting, liquidity
  math) live in `amm-fuzz` — extend these when touching core AMM math.
- **Integration tests** across contracts live in `integration-tests`.

Run the full suite:

```bash
make test          # or: cargo test --workspace
```

Run a single crate's tests:

```bash
cargo test -p factory
```

For security-sensitive changes (swap math, fee logic, authorization, flash
loans), describe in your PR how you verified correctness and what invariants you
checked. See [`AUDIT.md`](AUDIT.md) for the security properties we track.

---

## Releasing contracts

Release changes through a pull request to `main`. The PR must add a dated
Keep a Changelog section for the exact version heading (for example,
`## [v1.2.3] - 2026-10-21`) and update the `[Unreleased]` section as needed.
The release workflow uses only that version's section as the GitHub release
body; it does not modify or upload a temporary changelog.

After the PR is merged, create and push a `v*` tag on the merged commit. The
workflow confirms that commit is reachable from `main`, builds with the pinned
Rust toolchain and locked Cargo dependencies, runs the Rust and ABI/documentation
gates, and checks the exact deployable WASM set. It then creates a draft release,
downloads and verifies the draft assets, and runs the testnet e2e smoke suite
against those downloaded WASM files. The release is published only if the smoke
test passes. The published assets include the contract WASM files,
`sha256sums.txt`, and `manifest.json`; GitHub build provenance is attested for
the WASM files.

To validate a release candidate from **Actions → Release contracts → Run
workflow**, provide the version tag and leave **dry_run** enabled. The workflow
runs the same gates, creates an unpublished draft, downloads and verifies its
assets, and runs the testnet smoke test against them. It leaves the draft
unpublished and uploads the assets and notes as a workflow artifact. Use a
commit reachable from `main` with its version section in `CHANGELOG.md`. Link
the Actions run and its artifact from the release pull request.

---

## Pull Request Process

1. **Fork** the repository and create a branch from `main`
   (`fix/short-description` or `feat/short-description`).
2. Make your change, add tests, and run `make check` locally.
3. **Open a pull request** against `main` with a clear description of _what_
   changed and _why_. Link any related issue.
4. Ensure **CI passes** — build, WASM size checks, clippy, formatting, and the
   test suite all run automatically on every PR.
5. A **maintainer reviews and approves** before merge. Direct pushes to `main`
   are not permitted; all changes land via reviewed PR.
6. First-time contributors receive additional review. Please be responsive to
   feedback — we're happy to help you get a change over the line.

---

## Commit Messages

Use clear, conventional-style prefixes where possible:

- `feat:` a new feature
- `fix:` a bug fix
- `test:` adding or improving tests
- `docs:` documentation only
- `refactor:` code change that neither fixes a bug nor adds a feature
- `chore:` tooling, CI, or housekeeping

Keep the subject line under ~72 characters and explain the reasoning in the body
when the change isn't obvious.

---

## Reporting Bugs & Security Issues

- **Non-sensitive bugs**: open a GitHub issue with steps to reproduce, expected
  vs. actual behavior, and the affected contract.
- **Security vulnerabilities**: do **not** open a public issue. Follow the
  responsible-disclosure process in [`SECURITY.md`](SECURITY.md).

---

## Code of Conduct

Be respectful and constructive. Harassment, discrimination, or abusive behavior
will not be tolerated. Maintainers may remove comments, commits, or contributors
that violate this standard.

---

## License

By contributing, you agree that your contributions will be licensed under the
[MIT License](LICENSE) that covers this project.

