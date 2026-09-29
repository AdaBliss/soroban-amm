## Summary of Changes

<!-- Explain *what* changed and *why*. Keep it concise but complete enough
     for a reviewer who has no prior context. -->

## Related Issue(s)

<!-- Link every issue this PR addresses.
     Use "Closes #<n>" to auto-close on merge, or "Related to #<n>" for partial work. -->

- Closes #

## Type of Change

<!-- Check all that apply. -->

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor (no behaviour change)
- [ ] Tests only
- [ ] Documentation only
- [ ] Chore (build, tooling, dependencies)

## Checklist

<!-- Complete every item before requesting review. -->

- [ ] `cargo fmt` has been run
- [ ] `make check` has been run (or the reason it could not be run is documented)
- [ ] `cargo clippy -- -D warnings` passes with no new warnings
- [ ] `cargo test` passes (build WASM first for factory tests: `cargo build --release --target wasm32-unknown-unknown`)
- [ ] New behaviour is covered by tests
- [ ] WASM size has been checked for contract changes
- [ ] For security-sensitive changes, correctness verification is described in the PR
- [ ] If I changed a hot-path contract (`amm`, `concentrated_liquidity`, `batch_auction`), I ran `cargo run -p benches -- --write-baseline` and committed the updated `benches/baseline.json`
- [ ] Public interface and error documentation updated (`docs/abi.json`, `docs/error-codes.md`)
- [ ] `CHANGELOG.md` has been updated with any notable changes
- [ ] Commit messages follow the [Conventional Commits](https://www.conventionalcommits.org/) format
