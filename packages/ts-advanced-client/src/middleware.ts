import type {
  Middleware,
  MiddlewareContext,
  MiddlewareMetadata,
  PriceMetadata,
  PriceQuote,
  Transaction,
} from './core.js'

// -------------------------------------------------------------------------------------------------------
// Validation
// -------------------------------------------------------------------------------------------------------

export type ValidationErrorCode =
  | 'INVALID_AMOUNT'
  | 'DEADLINE_PASSED'
  | 'SLIPPAGE_EXCEEDED'

export class ValidationError extends Error {
  constructor(
    readonly code: ValidationErrorCode,
    message: string,
  ) {
    super(message)
    this.name = 'ValidationError'
  }
}

export interface ValidationInput {
  /** Amount to validate (i128-shaped bigint). */
  amount: bigint
  /** Deadline in Unix milliseconds. */
  deadline?: number
  /** Quote to check slippage against. */
  quote?: PriceQuote
  /** Minimum acceptable ratio (numerator/denominator). */
  minRatio: PriceQuote
}

export type ValidationRule = (input: ValidationInput) => void

/** Rejects an amount that is not strictly positive. */
export const nonZeroAmountRule: ValidationRule = (input) => {
  if (input.amount <= 0n) {
    throw new ValidationError(
      'INVALID_AMOUNT',
      `Amount must be positive (got ${String(input.amount)})`,
    )
  }
}

/** @deprecated use nonZeroAmountRule */
export const nonFundAmountRule = nonZeroAmountRule

/** Rejects a deadline that is at or before the injected clock. */
export function deadlineRule(clock: () => number): ValidationRule {
  return (input) => {
    if (input.deadline === undefined) return
    if (input.deadline <= clock()) {
      throw new ValidationError(
        'DEADLINE_PASSED',
        `Deadline ${String(input.deadline)} is not in the future`,
      )
    }
  }
}

/** Rejects a quote that violates the slippage bound (minRatio). */
export const slippageRule: ValidationRule = (input) => {
  const { quote, minRatio } = input
  if (!quote) return
  if (quote.denominator === 0n || minRatio.denominator === 0n) {
    throw new ValidationError('SLIPPAGE_EXCEEDED', 'Quote denominator must be non-zero')
  }
  // quote.numerator / quote.denominator >= minRatio.numerator / minRatio.denominator
  // => quote.num * minRatio.den >= minRatio.num * quote.den
  const left = quote.numerator * minRatio.denominator
  const right = minRatio.numerator * quote.denominator
  if (left < right) {
    throw new ValidationError(
      'SLIPPAGE_EXCEEDED',
      `Quote ${String(quote.numerator)}/${String(quote.denominator)} is below min ${String(minRatio.numerator)}/${String(minRatio.denominator)}`,
    )
  }
}

export interface ValidationMetadata extends PriceMetadata {
  amount?: bigint
  deadline?: number
  minRatio?: PriceQuote
}

export interface ValidationOptions<TTransaction extends Transaction> {
  /**
   * Picks the values to validate, e.g. from the transaction itself. Whatever it
   * returns overrides the same-named `ctx.metadata` entries.
   */
  select?: (
    ctx: MiddlewareContext<TTransaction, ValidationMetadata>,
  ) => Partial<ValidationInput>
}

/**
 * Builds a middleware that runs each rule against the amount, deadline, quote
 * and `minRatio` found in `ctx.metadata` (or returned by `options.select`).
 * A rule that throws a `ValidationError` aborts the chain with that error
 * before `next()` runs, so nothing downstream, including submission, happens.
 * Valid input passes through unchanged.
 */
export function validationMiddleware<TTransaction extends Transaction>(
  rules: ReadonlyArray<ValidationRule>,
  options: ValidationOptions<TTransaction> = {},
): Middleware<TTransaction, ValidationMetadata> {
  return async (ctx, next) => {
    const input: ValidationInput = {
      amount: ctx.metadata.amount ?? 0n,
      deadline: ctx.metadata.deadline,
      quote: ctx.metadata.quote,
      minRatio: ctx.metadata.minRatio ?? { numerator: 0n, denominator: 1n },
      ...options.select?.(ctx),
    }
    for (const rule of rules) {
      rule(input)
    }
    await next()
  }
}

// -------------------------------------------------------------------------------------------------------
// Retry
// -------------------------------------------------------------------------------------------------------

export interface RetryOptions {
  /** Maximum number of attempts (including the first). */
  attempts: number
  /** Backoff before the second attempt in milliseconds; doubles on every further attempt. */
  backoffMs: number
  /** Predicate deciding whether an error is retryable. */
  retryOn: (error: unknown) => boolean
}

/** Wraps the downstream chain, submission included, in a retry loop. The chain
 *  is re-run from the point of this middleware on each attempt, waiting
 *  `backoffMs * 2^(attempt - 1)` between attempts. Gives up with the last
 *  error after `attempts` tries or as soon as `retryOn` returns `false`. */
export function retryMiddleware<TTransaction extends Transaction>(
  options: RetryOptions,
): Middleware<TTransaction, MiddlewareMetadata> {
  const { attempts, backoffMs, retryOn } = options
  return async (_ctx, next) => {
    for (let attempt = 1; ; attempt++) {
      try {
        await next()
        return
      } catch (error) {
        if (attempt >= attempts || !retryOn(error)) {
          throw error
        }
        await sleep(backoffMs * 2 ** (attempt - 1))
      }
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

// -------------------------------------------------------------------------------------------------------
// Logging
// -------------------------------------------------------------------------------------------------------

export interface LogSink {
  info(message: string, data?: unknown): void
  error(message: string, data?: unknown): void
}

export function loggingMiddleware<TTransaction extends Transaction>(
  sink: LogSink,
): Middleware<TTransaction, MiddlewareMetadata> {
  return async (ctx, next) => {
    sink.info('middleware:begin', { tx: ctx.tx })
    try {
      await next()
      sink.info('middleware:end')
    } catch (error) {
      sink.error('middleware:error', error)
      throw error
    }
  }
}
