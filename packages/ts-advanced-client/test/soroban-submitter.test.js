const test = require('node:test')
const assert = require('node:assert/strict')
const {
  AdvancedClient,
  SorobanSubmissionError,
  SorobanSubmitter,
  retryMiddleware,
} = require('../lib/index.js')

// A fake RPC that records every call and answers from scripted queues.
function fakeRpc({ simulation = {}, sends = [{ status: 'PENDING', hash: 'H' }], polls = [] } = {}) {
  const calls = []
  const sendQueue = [...sends]
  const pollQueue = [...polls]
  return {
    calls,
    async simulateTransaction(tx) {
      calls.push(['simulate', tx])
      return simulation
    },
    assembleTransaction(tx, sim) {
      calls.push(['assemble', sim])
      return { ...tx, assembled: true }
    },
    async sendTransaction(tx) {
      calls.push(['send', tx])
      return sendQueue.length > 1 ? sendQueue.shift() : sendQueue[0]
    },
    async getTransaction(hash) {
      calls.push(['get', hash])
      return pollQueue.length > 1 ? pollQueue.shift() : pollQueue[0]
    },
  }
}

const signer = {
  async sign(tx) {
    return { ...tx, signed: true }
  },
}

// Virtual time: sleeping advances the clock instead of waiting.
function virtualTime() {
  let now = 0
  return {
    sleeps: [],
    now: () => now,
    sleep(ms) {
      this.sleeps.push(ms)
      now += ms
      return Promise.resolve()
    },
  }
}

function submitterFor(rpc, extra = {}) {
  const time = virtualTime()
  const submitter = new SorobanSubmitter({
    rpc,
    signer,
    sleep: (ms) => time.sleep(ms),
    now: time.now,
    pollIntervalMs: 100,
    timeoutMs: 1_000,
    ...extra,
  })
  return { submitter, time }
}

async function failure(promise) {
  try {
    await promise
  } catch (error) {
    assert.ok(error instanceof SorobanSubmissionError, `unexpected error: ${String(error)}`)
    return error
  }
  assert.fail('expected a rejection')
}

test('simulates, assembles, signs, sends and polls until SUCCESS', async () => {
  const rpc = fakeRpc({
    simulation: { minResourceFee: 500n },
    polls: [{ status: 'NOT_FOUND' }, { status: 'SUCCESS', returnValue: 42n }],
  })
  const { submitter } = submitterFor(rpc)

  const result = await submitter.sendTransaction({ kind: 'swap' })

  assert.deepEqual(result, { hash: 'H', status: 'SUCCESS', returnValue: 42n })
  assert.deepEqual(rpc.calls, [
    ['simulate', { kind: 'swap' }],
    ['assemble', { minResourceFee: 500n }],
    ['send', { kind: 'swap', assembled: true, signed: true }],
    ['get', 'H'],
    ['get', 'H'],
  ])
})

test('a failed simulation is SIMULATION_FAILED and nothing is sent', async () => {
  const rpc = fakeRpc({ simulation: { error: 'HostError: Error(Contract, #3)' } })
  const { submitter } = submitterFor(rpc)

  const error = await failure(submitter.sendTransaction({}))
  assert.equal(error.code, 'SIMULATION_FAILED')
  assert.match(error.message, /Error\(Contract, #3\)/)
  assert.deepEqual(rpc.calls.map(([name]) => name), ['simulate'])
})

test('a FAILED result is TRANSACTION_FAILED and carries the hash', async () => {
  const rpc = fakeRpc({ polls: [{ status: 'FAILED' }] })
  const { submitter } = submitterFor(rpc)

  const error = await failure(submitter.sendTransaction({}))
  assert.equal(error.code, 'TRANSACTION_FAILED')
  assert.equal(error.hash, 'H')
})

test('polling past the timeout is POLL_TIMEOUT', async () => {
  const rpc = fakeRpc({ polls: [{ status: 'NOT_FOUND' }] })
  const { submitter, time } = submitterFor(rpc, { timeoutMs: 450, pollIntervalMs: 100 })

  const error = await failure(submitter.sendTransaction({}))
  assert.equal(error.code, 'POLL_TIMEOUT')
  assert.equal(error.hash, 'H')
  assert.deepEqual(time.sleeps, [100, 100, 100, 100, 100])
  assert.equal(rpc.calls.filter(([name]) => name === 'get').length, 6)
})

test('a result that arrives on the last poll before the deadline still succeeds', async () => {
  const rpc = fakeRpc({
    polls: [{ status: 'NOT_FOUND' }, { status: 'NOT_FOUND' }, { status: 'SUCCESS' }],
  })
  const { submitter } = submitterFor(rpc, { timeoutMs: 250, pollIntervalMs: 100 })

  assert.equal((await submitter.sendTransaction({})).status, 'SUCCESS')
})

test('TRY_AGAIN_LATER is resubmitted after a pause, then polled', async () => {
  const rpc = fakeRpc({
    sends: [{ status: 'TRY_AGAIN_LATER', hash: 'X' }, { status: 'TRY_AGAIN_LATER', hash: 'X' }, { status: 'PENDING', hash: 'H' }],
    polls: [{ status: 'SUCCESS' }],
  })
  const { submitter, time } = submitterFor(rpc)

  const result = await submitter.sendTransaction({})
  assert.equal(result.hash, 'H')
  assert.equal(rpc.calls.filter(([name]) => name === 'send').length, 3)
  assert.deepEqual(time.sleeps, [100, 100])
})

test('giving up on TRY_AGAIN_LATER is RESUBMIT_EXHAUSTED', async () => {
  const rpc = fakeRpc({ sends: [{ status: 'TRY_AGAIN_LATER', hash: 'X' }] })
  const { submitter } = submitterFor(rpc, { maxResubmits: 2 })

  const error = await failure(submitter.sendTransaction({}))
  assert.equal(error.code, 'RESUBMIT_EXHAUSTED')
  assert.equal(rpc.calls.filter(([name]) => name === 'send').length, 3)
  assert.equal(rpc.calls.some(([name]) => name === 'get'), false)
})

test('an ERROR send is SEND_REJECTED with the network message', async () => {
  const rpc = fakeRpc({ sends: [{ status: 'ERROR', hash: 'E', errorMessage: 'tx_bad_seq' }] })
  const { submitter } = submitterFor(rpc)

  const error = await failure(submitter.sendTransaction({}))
  assert.equal(error.code, 'SEND_REJECTED')
  assert.match(error.message, /tx_bad_seq/)
})

test('DUPLICATE is treated as accepted and polled', async () => {
  const rpc = fakeRpc({
    sends: [{ status: 'DUPLICATE', hash: 'D' }],
    polls: [{ status: 'SUCCESS' }],
  })
  const { submitter } = submitterFor(rpc)

  assert.equal((await submitter.sendTransaction({})).hash, 'D')
})

test('works as the AdvancedClient submitter, with retry covering submission', async () => {
  let simulations = 0
  const rpc = fakeRpc({ polls: [{ status: 'SUCCESS', returnValue: 'done' }] })
  const simulate = rpc.simulateTransaction.bind(rpc)
  rpc.simulateTransaction = async (tx) => {
    simulations += 1
    return simulations === 1 ? { error: 'RPC timeout' } : simulate(tx)
  }
  const { submitter } = submitterFor(rpc)
  const client = new AdvancedClient(submitter)
  client.middleware().use(
    retryMiddleware({
      attempts: 2,
      backoffMs: 0,
      retryOn: (error) => error instanceof SorobanSubmissionError && error.code === 'SIMULATION_FAILED',
    }),
  )

  const result = await client.sendTransaction({ kind: 'swap' })
  assert.equal(result.returnValue, 'done')
  assert.equal(simulations, 2)
})
