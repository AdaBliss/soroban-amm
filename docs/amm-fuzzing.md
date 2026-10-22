# AMM stateful invariant fuzzing

The deployed-contract harness in `contracts/amm-fuzz` applies generated AMM
operation sequences to the real `amm.wasm`, checking state after every call.
It covers liquidity changes, exact-in and exact-out swaps, rejected flash
loans, protocol fee withdrawal and configuration, ledger advancement, and the
emergency drain path. Initial pools include balanced and 1:1000 reserve ratios.

The checked invariants are:

- recorded reserve plus accrued protocol fee equals the pool token balance;
- recorded LP shares equal the LP token supply outside a quarantined drain;
- tracked account and pool token balances remain globally conserved;
- rejected calls leave observable state unchanged;
- exact-out calls deliver the exact amount without exceeding their quote.

## Run

Use the bounded CI profile:

```sh
make fuzz-amm
```

Use the deeper nightly profile:

```sh
make fuzz-amm-nightly
```

Override the case count with `PROPTEST_CASES`. When proptest reports a failing
seed, replay it with the printed `PROPTEST_RNG_SEED` value and the same command.
Keep `PROPTEST_MAX_SHRINK_ITERS` enabled so the operation vector shrinks to a
minimal reproduction.

## Known failures

`KNOWN_FAILURES` in `contracts/amm-fuzz/src/lib.rs` maps a single invariant and
operation to an open issue. It is not a blanket test skip. Remove the mapping
when its issue is fixed so that operation is immediately checked by the full
invariant set. The catalog test rejects duplicate or untraceable entries.
