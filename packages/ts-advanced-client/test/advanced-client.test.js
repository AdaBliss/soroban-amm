const test = require('node:test')
const assert = require('node:assert/strict')
const { setImmediate: tick } = require('node:timers/promises')
const {
  AdvancedClient,
  MiddlewareAborted,
  MiddlewareStack,
  plugin,
  priceFeedMiddleware,
  signerMiddleware,
} = require('../lib/index.js')

test('runs middleware in order and submits the processed transaction', async () => {
  const calls = []
  const submitter = {
    async sendTransaction(tx) {
      calls.push(['submit', tx])
      return { hash: 'abc123', tx }
    },
  }
  const client = new AdvancedClient(submitter)
  client
    .middleware()
    .use(priceFeedMiddleware(async () => ({ numerator: 12345n, denominator: 100n })))
    .use(signerMiddleware({ sign: async (tx) => ({ ...tx, signed: true }) }))
    .use(async (ctx, next) => {
      calls.push(['middleware', ctx.metadata.quote])
      await next()
    })

  const result = await client.sendTransaction({ kind: 'swap', amount: 1000 })

  assert.deepEqual(calls, [
    ['middleware', { numerator: 12345n, denominator: 100n }],
    ['submit', { kind: 'swap', amount: 1000, signed: true }],
  ])
  assert.deepEqual(result, {
    hash: 'abc123',
    tx: { kind: 'swap', amount: 1000, signed: true },
  })
})

test('does not submit when middleware fails', async () => {
  let submitted = false
  const client = new AdvancedClient({
    async sendTransaction() {
      submitted = true
      return { ok: true }
    },
  })
  client.middleware().use(async () => {
    throw new Error('validation failed')
  })

  await assert.rejects(() => client.sendTransaction({ kind: 'swap' }), /validation failed/)
  assert.equal(submitted, false)
})

test('rejects calling next more than once', async () => {
  const stack = new MiddlewareStack()
  stack.use(async (_ctx, next) => {
    await next()
    await assert.rejects(() => next(), /next\(\) called multiple times/)
  })

  await stack.run({ tx: { kind: 'swap' }, metadata: {} })
})

function recordingSubmitter() {
  const submitted = []
  return {
    submitted,
    async sendTransaction(tx) {
      submitted.push(tx)
      return { hash: 'h' + submitted.length }
    },
  }
}

test('a middleware that returns without calling next() aborts and nothing is submitted', async () => {
  const submitter = recordingSubmitter()
  const client = new AdvancedClient(submitter)
  client.middleware().use(async () => {
    // "if invalid, return": the natural way to write a validation middleware
  })

  await assert.rejects(() => client.sendTransaction({ kind: 'swap' }), MiddlewareAborted)
  assert.deepEqual(submitter.submitted, [])
})

test('a middleware further down the chain that skips next() also aborts', async () => {
  const submitter = recordingSubmitter()
  const client = new AdvancedClient(submitter)
  client
    .middleware()
    .use(async (_ctx, next) => {
      await next()
    })
    .use(async () => {})

  await assert.rejects(() => client.sendTransaction({ kind: 'swap' }), MiddlewareAborted)
  assert.equal(submitter.submitted.length, 0)
})

test('MiddlewareAborted is a named Error', () => {
  const error = new MiddlewareAborted()
  assert.ok(error instanceof Error)
  assert.equal(error.name, 'MiddlewareAborted')
})

test('throwing still propagates the original error, not MiddlewareAborted', async () => {
  const client = new AdvancedClient(recordingSubmitter())
  client.middleware().use(async () => {
    throw new RangeError('explicit rejection')
  })

  await assert.rejects(() => client.sendTransaction({}), RangeError)
})

test('an empty stack submits the transaction', async () => {
  const submitter = recordingSubmitter()
  const client = new AdvancedClient(submitter)

  assert.deepEqual(await client.sendTransaction({ kind: 'swap' }), { hash: 'h1' })
})

test('run reports whether the chain completed', async () => {
  const done = new MiddlewareStack().use(async (_ctx, next) => next())
  assert.deepEqual(await done.run({ tx: {}, metadata: {} }), { completed: true })

  const aborted = new MiddlewareStack().use(async () => {})
  assert.deepEqual(await aborted.run({ tx: {}, metadata: {} }), { completed: false })
})

test('run executes the terminal step only when the chain completes', async () => {
  let terminalRuns = 0
  const terminal = async () => {
    terminalRuns += 1
  }
  await new MiddlewareStack().use(async () => {}).run({ tx: {}, metadata: {} }, terminal)
  assert.equal(terminalRuns, 0)

  await new MiddlewareStack().use(async (_c, next) => next()).run({ tx: {}, metadata: {} }, terminal)
  assert.equal(terminalRuns, 1)
})

test('next() may be called again after the downstream attempt failed', async () => {
  let attempts = 0
  const stack = new MiddlewareStack()
  stack
    .use(async (_ctx, next) => {
      await assert.rejects(() => next(), /boom/)
      await next()
    })
    .use(async (_ctx, next) => {
      attempts += 1
      if (attempts === 1) throw new Error('boom')
      await next()
    })

  assert.deepEqual(await stack.run({ tx: {}, metadata: {} }), { completed: true })
  assert.equal(attempts, 2)
})

test('next() is rejected while a previous call is still running', async () => {
  const stack = new MiddlewareStack()
  stack
    .use(async (_ctx, next) => {
      const first = next()
      await assert.rejects(() => next(), /next\(\) called multiple times/)
      await first
    })
    .use(async (_ctx, next) => {
      await tick()
      await next()
    })

  await stack.run({ tx: {}, metadata: {} })
})

test('priceFeedMiddleware stores an exact bigint quote and no floating point', async () => {
  const client = new AdvancedClient(recordingSubmitter())
  let seen
  client
    .middleware()
    .use(priceFeedMiddleware(async () => ({ numerator: 10n ** 30n + 1n, denominator: 3n })))
    .use(async (ctx, next) => {
      seen = ctx.metadata.quote
      await next()
    })

  await client.sendTransaction({})
  assert.equal(typeof seen.numerator, 'bigint')
  assert.equal(typeof seen.denominator, 'bigint')
  assert.equal(seen.numerator, 10n ** 30n + 1n)
})

test('plugin hooks run around submission and see the processed transaction and result', async () => {
  const order = []
  const submitter = {
    async sendTransaction(tx) {
      order.push(['submit', tx])
      return { hash: 'abc' }
    },
  }
  const client = new AdvancedClient(submitter)
  client.plugin(
    plugin({
      name: 'audit',
      middleware: async (ctx, next) => {
        ctx.tx = { ...ctx.tx, audited: true }
        order.push(['middleware'])
        await next()
      },
      hooks: {
        beforeSend: (tx) => {
          order.push(['beforeSend', tx])
        },
        afterSend: (result) => {
          order.push(['afterSend', result])
        },
      },
    }),
  )

  await client.sendTransaction({ kind: 'swap' })
  assert.deepEqual(order, [
    ['middleware'],
    ['beforeSend', { kind: 'swap', audited: true }],
    ['submit', { kind: 'swap', audited: true }],
    ['afterSend', { hash: 'abc' }],
  ])
})

test('plugin hooks do not run when the chain aborts', async () => {
  let hooked = false
  const client = new AdvancedClient(recordingSubmitter())
  client.plugin(
    plugin({
      name: 'gate',
      middleware: async () => {},
      hooks: {
        beforeSend: () => {
          hooked = true
        },
      },
    }),
  )

  await assert.rejects(() => client.sendTransaction({}), MiddlewareAborted)
  assert.equal(hooked, false)
})
