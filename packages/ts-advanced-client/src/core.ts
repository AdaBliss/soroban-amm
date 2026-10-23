// -------------------------------------------------------------------------------------------------------
// Core types
// -------------------------------------------------------------------------------------------------------

export type Transaction = object

/** Free-form per-request data middlewares use to talk to each other. */
export type MiddlewareMetadata = Record<string, unknown>

/** A price as an exact ratio. AMM amounts are i128, so no floating point. */
export interface PriceQuote {
  numerator: bigint
  denominator: bigint
}

export interface MiddlewareContext<
  TTransaction extends Transaction = Transaction,
  TMetadata extends MiddlewareMetadata = MiddlewareMetadata,
> {
  tx: TTransaction
  metadata: TMetadata
}

export type Middleware<
  TTransaction extends Transaction = Transaction,
  TMetadata extends MiddlewareMetadata = MiddlewareMetadata,
> = (ctx: MiddlewareContext<TTransaction, TMetadata>, next: () => Promise<void>) => Promise<void>

export interface TransactionSubmitter<
  TTransaction extends Transaction = Transaction,
  TResult = unknown,
> {
  sendTransaction(tx: TTransaction): Promise<TResult>
}

/**
 * Thrown by `AdvancedClient.sendTransaction` when a middleware returned
 * without calling `next()`, so the chain never reached submission. Throwing
 * from a middleware is the explicit rejection path and propagates as-is.
 */
export class MiddlewareAborted extends Error {
  constructor(message = 'middleware chain ended without calling next(); nothing was submitted') {
    super(message)
    this.name = 'MiddlewareAborted'
  }
}

export interface RunResult {
  /** `true` only when every middleware called `next()` and the terminal step ran. */
  completed: boolean
}

// -------------------------------------------------------------------------------------------------------
// Plugins
// -------------------------------------------------------------------------------------------------------

export interface PluginHooks<TTransaction extends Transaction = Transaction, TResult = unknown> {
  /** Runs after the whole middleware chain succeeded, right before submission. */
  beforeSend?: (tx: TTransaction) => Promise<void> | void
  /** Runs after the submitter returned. */
  afterSend?: (result: TResult) => Promise<void> | void
}

export interface Plugin<
  TTransaction extends Transaction = Transaction,
  TResult = unknown,
  TMetadata extends MiddlewareMetadata = MiddlewareMetadata,
> {
  name: string
  middleware: Middleware<TTransaction, TMetadata>
  hooks?: PluginHooks<TTransaction, TResult>
}

/** Identity helper that infers the plugin's types; register it with `client.plugin()`. */
export function plugin<
  TTransaction extends Transaction,
  TResult = unknown,
  TMetadata extends MiddlewareMetadata = MiddlewareMetadata,
>(
  definition: Plugin<TTransaction, TResult, TMetadata>,
): Plugin<TTransaction, TResult, TMetadata> {
  return definition
}

// -------------------------------------------------------------------------------------------------------
// Stack and client
// -------------------------------------------------------------------------------------------------------

export class MiddlewareStack<
  TTransaction extends Transaction = Transaction,
  TMetadata extends MiddlewareMetadata = MiddlewareMetadata,
> {
  private readonly middlewares: Array<Middleware<TTransaction, TMetadata>> = []

  use(middleware: Middleware<TTransaction, TMetadata>): this {
    this.middlewares.push(middleware)
    return this
  }

  /**
   * Runs the chain, then `terminal` once the last middleware has called
   * `next()`. A middleware that returns without calling `next()` makes the
   * result `{ completed: false }` and `terminal` never runs.
   *
   * Each middleware's `next` may be called again only after the previous
   * downstream attempt rejected, which is what lets `retryMiddleware` re-run
   * the rest of the chain. Calling it while an attempt is running, or after
   * one succeeded, throws.
   */
  async run(
    ctx: MiddlewareContext<TTransaction, TMetadata>,
    terminal?: () => Promise<void>,
  ): Promise<RunResult> {
    let completed = false
    const dispatch = async (index: number): Promise<void> => {
      const middleware = this.middlewares[index]
      if (!middleware) {
        if (terminal) await terminal()
        completed = true
        return
      }
      let state: 'idle' | 'running' | 'done' = 'idle'
      const next = async (): Promise<void> => {
        if (state !== 'idle') throw new Error('next() called multiple times')
        state = 'running'
        try {
          await dispatch(index + 1)
          state = 'done'
        } catch (error) {
          state = 'idle'
          throw error
        }
      }
      await middleware(ctx, next)
    }
    await dispatch(0)
    return { completed }
  }
}

export class AdvancedClient<
  TTransaction extends Transaction = Transaction,
  TResult = unknown,
  TMetadata extends MiddlewareMetadata = MiddlewareMetadata,
> {
  private readonly stack = new MiddlewareStack<TTransaction, TMetadata>()
  private readonly hooks: Array<PluginHooks<TTransaction, TResult>> = []

  constructor(private readonly submitter: TransactionSubmitter<TTransaction, TResult>) {}

  middleware(): MiddlewareStack<TTransaction, TMetadata> {
    return this.stack
  }

  /** Registers a plugin's middleware and its lifecycle hooks. */
  plugin(definition: Plugin<TTransaction, TResult, TMetadata>): this {
    this.stack.use(definition.middleware)
    if (definition.hooks) this.hooks.push(definition.hooks)
    return this
  }

  /**
   * Runs the middleware chain and submits the processed transaction. Submission
   * is the last step of the chain, so a middleware that wraps `next()` (retry,
   * logging) also covers it.
   *
   * Rejects with `MiddlewareAborted`, without submitting, when a middleware
   * returns without calling `next()`.
   */
  async sendTransaction(tx: TTransaction): Promise<TResult> {
    const ctx: MiddlewareContext<TTransaction, TMetadata> = { tx, metadata: {} as TMetadata }
    let result: { value: TResult } | undefined
    const { completed } = await this.stack.run(ctx, async () => {
      for (const hooks of this.hooks) await hooks.beforeSend?.(ctx.tx)
      const value = await this.submitter.sendTransaction(ctx.tx)
      result = { value }
      for (const hooks of this.hooks) await hooks.afterSend?.(value)
    })
    if (!completed || !result) throw new MiddlewareAborted()
    return result.value
  }
}

// -------------------------------------------------------------------------------------------------------
// Basic middlewares
// -------------------------------------------------------------------------------------------------------

export interface Signer<TTransaction extends Transaction = Transaction> {
  sign(tx: TTransaction): Promise<TTransaction>
}

export function signerMiddleware<TTransaction extends Transaction>(
  signer: Signer<TTransaction>,
): Middleware<TTransaction> {
  return async (ctx, next) => {
    ctx.tx = await signer.sign(ctx.tx)
    await next()
  }
}

export interface PriceMetadata extends MiddlewareMetadata {
  /** Latest quote from the price feed, as an exact ratio. */
  quote?: PriceQuote
}

/** Stores the feed's latest quote in `metadata.quote`. */
export function priceFeedMiddleware<TTransaction extends Transaction = Transaction>(
  getQuote: () => Promise<PriceQuote>,
): Middleware<TTransaction, PriceMetadata> {
  return async (ctx, next) => {
    ctx.metadata.quote = await getQuote()
    await next()
  }
}
