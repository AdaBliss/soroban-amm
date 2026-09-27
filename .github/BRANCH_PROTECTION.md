# Main branch protection

The `main` branch is protected and should accept changes only through reviewed pull requests.

Maintainers should configure the repository with the following settings:

- Require a pull request before merging, with at least one approving review.
- Require CODEOWNERS review for changes under `.github/`, `contracts/`, and release-sensitive paths.
- Dismiss stale approvals when new commits are pushed.
- Require branches to be up to date before merging.
- Require conversation resolution and prevent force pushes or branch deletion.
- Do not permit direct pushes to `main`, including administrator bypasses except for emergency recovery.
- Require the following status checks to pass before merging (see [CI workflow](workflows/ci.yml)):
  - `build-and-test`
  - `go-sdk`
  - `python-examples`
  - `npm-packages (dir: packages/sdk)`
  - `npm-packages (dir: packages/ui-components)`
  - `npm-packages (dir: packages/ts-advanced-client)`
  - `npm-packages (dir: services/graphql-api)`
  - `npm-packages (dir: services/webhook-streamer)`
  - `npm-packages (dir: services/health-dashboard)`
  - `npm-packages (dir: examples/client)`

  > **Keeping this list in sync with `ci.yml`**
  > Every job name and matrix entry above must match the names in `.github/workflows/ci.yml` verbatim.
  > If jobs are added, removed, or renamed, update this list in the same PR.

## Testnet Smoke Test

The [Testnet Smoke Test](workflows/smoke-test.yml) workflow is **not** a required PR check.
It needs the `TESTNET_SECRET_KEY` secret to sign real testnet transactions, so it cannot run on pull
requests or from forks. Instead it runs:

- **nightly** (04:17 UTC),
- on **pushes to `main`** that touch contracts, `scripts/deploy*`, `scripts/e2e*`, the Cargo
  manifests or the toolchain,
- on every **published release**,
- and on demand via `workflow_dispatch`.

Before deploying anything it runs `scripts/e2e/preflight.sh`, which checks RPC health, re-creates the
smoke-test account through friendbot if a testnet reset wiped it, and verifies the account balance.
Each run's summary labels a failure as either an **infrastructure failure** (RPC outage, rate limit,
friendbot, underfunded or missing account, CI setup) or a **protocol regression**.

Failures from scheduled, push and release runs open an issue labelled `smoke-test-failure` (or comment
on the open one), and the next passing run closes it. If the account runs low, top it up or rotate
`TESTNET_SECRET_KEY` to a fresh key: friendbot only funds accounts that do not exist yet, and the next
run creates the new one automatically.

## Settings drift

These settings encode the policy described in `CONTRIBUTING.md`; repository administrators should
periodically verify that the configured rules have not drifted. Any time the CI workflow's job names
or matrix entries change, update the required-check list above and confirm the branch-protection
settings still reference valid check names.
