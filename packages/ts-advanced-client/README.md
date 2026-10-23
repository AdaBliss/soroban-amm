# Advanced TypeScript Client with Middleware

A chainable middleware API for Soroban transaction signing, validation, price
feeds and plugins. It is generic over the transaction type and ships a Soroban submitter that implements the simulate → assemble → sign → send → poll
lifecycle against a narrow RPC interface.

## Install

```sh
npm install @soroban-amm/ts-advanced-client
```

## Quickstart

```ts
import {
  AdvancedClient,
  priceFeedMiddleware,
  signerMiddleware,
  validationMiddleware,
  nonZeroAmountRule,
  deadlineRule,
  slippageRule,
  type ValidationMetadata,
} from '@soroban-amm/ts-advanced-client'

type Transaction = { kind: string; amount: bigint; signed?: boolean }
type Result = { hash: string }

const client = new AdvancedClient<Transaction, Result, ValidationMetadata>(submitter)

client.middleware()
  .use(priceFeedMiddleware(async () => ({ numerator: 12345n, denominator: 1000n })))
  .use(
    validationMiddleware([nonZeroAmountRule, deadlineRule(() => Date.now()), slippageRule], {
      // Validate values from the transaction itself (default: `ctx.metadata`).
      select: (ctx) => ({ amount: ctx.tx.amount }),
    }),
  )
  .use(signerMiddleware({ sign: async (tx) => ({ ...tx, signed: true }) }))

const result = await client.sendTransaction({ kind: 'swap', amount: 1000n })
```

## Aborting the chain

A middleware that returns without calling `next()` aborts the chain. The
transaction is *not* submitted, and `sendTransaction` rejects with a
`MiddlewareAborted` error. Submission is the last step of the chain, so a
middleware that wraps `next()` (retry, logging) also covers it. This is the natural way to write a validation
middleware ("if invalid, return"). Throwing is the explicit rejection path
and propagates the error to the caller.

```ts
import { MiddlewareAborted } from '@soroban-amm/ts-advanced-client'

try {
  await client.sendTransaction(tx)
} catch (err) {
  if (err instanceof MiddlewareAborted) {
    // the chain aborted; nothing was submitted
  }
}
```

## Price quotes

Prices are `bigint`-based `PriceQuote { numerator, denominator }` values.
No floating point is involved, so i128 amounts remain exact and downstream
slippage checks do not lose precision.

## Middlewares

- `signerMiddleware(signer)` — signs the transaction.
- `priceFeedMiddleware(getQuote)` — stores a `PriceQuote` in `metadata.quote`.
- `validationMiddleware(rules, { select? })` — reads `amount`, `deadline`,
  `quote` and `minRatio` from `ctx.metadata` (or `select`) and runs rules.
  Valid input passes through unchanged. Built-in rules:
  - `nonZeroAmountRule` — rejects zero/negative amounts (`INVALID_AMOUNT`),
  - `deadlineRule(clock)` — rejects past deadlines (`DEADLINE_PASSED`),
  - `slippageRule` — rejects quotes below `minRatio` (`SLIPPAGE_EXCEEDED`).
- `retryMiddleware({ attempts, backoffMs, retryOn })` — re-runs the rest of the
  chain, submission included, on errors `retryOn` accepts. Waits
  `backoffMs * 2^(attempt - 1)` between attempts and rethrows the last error
  once `attempts` is used up.
- `loggingMiddleware(sink)` — logs begin/end/error to a sink.
- `plugin({ name, middleware, hooks })` — a typed definition for
  `client.plugin(...)`, which registers the middleware plus `beforeSend` /
  `afterSend` lifecycle hooks (they run around the submitter, after the whole
  chain succeeded, and not at all when the chain aborts).

## Soroban submitter

`SorobanSubmitter` implements `TransactionSubmitter` on top of a narrow
`SorobanRpc` interface (the `@soroban-amm/sdk` write path implements it). It
simulates, assembles with the simulated resource fee, signs, sends, resubmits
on `TRY_AGAIN_LATER`, and polls until `SUCCESS`/`FAILED` or a timeout. Every
failure is a `SorobanSubmissionError` whose `code` is one of
`SIMULATION_FAILED`, `SEND_REJECTED`, `RESUBMIT_EXHAUSTED`,
`TRANSACTION_FAILED` or `POLL_TIMEOUT`.

```ts
import { SorobanSubmitter } from '@soroban-amm/ts-advanced-client'

const submitter = new SorobanSubmitter({
  rpc,
  signer,
  timeoutMs: 30_000,
  pollIntervalMs: 1_000,
  maxResubmits: 3,
})
```
