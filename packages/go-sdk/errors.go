package gosdk

import (
	"errors"
	"fmt"
	"regexp"
	"strconv"
	"strings"
)

// Contract error sentinels for contracts/amm's AmmError enum. The discriminants
// are documented in docs/error-codes.md; callers match with errors.Is rather
// than string comparison so the RPC wire format can change without breaking
// them.
var (
	// ErrAlreadyInitialized is AmmError::AlreadyInitialized (1).
	ErrAlreadyInitialized = errors.New("pool already initialized")
	// ErrInvalidFeeBps is AmmError::InvalidFeeBps (2).
	ErrInvalidFeeBps = errors.New("invalid fee bps")
	// ErrInsufficientShares is AmmError::InsufficientShares (3).
	ErrInsufficientShares = errors.New("insufficient shares")
	// ErrDeadlineExceeded is AmmError::DeadlineExceeded (4).
	ErrDeadlineExceeded = errors.New("deadline exceeded")
	// ErrSlippageExceeded is AmmError::SlippageExceeded (5).
	ErrSlippageExceeded = errors.New("slippage exceeded")
	// ErrPaused is AmmError::Paused (6).
	ErrPaused = errors.New("pool is paused")
	// ErrUnauthorized is AmmError::Unauthorized (7).
	ErrUnauthorized = errors.New("unauthorized")
	// ErrZeroAmount is AmmError::ZeroAmount (8).
	ErrZeroAmount = errors.New("zero amount")
	// ErrInvalidToken is AmmError::InvalidToken (9).
	ErrInvalidToken = errors.New("invalid token")
	// ErrEmptyPool is AmmError::EmptyPool (10).
	ErrEmptyPool = errors.New("empty pool")
	// ErrInsufficientLiquidity is AmmError::InsufficientLiquidity (11).
	ErrInsufficientLiquidity = errors.New("insufficient liquidity")
	// ErrNoPendingAdmin is AmmError::NoPendingAdmin (12).
	ErrNoPendingAdmin = errors.New("no pending admin")
	// ErrWrongAdmin is AmmError::WrongAdmin (13).
	ErrWrongAdmin = errors.New("wrong admin")
	// ErrReentrant is AmmError::Reentrant (14).
	ErrReentrant = errors.New("reentrant call")
	// ErrCircuitBreaker is AmmError::CircuitBreaker (15).
	ErrCircuitBreaker = errors.New("circuit breaker tripped")
	// ErrFotSlippage is AmmError::FotSlippage (16).
	ErrFotSlippage = errors.New("fee-on-transfer slippage exceeded")
	// ErrOracleDeviationExceeded is AmmError::OracleDeviationExceeded (17).
	ErrOracleDeviationExceeded = errors.New("oracle deviation exceeded")
	// ErrFlashLoanRepaymentFailed is AmmError::FlashLoanRepaymentFailed (18).
	ErrFlashLoanRepaymentFailed = errors.New("flash loan repayment failed")
	// ErrAlreadyExecuted is AmmError::AlreadyExecuted (19).
	ErrAlreadyExecuted = errors.New("proposal already executed")
	// ErrProposalExpired is AmmError::ProposalExpired (20).
	ErrProposalExpired = errors.New("proposal expired")
	// ErrNotInitialized is AmmError::NotInitialized (21).
	ErrNotInitialized = errors.New("pool not initialized")
)

// Client-side sentinels, distinct from contract errors.
var (
	// ErrInvalidConfig is returned by NewClient for a config that cannot
	// produce a usable client.
	ErrInvalidConfig = errors.New("invalid client config")
	// ErrNoSigner is returned when a write method is called on a client
	// constructed without a Signer.
	ErrNoSigner = errors.New("no signer configured")
	// ErrDeadlineRequired is returned when a write method is given a zero
	// deadline. Deadlines are a safety parameter and are never defaulted.
	ErrDeadlineRequired = errors.New("deadline required")
	// ErrSlippageRequired is returned when a write method is given a nil or
	// negative slippage bound.
	ErrSlippageRequired = errors.New("slippage bound required")
	// ErrTransactionFailed is returned when a submitted transaction reaches a
	// terminal non-success status.
	ErrTransactionFailed = errors.New("transaction failed")
	// ErrRPC is returned when the RPC endpoint reports a JSON-RPC error.
	ErrRPC = errors.New("rpc error")
	// ErrAuthFailed identifies a failed Soroban authorization check.
	ErrAuthFailed = errors.New("soroban authorization failed")
	// ErrWasmTrap identifies a contract WASM trap or invalid action.
	ErrWasmTrap = errors.New("soroban wasm trap")
	// ErrResourceLimit identifies an instruction, memory, or resource budget failure.
	ErrResourceLimit = errors.New("soroban resource limit exceeded")
	// ErrArchivedState identifies ledger state that must be restored before use.
	ErrArchivedState = errors.New("soroban state is archived")
)

// ContractKind selects the error table for the contract being called.
type ContractKind string

const (
	// ContractAmm is the constant-product AMM contract.
	ContractAmm ContractKind = "amm"
)

type contractErrorInfo struct {
	name     string
	sentinel error
}

// contractErrorsByKind prevents a numeric code from being mislabeled as an
// AMM error when it came from a different contract family.
var contractErrorsByKind = map[ContractKind]map[int]contractErrorInfo{
	ContractAmm: {
		1:  {"AlreadyInitialized", ErrAlreadyInitialized},
		2:  {"InvalidFeeBps", ErrInvalidFeeBps},
		3:  {"InsufficientShares", ErrInsufficientShares},
		4:  {"DeadlineExceeded", ErrDeadlineExceeded},
		5:  {"SlippageExceeded", ErrSlippageExceeded},
		6:  {"Paused", ErrPaused},
		7:  {"Unauthorized", ErrUnauthorized},
		8:  {"ZeroAmount", ErrZeroAmount},
		9:  {"InvalidToken", ErrInvalidToken},
		10: {"EmptyPool", ErrEmptyPool},
		11: {"InsufficientLiquidity", ErrInsufficientLiquidity},
		12: {"NoPendingAdmin", ErrNoPendingAdmin},
		13: {"WrongAdmin", ErrWrongAdmin},
		14: {"Reentrant", ErrReentrant},
		15: {"CircuitBreaker", ErrCircuitBreaker},
		16: {"FotSlippage", ErrFotSlippage},
		17: {"OracleDeviationExceeded", ErrOracleDeviationExceeded},
		18: {"FlashLoanRepaymentFailed", ErrFlashLoanRepaymentFailed},
		19: {"AlreadyExecuted", ErrAlreadyExecuted},
		20: {"ProposalExpired", ErrProposalExpired},
		21: {"NotInitialized", ErrNotInitialized},
	},
}

// ContractError is a decoded contract error. It wraps the matching sentinel so
// errors.Is works, and keeps the discriminant and the raw RPC text for logging.
type ContractError struct {
	// Code is the on-chain discriminant, or 0 if it could not be extracted.
	Code int
	// Raw is the unmodified error text returned by the RPC endpoint.
	Raw string
	// Kind is the contract family used to decode Code.
	Kind ContractKind

	sentinel error
}

// Error implements the error interface.
func (e *ContractError) Error() string {
	if e.sentinel != nil {
		return fmt.Sprintf("contract error %d: %s", e.Code, e.sentinel.Error())
	}
	return fmt.Sprintf("contract error: %s", e.Raw)
}

// Unwrap exposes the sentinel so errors.Is(err, ErrSlippageExceeded) matches.
func (e *ContractError) Unwrap() error { return e.sentinel }

// contractErrPattern matches the Error(Contract, #N) form Soroban RPC uses to
// report a contract-returned error.
var contractErrPattern = regexp.MustCompile(`Error\s*\(\s*Contract\s*,\s*#(\d+)\s*\)`)

var diagnosticErrors = []struct {
	patterns []string
	err      error
}{
	{[]string{"error(auth", "auth,", "unauthorized"}, ErrAuthFailed},
	{[]string{"wasmvm", "wasm trap", "unreachable"}, ErrWasmTrap},
	{[]string{"budget", "exceededlimit", "resource limit", "txinsufficientfee"}, ErrResourceLimit},
	{[]string{"archived", "restorefootprint", "entryarchived", "ttl"}, ErrArchivedState},
}

// DecodeContractError converts an RPC error string into a typed *ContractError.
// An unrecognised discriminant still yields a *ContractError so the caller
// keeps the code and raw text; only the sentinel is absent.
func DecodeContractError(raw string) *ContractError {
	return DecodeContractErrorFor(ContractAmm, raw)
}

// DecodeContractErrorFor decodes raw using only the selected contract's error
// table. Unknown contract kinds retain the numeric code without assigning an
// unrelated sentinel.
func DecodeContractErrorFor(kind ContractKind, raw string) *ContractError {
	ce := &ContractError{Raw: raw, Kind: kind}

	m := contractErrPattern.FindStringSubmatch(raw)
	if m == nil {
		lower := strings.ToLower(raw)
		for _, diagnostic := range diagnosticErrors {
			for _, pattern := range diagnostic.patterns {
				if strings.Contains(lower, pattern) {
					ce.sentinel = diagnostic.err
					return ce
				}
			}
		}
		return ce
	}

	code, err := strconv.Atoi(m[1])
	if err != nil {
		return ce
	}
	ce.Code = code
	if table := contractErrorsByKind[kind]; table != nil {
		ce.sentinel = table[code].sentinel
	}
	return ce
}
