const test = require('node:test')
const assert = require('node:assert/strict')
const { setImmediate: tick } = require('node:timers/promises')
const {
  AdvancedClient,
  MiddlewareAborted,
  ValidationError,
  deadlineRule,
  loggingMiddleware,
  nonZeroAmountRule,
  retryMiddleware,
  slippageRule,
  validationMiddleware,
} = require('../lib/index.js')

function clientWith(...middlewares) {
  const submitted = []
  const client = new AdvancedClient({
    async sendTransaction(tx) {
      submitted.push(tx)
      return { ok: true }
    },
  })
  for (const middleware of middlewares) client.middleware().use(middleware)
  return { client, submitted }
}

const rules = (now = () => 1_000) => [nonZeroAmountRule, deadlineRule(now), slippageRule]

async function codeOf(promise) {
  try {
    await promise
  } catch (error) {
    assert.ok(error instanceof ValidationError, `expected a ValidationError, got ${String(error)}`)
    return error.code
  }
  assert.fail('expected a rejection')
}

test('validation rejects a zero amount with INVALID_AMOUNT and submits nothing', async () => {
  const { client, submitted } = clientWith(
    validationMiddleware(rules(), { select: (ctx) => ({ amount: ctx.tx.amount }) }),
  )

  assert.equal(await codeOf(client.sendTransaction({ amount: 0n })), 'INVALID_AMOUNT')
  assert.equal(await codeOf(client.sendTransaction({ amount: -5n })), 'INVALID_AMOUNT')
  assert.deepEqual(submitted, [])
})

test('validation rejects a past or present deadline with DEADLINE_PASSED (injected clock)', async () => {
  const { client, submitted } = clientWith(
    validationMiddleware(rules(() => 5_000), {
      select: (ctx) => ({ amount: 1n, deadline: ctx.tx.deadline }),
    }),
  )

  assert.equal(await codeOf(client.sendTransaction({ deadline: 4_999 })), 'DEADLINE_PASSED')
  assert.equal(await codeOf(client.sendTransaction({ deadline: 5_000 })), 'DEADLINE_PASSED')
  assert.deepEqual(submitted, [])
  await client.sendTransaction({ deadline: 5_001 })
  assert.equal(submitted.length, 1)
})

test('validation rejects a quote below minRatio with SLIPPAGE_EXCEEDED', async () => {
  const { client } = clientWith(
    async (ctx, next) => {
      ctx.metadata.amount = 1n
      ctx.metadata.quote = { numerator: 99n, denominator: 100n }
      ctx.metadata.minRatio = { numerator: 1n, denominator: 1n }
      await next()
    },
    validationMiddleware(rules()),
  )

  assert.equal(await codeOf(client.sendTransaction({})), 'SLIPPAGE_EXCEEDED')
})

test('slippage uses exact cross-multiplication, not floats', async () => {
  const huge = 10n ** 30n
  const make = (quote) =>
    clientWith(
      async (ctx, next) => {
        ctx.metadata.amount = 1n
        ctx.metadata.quote = quote
        ctx.metadata.minRatio = { numerator: huge + 1n, denominator: huge }
        await next()
      },
      validationMiddleware(rules()),
    ).client

  // Equal to the bound passes; one unit below it fails. A float comparison
  // cannot tell these apart at 1e30.
  await make({ numerator: huge + 1n, denominator: huge }).sendTransaction({})
  assert.equal(
    await codeOf(make({ numerator: huge, denominator: huge }).sendTransaction({})),
    'SLIPPAGE_EXCEEDED',
  )
})

test('a zero quote denominator is rejected as SLIPPAGE_EXCEEDED', async () => {
  const { client } = clientWith(
    async (ctx, next) => {
      ctx.metadata.amount = 1n
      ctx.metadata.quote = { numerator: 1n, denominator: 0n }
      await next()
    },
    validationMiddleware(rules()),
  )

  assert.equal(await codeOf(client.sendTransaction({})), 'SLIPPAGE_EXCEEDED')
})

test('the three built-in rules report distinct error codes', async () => {
  const codes = new Set()
  for (const input of [
    { amount: 0n, minRatio: { numerator: 0n, denominator: 1n } },
    { amount: 1n, deadline: 1, minRatio: { numerator: 0n, denominator: 1n } },
    {
      amount: 1n,
      quote: { numerator: 1n, denominator: 2n },
      minRatio: { numerator: 1n, denominator: 1n },
    },
  ]) {
    for (const rule of rules(() => 10)) {
      try {
        rule(input)
      } catch (error) {
        codes.add(error.code)
      }
    }
  }
  assert.deepEqual([...codes].sort(), ['DEADLINE_PASSED', 'INVALID_AMOUNT', 'SLIPPAGE_EXCEEDED'])
})

test('valid input passes through unchanged', async () => {
  const seen = []
  const { client, submitted } = clientWith(
    validationMiddleware(rules(() => 1), {
      select: (ctx) => ({ amount: ctx.tx.amount, deadline: ctx.tx.deadline }),
    }),
    async (ctx, next) => {
      seen.push(ctx.tx)
      await next()
    },
  )
  const tx = { amount: 7n, deadline: 100 }

  await client.sendTransaction(tx)
  assert.deepEqual(seen, [tx])
  assert.deepEqual(submitted, [tx])
})

test('a missing deadline and missing quote are not validated', async () => {
  const { client, submitted } = clientWith(
    async (ctx, next) => {
      ctx.metadata.amount = 1n
      await next()
    },
    validationMiddleware(rules()),
  )

  await client.sendTransaction({})
  assert.equal(submitted.length, 1)
})

// -------------------------------------------------------------------------------------------------
// retry, with fake timers
// -------------------------------------------------------------------------------------------------

function flaky(failures, error = new Error('RPC unavailable')) {
  let calls = 0
  return {
    get calls() {
      return calls
    },
    async sendTransaction(tx) {
      calls += 1
      if (calls <= failures) throw error
      return { hash: 'ok', tx }
    },
  }
}

// Lets pending promise callbacks run without touching the mocked timers.
const settle = () => tick()

test('retry re-runs a submitter that fails twice and then succeeds, with doubling backoff', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] })
  const submitter = flaky(2)
  const client = new AdvancedClient(submitter)
  client.middleware().use(retryMiddleware({ attempts: 3, backoffMs: 100, retryOn: () => true }))

  const pending = client.sendTransaction({ kind: 'swap' })
  await settle()
  assert.equal(submitter.calls, 1)

  t.mock.timers.tick(99)
  await settle()
  assert.equal(submitter.calls, 1, 'still waiting out the first 100 ms backoff')
  t.mock.timers.tick(1)
  await settle()
  assert.equal(submitter.calls, 2)

  t.mock.timers.tick(199)
  await settle()
  assert.equal(submitter.calls, 2, 'the second backoff is 200 ms')
  t.mock.timers.tick(1)
  await settle()

  assert.deepEqual(await pending, { hash: 'ok', tx: { kind: 'swap' } })
  assert.equal(submitter.calls, 3)
})

test('retry gives up after the configured attempts with the last error', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] })
  const errors = [new Error('first'), new Error('second'), new Error('third')]
  let calls = 0
  const client = new AdvancedClient({
    async sendTransaction() {
      throw errors[calls++]
    },
  })
  client.middleware().use(retryMiddleware({ attempts: 3, backoffMs: 10, retryOn: () => true }))

  const pending = client.sendTransaction({})
  const outcome = assert.rejects(pending, (error) => error === errors[2])
  for (const ms of [10, 20]) {
    await settle()
    t.mock.timers.tick(ms)
  }
  await outcome
  assert.equal(calls, 3)
})

test('retry does not retry errors retryOn rejects', async () => {
  const submitter = flaky(5, new TypeError('permanent'))
  const client = new AdvancedClient(submitter)
  client
    .middleware()
    .use(retryMiddleware({ attempts: 5, backoffMs: 1, retryOn: (e) => !(e instanceof TypeError) }))

  await assert.rejects(() => client.sendTransaction({}), TypeError)
  assert.equal(submitter.calls, 1)
})

test('retry with a single attempt never waits', async () => {
  const submitter = flaky(1)
  const client = new AdvancedClient(submitter)
  client.middleware().use(retryMiddleware({ attempts: 1, backoffMs: 1_000, retryOn: () => true }))

  await assert.rejects(() => client.sendTransaction({}), /RPC unavailable/)
  assert.equal(submitter.calls, 1)
})

test('retry re-runs the middlewares after it, not the ones before it', async () => {
  const runs = { before: 0, after: 0 }
  const submitter = flaky(1)
  const client = new AdvancedClient(submitter)
  client
    .middleware()
    .use(async (_ctx, next) => {
      runs.before += 1
      await next()
    })
    .use(retryMiddleware({ attempts: 2, backoffMs: 0, retryOn: () => true }))
    .use(async (_ctx, next) => {
      runs.after += 1
      await next()
    })

  await client.sendTransaction({})
  assert.deepEqual(runs, { before: 1, after: 2 })
})

test('a retried chain whose middleware aborts still ends in MiddlewareAborted', async () => {
  const { client } = clientWith(
    retryMiddleware({ attempts: 3, backoffMs: 0, retryOn: () => true }),
    async () => {},
  )

  await assert.rejects(() => client.sendTransaction({}), MiddlewareAborted)
})

// -------------------------------------------------------------------------------------------------
// logging
// -------------------------------------------------------------------------------------------------

test('logging reports begin and end around a successful chain', async () => {
  const lines = []
  const sink = {
    info: (message, data) => lines.push(['info', message, data]),
    error: (message, data) => lines.push(['error', message, data]),
  }
  const { client } = clientWith(loggingMiddleware(sink))

  await client.sendTransaction({ kind: 'swap' })
  assert.deepEqual(lines, [
    ['info', 'middleware:begin', { tx: { kind: 'swap' } }],
    ['info', 'middleware:end', undefined],
  ])
})

test('logging reports the error and rethrows it', async () => {
  const lines = []
  const sink = {
    info: (message) => lines.push(['info', message]),
    error: (message, data) => lines.push(['error', message, data]),
  }
  const failure = new Error('submit failed')
  const client = new AdvancedClient({
    async sendTransaction() {
      throw failure
    },
  })
  client.middleware().use(loggingMiddleware(sink))

  await assert.rejects(() => client.sendTransaction({}), (error) => error === failure)
  assert.deepEqual(lines, [
    ['info', 'middleware:begin'],
    ['error', 'middleware:error', failure],
  ])
})
