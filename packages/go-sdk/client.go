// Package gosdk is the Go client for the Soroban AMM protocol. It mirrors the
// surface of the TypeScript SDK in packages/sdk: read methods run through
// simulation and need no signer, while write methods build, simulate, sign,
// submit and poll a transaction.
package gosdk

import (
	"context"
	"fmt"
	"math/big"
	"net/http"
	"time"
)

// Client talks to a Soroban RPC endpoint on behalf of one network.
type Client struct {
	rpcURL                string
	networkPassphrase     string
	sourceAccount         string
	signer                Signer
	http                  *http.Client
	pollInterval          time.Duration
	pollAttempts          int
	transactionTimeout    time.Duration
	resourceFeeMultiplier float64
}

// Default polling behaviour for submitted transactions.
const (
	// DefaultPollInterval is the delay between getTransaction polls.
	DefaultPollInterval = time.Second
	// DefaultPollAttempts bounds how many times a submitted transaction is
	// polled before the client gives up waiting.
	DefaultPollAttempts = 30
	// DefaultTransactionTimeout is the validity window for submitted writes.
	DefaultTransactionTimeout = 5 * time.Minute
	// DefaultResourceFeeMultiplier leaves a 15% margin above simulation.
	DefaultResourceFeeMultiplier = 1.15
)

// NewClient validates cfg and returns a ready client. A config that cannot
// produce a usable client is rejected with ErrInvalidConfig rather than
// yielding a half-built one.
func NewClient(cfg Config) (*Client, error) {
	if cfg.RPCURL == "" {
		return nil, fmt.Errorf("%w: RPCURL is required", ErrInvalidConfig)
	}
	if cfg.NetworkPassphrase == "" {
		return nil, fmt.Errorf("%w: NetworkPassphrase is required", ErrInvalidConfig)
	}

	source := cfg.SourceAccount
	if source == "" {
		source = DefaultSimulationAccount
	}
	if !IsAccountAddress(source) {
		return nil, fmt.Errorf("%w: source account %q", ErrInvalidConfig, source)
	}
	if cfg.Signer != nil {
		if err := ValidateSigner(cfg.Signer); err != nil {
			return nil, fmt.Errorf("%w: %v", ErrInvalidConfig, err)
		}
	}

	timeout := cfg.Timeout
	if timeout == 0 {
		timeout = DefaultTimeout
	}
	transactionTimeout := cfg.TransactionTimeout
	if transactionTimeout == 0 {
		transactionTimeout = DefaultTransactionTimeout
	}
	if transactionTimeout < 0 {
		return nil, fmt.Errorf("%w: TransactionTimeout must be positive", ErrInvalidConfig)
	}
	resourceFeeMultiplier := cfg.ResourceFeeMultiplier
	if resourceFeeMultiplier == 0 {
		resourceFeeMultiplier = DefaultResourceFeeMultiplier
	}
	if resourceFeeMultiplier < 1 {
		return nil, fmt.Errorf("%w: ResourceFeeMultiplier must be at least 1", ErrInvalidConfig)
	}

	return &Client{
		rpcURL:                cfg.RPCURL,
		networkPassphrase:     cfg.NetworkPassphrase,
		sourceAccount:         source,
		signer:                cfg.Signer,
		http:                  &http.Client{Timeout: timeout},
		pollInterval:          DefaultPollInterval,
		pollAttempts:          DefaultPollAttempts,
		transactionTimeout:    transactionTimeout,
		resourceFeeMultiplier: resourceFeeMultiplier,
	}, nil
}

// WithHTTPClient replaces the underlying HTTP client so callers control
// transport, proxying and timeouts. It returns the receiver for chaining.
func (c *Client) WithHTTPClient(h *http.Client) *Client {
	if h != nil {
		c.http = h
	}
	return c
}

// WithPolling overrides how a submitted transaction is polled for its result.
func (c *Client) WithPolling(interval time.Duration, attempts int) *Client {
	if interval > 0 {
		c.pollInterval = interval
	}
	if attempts > 0 {
		c.pollAttempts = attempts
	}
	return c
}

// HasSigner reports whether write methods are available on this client.
func (c *Client) HasSigner() bool { return c.signer != nil }

// ── Read methods ──────────────────────────────────────────────────────────────

// GetInfo returns the pool's full state. Read-only: it runs through simulation
// and needs no signer.
func (c *Client) GetInfo(ctx context.Context, poolID string) (*PoolInfo, error) {
	raw, err := c.simulate(ctx, poolID, "get_info", nil)
	if err != nil {
		return nil, err
	}
	v, err := DecodeScValBase64(raw)
	if err != nil {
		return nil, err
	}
	return parsePoolInfo(v)
}

// SimulateSwap quotes a swap without submitting it, returning the output
// amount alongside the fee and price impact. Read-only.
func (c *Client) SimulateSwap(ctx context.Context, poolID, tokenIn string, amountIn *big.Int) (*SwapQuote, error) {
	if err := requirePositive(amountIn, "amountIn"); err != nil {
		return nil, err
	}
	raw, err := c.simulate(ctx, poolID, "simulate_swap", []ScValue{Addr(tokenIn), I128(amountIn)})
	if err != nil {
		return nil, err
	}
	v, err := DecodeScValBase64(raw)
	if err != nil {
		return nil, err
	}
	return parseSwapQuote(v)
}

// GetAmountOut returns the output amount for a given input. Read-only.
func (c *Client) GetAmountOut(ctx context.Context, poolID, tokenIn string, amountIn *big.Int) (*big.Int, error) {
	if err := requirePositive(amountIn, "amountIn"); err != nil {
		return nil, err
	}
	return c.simulateBigInt(ctx, poolID, "get_amount_out", []ScValue{Addr(tokenIn), I128(amountIn)})
}

// GetAmountIn returns the input amount required for a given output. Read-only.
func (c *Client) GetAmountIn(ctx context.Context, poolID, tokenOut string, amountOut *big.Int) (*big.Int, error) {
	if err := requirePositive(amountOut, "amountOut"); err != nil {
		return nil, err
	}
	return c.simulateBigInt(ctx, poolID, "get_amount_in", []ScValue{Addr(tokenOut), I128(amountOut)})
}

// SharesOf returns a provider's LP share balance. Read-only.
func (c *Client) SharesOf(ctx context.Context, poolID, provider string) (*big.Int, error) {
	if !IsAccountAddress(provider) && !IsContractAddress(provider) {
		return nil, fmt.Errorf("%w: provider %q", ErrInvalidConfig, provider)
	}
	return c.simulateBigInt(ctx, poolID, "shares_of", []ScValue{Addr(provider)})
}

// GetReserves returns both pool reserves. Read-only.
func (c *Client) GetReserves(ctx context.Context, poolID string) (*Reserves, error) {
	info, err := c.GetInfo(ctx, poolID)
	if err != nil {
		return nil, err
	}
	return &Reserves{ReserveA: info.ReserveA, ReserveB: info.ReserveB}, nil
}

// ── Write methods ─────────────────────────────────────────────────────────────

// Swap sells AmountIn of TokenIn into the pool. It requires a Signer, and
// rejects a zero deadline or a missing slippage bound rather than defaulting
// either, since both are safety parameters.
func (c *Client) Swap(ctx context.Context, p SwapParams) (*TxResult, error) {
	if err := requirePositive(p.AmountIn, "AmountIn"); err != nil {
		return nil, err
	}
	if err := requireBound(p.MinOut, ErrSlippageRequired, "MinOut"); err != nil {
		return nil, err
	}
	if p.Deadline == 0 {
		return nil, fmt.Errorf("%w: Deadline must be a future ledger timestamp", ErrDeadlineRequired)
	}

	return c.invoke(ctx, p.PoolID, "swap", []ScValue{
		Addr(p.Trader),
		Addr(p.TokenIn),
		I128(p.AmountIn),
		I128(p.MinOut),
		U64(p.Deadline),
	})
}

// AddLiquidity deposits both tokens and mints LP shares. MinShares bounds
// slippage and Deadline bounds inclusion; neither is defaulted.
func (c *Client) AddLiquidity(ctx context.Context, p AddLiquidityParams) (*TxResult, error) {
	if err := requirePositive(p.AmountA, "AmountA"); err != nil {
		return nil, err
	}
	if err := requirePositive(p.AmountB, "AmountB"); err != nil {
		return nil, err
	}
	if err := requireBound(p.MinShares, ErrSlippageRequired, "MinShares"); err != nil {
		return nil, err
	}
	if p.Deadline == 0 {
		return nil, fmt.Errorf("%w: Deadline must be a future ledger timestamp", ErrDeadlineRequired)
	}

	return c.invoke(ctx, p.PoolID, "add_liquidity", []ScValue{
		Addr(p.Provider),
		I128(p.AmountA),
		I128(p.AmountB),
		I128(p.MinShares),
		U64(p.Deadline),
	})
}

// RemoveLiquidity burns LP shares and returns the underlying tokens. MinA and
// MinB bound slippage and Deadline bounds inclusion; none is defaulted.
func (c *Client) RemoveLiquidity(ctx context.Context, p RemoveLiquidityParams) (*TxResult, error) {
	if err := requirePositive(p.Shares, "Shares"); err != nil {
		return nil, err
	}
	if err := requireBound(p.MinA, ErrSlippageRequired, "MinA"); err != nil {
		return nil, err
	}
	if err := requireBound(p.MinB, ErrSlippageRequired, "MinB"); err != nil {
		return nil, err
	}
	if p.Deadline == 0 {
		return nil, fmt.Errorf("%w: Deadline must be a future ledger timestamp", ErrDeadlineRequired)
	}

	return c.invoke(ctx, p.PoolID, "remove_liquidity", []ScValue{
		Addr(p.Provider),
		I128(p.Shares),
		I128(p.MinA),
		I128(p.MinB),
		U64(p.Deadline),
	})
}

// ── Helpers ───────────────────────────────────────────────────────────────────

// simulateBigInt runs a read-only call whose return value is a single integer.
func (c *Client) simulateBigInt(ctx context.Context, contractID, method string, args []ScValue) (*big.Int, error) {
	raw, err := c.simulate(ctx, contractID, method, args)
	if err != nil {
		return nil, err
	}
	v, err := DecodeScValBase64(raw)
	if err != nil {
		return nil, err
	}
	return v.BigInt()
}

// requirePositive rejects a nil, zero or negative amount, matching the
// contract's own ZeroAmount guard but failing before the round trip.
func requirePositive(v *big.Int, name string) error {
	if v == nil || v.Sign() <= 0 {
		return fmt.Errorf("%w: %s must be positive", ErrZeroAmount, name)
	}
	return nil
}

// requireBound rejects a nil or negative slippage bound. Zero is permitted: it
// is an explicit choice to accept any output.
func requireBound(v *big.Int, sentinel error, name string) error {
	if v == nil || v.Sign() < 0 {
		return fmt.Errorf("%w: %s must be set to a non-negative value", sentinel, name)
	}
	return nil
}

// parsePoolInfo converts the contract's PoolInfo struct into its Go form.
func parsePoolInfo(v ScValue) (*PoolInfo, error) {
	info := &PoolInfo{}
	var err error

	if info.TokenA, err = mapFieldAddress(v, "token_a"); err != nil {
		return nil, err
	}
	if info.TokenB, err = mapFieldAddress(v, "token_b"); err != nil {
		return nil, err
	}
	if info.Admin, err = mapFieldAddress(v, "admin"); err != nil {
		return nil, err
	}
	if info.FeeRecipient, err = mapFieldAddress(v, "fee_recipient"); err != nil {
		return nil, err
	}

	for _, f := range []struct {
		name string
		dst  **big.Int
	}{
		{"reserve_a", &info.ReserveA},
		{"reserve_b", &info.ReserveB},
		{"total_shares", &info.TotalShares},
		{"fee_bps", &info.FeeBps},
		{"flash_loan_fee_bps", &info.FlashLoanFeeBps},
		{"protocol_fee_bps", &info.ProtocolFeeBps},
		{"lp_rebate_bps", &info.LpRebateBps},
	} {
		if *f.dst, err = mapFieldBigInt(v, f.name); err != nil {
			return nil, err
		}
	}
	return info, nil
}

// parseSwapQuote converts the contract's SwapSimulation struct into its Go form.
func parseSwapQuote(v ScValue) (*SwapQuote, error) {
	q := &SwapQuote{}
	var err error

	for _, f := range []struct {
		name string
		dst  **big.Int
	}{
		{"amount_out", &q.AmountOut},
		{"fee_amount", &q.FeeAmount},
		{"price_impact_bps", &q.PriceImpactBps},
		{"effective_price", &q.EffectivePrice},
		{"spot_price", &q.SpotPrice},
	} {
		if *f.dst, err = mapFieldBigInt(v, f.name); err != nil {
			return nil, err
		}
	}
	return q, nil
}
