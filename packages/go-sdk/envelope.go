package gosdk

import (
	"encoding/base64"
	"fmt"
)

// InvokeSpec describes the contract invocation an envelope should carry.
type InvokeSpec struct {
	// SourceAccount pays the fee and, for writes, is the signing account.
	SourceAccount string
	// ContractID is the invoked contract's address.
	ContractID string
	// Method is the contract function name.
	Method string
	// Args are the invocation arguments, in declaration order.
	Args []ScValue
	// NetworkPassphrase identifies the network the envelope targets.
	NetworkPassphrase string
	// Sequence is the source account's next sequence number. Simulation
	// ignores it, so it may be left zero for read-only calls.
	Sequence int64
	// Fee is the transaction fee in stroops. Defaults to BaseFee when zero;
	// simulation returns the resource fee that must be added before submission.
	Fee uint32
	// TimeoutSeconds is the envelope's absolute upper Unix timestamp. Zero
	// means no time bound.
	TimeoutSeconds uint64
	// MinLedgerTime is the lower time bound; normally zero.
	MinLedgerTime uint64
	// TransactionData is the base64 SorobanTransactionData returned by
	// simulateTransaction.
	TransactionData string
	// Auth contains base64 SorobanAuthorizationEntry values returned by
	// simulateTransaction.
	Auth []string
}

// RestoreSpec describes a RestoreFootprint transaction assembled from a
// simulation restore preamble.
type RestoreSpec struct {
	SourceAccount   string
	Sequence        int64
	Fee             uint32
	MaxTime         uint64
	TransactionData string
}

// BaseFee is the default per-operation fee in stroops.
const BaseFee uint32 = 100

// XDR constants for the envelope structure this client emits.
const (
	envelopeTypeTx      int32 = 2
	txPreconditionsTime int32 = 1
	txPreconditionsNone int32 = 0
	opTypeInvokeHostFn  int32 = 24
	opTypeRestoreFoot   int32 = 26
	hostFnTypeInvoke    int32 = 0
	keyTypeEd25519      int32 = 0
	muxedAccountEd25519 int32 = 0
)

// BuildInvokeEnvelope builds a base64-encoded TransactionEnvelope XDR carrying
// a single InvokeHostFunction operation. The envelope carries no signatures;
// a Signer adds those before submission.
func BuildInvokeEnvelope(spec InvokeSpec) (string, error) {
	if !IsAccountAddress(spec.SourceAccount) {
		return "", fmt.Errorf("%w: source account %q", ErrInvalidConfig, spec.SourceAccount)
	}
	if !IsContractAddress(spec.ContractID) {
		return "", fmt.Errorf("%w: contract id %q", ErrInvalidConfig, spec.ContractID)
	}
	if spec.Method == "" {
		return "", fmt.Errorf("%w: empty method name", ErrInvalidConfig)
	}
	if len(spec.Method) > 32 {
		return "", fmt.Errorf("%w: method %q exceeds the 32-character symbol limit", ErrInvalidConfig, spec.Method)
	}

	sourceRaw, kind, err := DecodeStrkey(spec.SourceAccount)
	if err != nil {
		return "", err
	}
	if kind != StrkeyAccount {
		return "", fmt.Errorf("%w: source %q is not an account address", ErrInvalidConfig, spec.SourceAccount)
	}

	fee := spec.Fee
	if fee == 0 {
		fee = BaseFee
	}

	var w xdrWriter

	// TransactionEnvelope: union discriminated on EnvelopeType.
	w.writeInt32(envelopeTypeTx)

	// Transaction.sourceAccount: MuxedAccount union.
	w.writeInt32(muxedAccountEd25519)
	w.writeRaw(sourceRaw)

	w.writeUint32(fee)
	w.writeInt64(spec.Sequence)

	// Transaction.cond: Preconditions union.
	if spec.TimeoutSeconds > 0 || spec.MinLedgerTime > 0 {
		w.writeInt32(txPreconditionsTime)
		w.writeUint64(spec.MinLedgerTime)
		w.writeUint64(spec.TimeoutSeconds)
	} else {
		w.writeInt32(txPreconditionsNone)
	}

	// Transaction.memo: MEMO_NONE.
	w.writeInt32(0)

	// Transaction.operations: exactly one.
	w.writeUint32(1)

	// Operation.sourceAccount: absent, so the transaction source is used.
	w.writeUint32(0)

	// Operation.body: InvokeHostFunction.
	w.writeInt32(opTypeInvokeHostFn)
	w.writeInt32(hostFnTypeInvoke)

	// InvokeContractArgs.contractAddress: SCAddress union.
	contractRaw, kind, err := DecodeStrkey(spec.ContractID)
	if err != nil {
		return "", err
	}
	if kind != StrkeyContract {
		return "", fmt.Errorf("%w: %q is not a contract address", ErrInvalidConfig, spec.ContractID)
	}
	w.writeInt32(scAddressCntrct)
	w.writeRaw(contractRaw)

	// InvokeContractArgs.functionName: SCSymbol.
	w.writeBytes([]byte(spec.Method))

	// InvokeContractArgs.args.
	w.writeUint32(uint32(len(spec.Args)))
	for i, arg := range spec.Args {
		if err := writeScVal(&w, arg); err != nil {
			return "", fmt.Errorf("encoding argument %d of %s: %w", i, spec.Method, err)
		}
	}

	// InvokeHostFunctionOp.auth: simulation-provided authorization entries.
	w.writeUint32(uint32(len(spec.Auth)))
	for i, encoded := range spec.Auth {
		auth, err := base64.StdEncoding.DecodeString(encoded)
		if err != nil {
			return "", fmt.Errorf("%w: decoding auth entry %d: %v", ErrXDR, i, err)
		}
		w.writeRaw(auth)
	}

	if spec.TransactionData == "" {
		w.writeInt32(0)
	} else {
		transactionData, err := base64.StdEncoding.DecodeString(spec.TransactionData)
		if err != nil {
			return "", fmt.Errorf("%w: decoding transaction data: %v", ErrXDR, err)
		}
		w.writeInt32(1)
		w.writeRaw(transactionData)
	}

	// TransactionV1Envelope.signatures: none yet.
	w.writeUint32(0)

	return base64.StdEncoding.EncodeToString(w.bytes()), nil
}

// BuildRestoreFootprintEnvelope builds an unsigned transaction from a
// simulation restore preamble.
func BuildRestoreFootprintEnvelope(spec RestoreSpec) (string, error) {
	if !IsAccountAddress(spec.SourceAccount) {
		return "", fmt.Errorf("%w: source account %q", ErrInvalidConfig, spec.SourceAccount)
	}
	transactionData, err := base64.StdEncoding.DecodeString(spec.TransactionData)
	if err != nil || len(transactionData) == 0 {
		return "", fmt.Errorf("%w: invalid restore transaction data", ErrXDR)
	}
	sourceRaw, _, err := DecodeStrkey(spec.SourceAccount)
	if err != nil {
		return "", err
	}
	fee := spec.Fee
	if fee == 0 {
		fee = BaseFee
	}

	var w xdrWriter
	w.writeInt32(envelopeTypeTx)
	w.writeInt32(muxedAccountEd25519)
	w.writeRaw(sourceRaw)
	w.writeUint32(fee)
	w.writeInt64(spec.Sequence)
	if spec.MaxTime > 0 {
		w.writeInt32(txPreconditionsTime)
		w.writeUint64(0)
		w.writeUint64(spec.MaxTime)
	} else {
		w.writeInt32(txPreconditionsNone)
	}
	w.writeInt32(0) // MEMO_NONE
	w.writeUint32(1)
	w.writeUint32(0) // operation source absent
	w.writeInt32(opTypeRestoreFoot)
	w.writeInt32(1) // Transaction.ext v1
	w.writeRaw(transactionData)
	w.writeUint32(0) // signatures
	return base64.StdEncoding.EncodeToString(w.bytes()), nil
}
