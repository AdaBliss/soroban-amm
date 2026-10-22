package gosdk

import (
	"math/big"
	"time"
)

// Config configures a Client. RPCURL and NetworkPassphrase are required.
type Config struct {
	// RPCURL is the Soroban JSON-RPC endpoint, e.g.
	// https://soroban-testnet.stellar.org.
	RPCURL string
	// NetworkPassphrase identifies the network, e.g.
	// "Test SDF Network ; September 2015".
	NetworkPassphrase string
	// Timeout bounds a single RPC round trip. Defaults to DefaultTimeout when
	// zero. Ignored if HTTPClient is supplied with its own timeout.
	Timeout time.Duration
	// SourceAccount is the account used to build simulation transactions. Read
	// methods never submit, so this account is not charged and need not be the
	// signer. Defaults to DefaultSimulationAccount.
	SourceAccount string
	// Signer signs write transactions. Read methods work without one; write
	// methods return ErrNoSigner if it is nil.
	Signer Signer
	// TransactionTimeout sets the submitted transaction's upper time bound.
	// Defaults to five minutes.
	TransactionTimeout time.Duration
	// ResourceFeeMultiplier is applied to simulation's minimum resource fee.
	// Values below 1 are rejected; the default is 1.15.
	ResourceFeeMultiplier float64
}

// Network passphrases for the public networks.
const (
	// NetworkTestnet is the Stellar testnet passphrase.
	NetworkTestnet = "Test SDF Network ; September 2015"
	// NetworkPublic is the Stellar public network passphrase.
	NetworkPublic = "Public Global Stellar Network ; September 2015"
	// NetworkFuturenet is the Stellar futurenet passphrase.
	NetworkFuturenet = "Test SDF Future Network ; October 2022"
)

// DefaultTimeout bounds a single RPC round trip when Config.Timeout is zero.
const DefaultTimeout = 30 * time.Second

// DefaultSimulationAccount is the null account used to build simulation
// transactions when Config.SourceAccount is empty. Simulation neither consumes
// its sequence number nor charges it, so the all-zero account id is safe here
// and avoids depending on any funded account existing.
//
// Note: packages/sdk hardcodes a 55-character string for the same purpose,
// which is one character short of a valid strkey. Do not copy it here.
const DefaultSimulationAccount = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"

// PoolInfo mirrors the contract's PoolInfo struct
// (contracts/amm/src/lib.rs). Field names follow Go convention; the
// wire-format keys are the snake_case names from the contract.
type PoolInfo struct {
	// TokenA is the first pool token's contract address.
	TokenA string
	// TokenB is the second pool token's contract address.
	TokenB string
	// ReserveA is the current reserve of TokenA.
	ReserveA *big.Int
	// ReserveB is the current reserve of TokenB.
	ReserveB *big.Int
	// TotalShares is the total supply of LP shares.
	TotalShares *big.Int
	// FeeBps is the total swap fee in basis points.
	FeeBps *big.Int
	// FlashLoanFeeBps is the flash loan fee in basis points.
	FlashLoanFeeBps *big.Int
	// Admin is the pool administrator's address.
	Admin string
	// FeeRecipient receives the protocol's share of fees.
	FeeRecipient string
	// ProtocolFeeBps is the protocol's cut, in basis points, of FeeBps.
	ProtocolFeeBps *big.Int
	// LpRebateBps is the fraction of the protocol fee rebated to LP reserves.
	LpRebateBps *big.Int
}

// SwapQuote mirrors the contract's SwapSimulation struct
// (contracts/amm/src/lib.rs).
type SwapQuote struct {
	// AmountOut is the tokens received for the quoted input.
	AmountOut *big.Int
	// FeeAmount is the fee deducted from the input.
	FeeAmount *big.Int
	// PriceImpactBps is the price impact in basis points.
	PriceImpactBps *big.Int
	// EffectivePrice is amount_out/amount_in scaled by 1_000_000.
	EffectivePrice *big.Int
	// SpotPrice is reserve_out/reserve_in scaled by 1_000_000.
	SpotPrice *big.Int
}

// Reserves is the pair of pool reserves returned by GetReserves.
type Reserves struct {
	// ReserveA is the current reserve of the pool's first token.
	ReserveA *big.Int
	// ReserveB is the current reserve of the pool's second token.
	ReserveB *big.Int
}

// SwapParams are the arguments to Client.Swap. Deadline and MinOut are safety
// parameters and are never defaulted.
type SwapParams struct {
	// PoolID is the AMM pool contract address.
	PoolID string
	// Trader is the address whose tokens are swapped; it must match the Signer.
	Trader string
	// TokenIn is the contract address of the token being sold.
	TokenIn string
	// AmountIn is the quantity of TokenIn to sell.
	AmountIn *big.Int
	// MinOut is the slippage bound: the swap reverts with ErrSlippageExceeded
	// if the output would fall below it.
	MinOut *big.Int
	// Deadline is a ledger timestamp (seconds); the swap reverts with
	// ErrDeadlineExceeded once it passes.
	Deadline uint64
}

// AddLiquidityParams are the arguments to Client.AddLiquidity.
type AddLiquidityParams struct {
	// PoolID is the AMM pool contract address.
	PoolID string
	// Provider is the liquidity provider's address; it must match the Signer.
	Provider string
	// AmountA is the desired deposit of the pool's first token.
	AmountA *big.Int
	// AmountB is the desired deposit of the pool's second token.
	AmountB *big.Int
	// MinShares is the slippage bound on minted LP shares.
	MinShares *big.Int
	// Deadline is a ledger timestamp (seconds).
	Deadline uint64
}

// RemoveLiquidityParams are the arguments to Client.RemoveLiquidity.
type RemoveLiquidityParams struct {
	// PoolID is the AMM pool contract address.
	PoolID string
	// Provider is the liquidity provider's address; it must match the Signer.
	Provider string
	// Shares is the quantity of LP shares to burn.
	Shares *big.Int
	// MinA is the slippage bound on the first token returned.
	MinA *big.Int
	// MinB is the slippage bound on the second token returned.
	MinB *big.Int
	// Deadline is a ledger timestamp (seconds).
	Deadline uint64
}

// LiquidityResult is the pair of token amounts returned by RemoveLiquidity.
type LiquidityResult struct {
	// AmountA is the quantity of the first token returned.
	AmountA *big.Int
	// AmountB is the quantity of the second token returned.
	AmountB *big.Int
}

// TxResult describes a submitted transaction that reached a terminal status.
type TxResult struct {
	// Hash is the transaction hash.
	Hash string
	// Status is the terminal status reported by the RPC endpoint, normally
	// "SUCCESS".
	Status string
	// ReturnValue is the base64 XDR of the invocation's return value, when the
	// endpoint supplies one.
	ReturnValue string
	// DecodedReturnValue is ReturnValue decoded as an ScVal when present.
	DecodedReturnValue *ScValue
	// DiagnosticEvents contains diagnostic event XDR returned for a failed
	// transaction. It never contains signing material.
	DiagnosticEvents []string
}
