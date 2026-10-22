# Main branch protection

The `main` branch is protected and should accept changes only through reviewed pull requests.

Maintainers should configure the repository with the following settings:

- Require a pull request before merging, with at least one approving review.
- Require CODEOWNERS review for changes under `.github/`, `contracts/`, and release-sensitive paths.
- Dismiss stale approvals when new commits are pushed.
- Require branches to be up to date before merging.
- Require conversation resolution and prevent force pushes or branch deletion.
- Do not permit direct pushes to `main`, including administrator bypasses except for emergency recovery.
- Require the status checks listed below to pass before merging (see [CI workflow](workflows/ci.yml)).

## Required status checks (contexts)

The following checks are reported by the CI workflow and must be marked as **required** on `main`.

This list is generated from `.github/workflows/ci.yml` and is verified by
`make check-branch-protection` (`scripts/check_branch_protection_doc.mjs`). Do not edit the
list by hand without running the checker.

<!-- begin: required-checks (generated from ci.yml) ** DO NOT EDIT MANUALLY ** -->
- `build-and-test`
- `fuzz`
- `go-sdk`
- `node-versions`
- `docker-build`
- `python-examples`
- `Security audit`
- `npm-packages (packages/sdk)`
- `npm-packages (packages/ui-components)`
- `npm-packages (packages/ts-advanced-client)`
- `npm-packages (services/graphql-api)`
- `npm-packages (services/webhook-streamer)`
- `npm-packages (services/health-dashboard)`
- `npm-packages (examples/client)`
<!-- end: required-checks -->

Notes:

- Matrix jobs report one context per matrix entry, formed from the job's explicit
  `name:` and the matrix values. Test commands are deliberately **not** part of the
  context name, so changing a test command does not silently drop the requirement.
- `Security audit` is a separate check run created by the `rustsec/audit-check`
  step in the `audit` job. It is reported under that name, not the job id.
- The [Testnet Smoke Test](workflows/smoke-test.yml) workflow is **advisory** and
  must **not** be marked required (see below).

## Advisory (not required) checks

- Testnet Smoke Test (`Testnet Smoke Test`) -- runs nightly, on pushes to `main`,
  on releases and on demand; it needs the `TESTNET_SECRET_KEY` secret and cannot run
  on forks.

## Applying the protection rule

The payload below is the copy-pasteable source of truth for the required checks.
It is generated from the same list as the checker and must be regenerated when `ci.yml`
changes. Replace `promisszn/soroban-amm` with the repository slug if you forked.

```bash
gh api -X PUT repos/promisszn/soroban-amm/branches/main/protection \
  -H "Accept: application/vnd.github+json" \
  --input - <<'JSON'
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "build-and-test",
      "fuzz",
      "go-sdk",
      "node-versions",
      "docker-build",
      "python-examples",
      "Security audit",
      "npm-packages (packages/sdk)",
      "npm-packages (packages/ui-components)",
      "npm-packages (packages/ts-advanced-client)",
      "npm-packages (services/graphql-api)",
      "npm-packages (services/webhook-streamer)",
      "npm-packages (services/health-dashboard)",
      "npm-packages (examples/client)"
    ]
  },
  "enforce_admins": true,
  "required_pull_request_reviews": {
    "require_code_owner_reviews": true,
    "dismiss_stale_reviews": true,
    "require_last_push_approval": false
  },
  "required_conversation_resolution": true,
  "allow_force_pushes": false,
  "allow_deletions": false
}
JSON
```

To verify the live rule against this document and `ci.yml`, run:

```bash
make check-branch-protection
make check-branch-protection ARGS="--verify-remote"
```

The `--verify-remote` flag reads the live protection via `gh api` and reports required
contexts that are absent from CI or not enforced. It requires admin scope on the
repository (`gh auth refresh -s admin:repo`).

## Testnet Smoke Test

The [Testnet Smoke Test](workflows/smoke-test.yml) workflow is **not** a required PR check.
It needs the `TESTNET_SECRET_KEY` secret to sign real testnet transactions, so it cannot run on pull
requests or from forks. Instead it runs:

- on pushes to `main`
- nightly on a schedule
- on releases
- on demand (`workflow_dispatch`)
