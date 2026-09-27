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

- **Stellar CLI** (for building optimized WASM and deploying) — see the
  [Stellar CLI install guide](https://developers.stellar.org/docs/tools/developer-tools/cli/install-cli).
- `make` (optional but recommended — the `Makefile` wraps the common commands).

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
make size       # print a WASM size report for all built contracts
make size-check # fail if any contract WASM exceeds the size limit
make doc        # build workspace docs with warnings denied
make audit      # run a security audit of dependencies (cargo-audit)
make check      # fmt + lint + test + check-docs + size-check + doc (run before pushing)
make bench      # hot-path benchmarks
make deploy     # deploy contracts via scripts/deploy.sh
make e2e        # run the end-to-end test suite
make clean      # cargo clean
```

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
- **WASM size**: contract binaries are size-constrained — CI fails any WASM over
  the configured limit. Prefer minimal dependencies and avoid unnecessary
  allocations in hot paths.
- Keep changes focused. A PR should do one thing; split unrelated work.
- Match the style, naming, and structure of the surrounding code.

---

## Testing

Every behavioral change **must** come with tests.

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
