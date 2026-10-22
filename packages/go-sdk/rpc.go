package gosdk

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"math"
	"net/http"
	"strconv"
	"strings"
	"time"
)

// rpcRequest is a JSON-RPC 2.0 request envelope.
type rpcRequest struct {
	JSONRPC string      `json:"jsonrpc"`
	ID      int         `json:"id"`
	Method  string      `json:"method"`
	Params  interface{} `json:"params,omitempty"`
}

// rpcError is a JSON-RPC 2.0 error object.
type rpcError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

// rpcResponse is a JSON-RPC 2.0 response envelope with a deferred result.
type rpcResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      int             `json:"id"`
	Result  json.RawMessage `json:"result"`
	Error   *rpcError       `json:"error"`
}

// SimulateResult is the subset of a simulateTransaction response this client
// consumes.
type SimulateResult struct {
	// Error carries the simulation failure text, empty on success.
	Error string `json:"error"`
	// Results holds one entry per host function invoked; XDR is the base64
	// return value.
	Results []struct {
		XDR  string   `json:"xdr"`
		Auth []string `json:"auth"`
	} `json:"results"`
	// LatestLedger is the ledger the simulation ran against.
	LatestLedger uint32 `json:"latestLedger"`
	// TransactionData is the base64 SorobanTransactionData to attach to the
	// transaction before submission.
	TransactionData string `json:"transactionData"`
	// MinResourceFee is the additional resource fee simulation computed.
	MinResourceFee string `json:"minResourceFee"`
	// RestorePreamble is present when archived ledger entries must be restored
	// before the invocation can be simulated successfully.
	RestorePreamble *struct {
		TransactionData string `json:"transactionData"`
		MinResourceFee  string `json:"minResourceFee"`
	} `json:"restorePreamble"`
}

// SendResult is the subset of a sendTransaction response this client consumes.
type SendResult struct {
	// Status is PENDING, DUPLICATE, TRY_AGAIN_LATER or ERROR.
	Status string `json:"status"`
	// Hash is the submitted transaction's hash.
	Hash string `json:"hash"`
	// ErrorResultXDR carries the failure detail when Status is ERROR.
	ErrorResultXDR string `json:"errorResultXdr"`
}

// GetTxResult is the subset of a getTransaction response this client consumes.
type GetTxResult struct {
	// Status is NOT_FOUND, SUCCESS or FAILED.
	Status string `json:"status"`
	// ReturnValue is the base64 XDR of the invocation's return value.
	ReturnValue string `json:"returnValue"`
	// ResultXDR carries the failure detail when Status is FAILED.
	ResultXDR string `json:"resultXdr"`
	// DiagnosticEvents contains base64 diagnostic-event XDR for failures.
	DiagnosticEvents []string `json:"diagnosticEvents"`
}

type ledgerEntriesResult struct {
	Entries []struct {
		XDR string `json:"xdr"`
	} `json:"entries"`
}

// call performs one JSON-RPC round trip and unmarshals the result into out.
func (c *Client) call(ctx context.Context, method string, params interface{}, out interface{}) error {
	body, err := json.Marshal(rpcRequest{JSONRPC: "2.0", ID: 1, Method: method, Params: params})
	if err != nil {
		return fmt.Errorf("%w: encoding %s request: %v", ErrRPC, method, err)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.rpcURL, bytes.NewReader(body))
	if err != nil {
		return fmt.Errorf("%w: building %s request: %v", ErrRPC, method, err)
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.http.Do(req)
	if err != nil {
		// Wrap the transport error as well, so callers can tell a cancelled or
		// timed-out context apart from an endpoint failure with errors.Is.
		return fmt.Errorf("%w: %s: %w", ErrRPC, method, err)
	}
	defer resp.Body.Close()

	raw, err := io.ReadAll(io.LimitReader(resp.Body, maxResponseBytes))
	if err != nil {
		return fmt.Errorf("%w: reading %s response: %v", ErrRPC, method, err)
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("%w: %s: http %d: %s", ErrRPC, method, resp.StatusCode, truncate(string(raw), 256))
	}

	var envelope rpcResponse
	if err := json.Unmarshal(raw, &envelope); err != nil {
		return fmt.Errorf("%w: decoding %s response: %v", ErrRPC, method, err)
	}
	if envelope.Error != nil {
		return fmt.Errorf("%w: %s: %d %s", ErrRPC, method, envelope.Error.Code, envelope.Error.Message)
	}
	if out == nil {
		return nil
	}
	if err := json.Unmarshal(envelope.Result, out); err != nil {
		return fmt.Errorf("%w: decoding %s result: %v", ErrRPC, method, err)
	}
	return nil
}

// maxResponseBytes caps how much of an RPC response is read, so a hostile or
// broken endpoint cannot exhaust memory.
const maxResponseBytes = 16 << 20

// simulate invokes a contract method read-only and returns the base64 XDR of
// its return value. A contract-returned error is decoded into a *ContractError.
func (c *Client) simulate(ctx context.Context, contractID, method string, args []ScValue) (string, error) {
	if !IsContractAddress(contractID) {
		return "", fmt.Errorf("%w: contract id %q", ErrInvalidConfig, contractID)
	}

	envelope, err := BuildInvokeEnvelope(InvokeSpec{
		SourceAccount:     c.sourceAccount,
		ContractID:        contractID,
		Method:            method,
		Args:              args,
		NetworkPassphrase: c.networkPassphrase,
	})
	if err != nil {
		return "", err
	}

	var sim SimulateResult
	if err := c.call(ctx, "simulateTransaction", map[string]interface{}{"transaction": envelope}, &sim); err != nil {
		return "", err
	}
	if sim.Error != "" {
		return "", DecodeContractError(sim.Error)
	}
	if len(sim.Results) == 0 || sim.Results[0].XDR == "" {
		return "", fmt.Errorf("%w: %s returned no result", ErrRPC, method)
	}
	return sim.Results[0].XDR, nil
}

// invoke simulates, signs, submits and polls a state-changing contract call.
func (c *Client) invoke(ctx context.Context, contractID, method string, args []ScValue) (*TxResult, error) {
	if c.signer == nil {
		return nil, ErrNoSigner
	}
	if !IsContractAddress(contractID) {
		return nil, fmt.Errorf("%w: contract id %q", ErrInvalidConfig, contractID)
	}

	sequence, err := c.getAccountSequence(ctx, c.signer.Address())
	if err != nil {
		return nil, err
	}
	maxTime := uint64(time.Now().Add(c.transactionTimeout).Unix())
	spec := InvokeSpec{
		SourceAccount:     c.signer.Address(),
		ContractID:        contractID,
		Method:            method,
		Args:              args,
		NetworkPassphrase: c.networkPassphrase,
		Sequence:          sequence + 1,
		TimeoutSeconds:    maxTime,
	}
	envelope, err := BuildInvokeEnvelope(spec)
	if err != nil {
		return nil, err
	}

	var sim SimulateResult
	if err := c.call(ctx, "simulateTransaction", map[string]interface{}{"transaction": envelope}, &sim); err != nil {
		return nil, err
	}
	if sim.Error != "" {
		return nil, DecodeContractErrorFor(ContractAmm, sim.Error)
	}
	if sim.RestorePreamble != nil {
		if err := c.restoreFootprint(ctx, sequence+1, maxTime, sim.RestorePreamble); err != nil {
			return nil, err
		}
		sequence, err = c.getAccountSequence(ctx, c.signer.Address())
		if err != nil {
			return nil, err
		}
		spec.Sequence = sequence + 1
		envelope, err = BuildInvokeEnvelope(spec)
		if err != nil {
			return nil, err
		}
		sim = SimulateResult{}
		if err := c.call(ctx, "simulateTransaction", map[string]interface{}{"transaction": envelope}, &sim); err != nil {
			return nil, err
		}
		if sim.Error != "" {
			return nil, DecodeContractErrorFor(ContractAmm, sim.Error)
		}
		if sim.RestorePreamble != nil {
			return nil, fmt.Errorf("%w: state remains archived after restore", ErrArchivedState)
		}
	}
	if len(sim.Results) == 0 {
		return nil, fmt.Errorf("%w: %s simulation returned no invocation result", ErrRPC, method)
	}
	if sim.TransactionData == "" {
		return nil, fmt.Errorf("%w: %s simulation returned no transaction data", ErrRPC, method)
	}
	fee, err := c.resourceFee(sim.MinResourceFee)
	if err != nil {
		return nil, err
	}
	spec.Fee = BaseFee + fee
	spec.TransactionData = sim.TransactionData
	spec.Auth = sim.Results[0].Auth
	envelope, err = BuildInvokeEnvelope(spec)
	if err != nil {
		return nil, err
	}

	signed, err := c.signer.SignEnvelope(ctx, envelope, c.networkPassphrase)
	if err != nil {
		return nil, fmt.Errorf("signing %s: %w", method, err)
	}

	send, err := c.sendWithRetry(ctx, signed, method)
	if err != nil {
		return nil, err
	}

	return c.pollTransaction(ctx, send.Hash, method)
}

func (c *Client) resourceFee(raw string) (uint32, error) {
	fee, err := strconv.ParseUint(raw, 10, 32)
	if err != nil {
		return 0, fmt.Errorf("%w: invalid simulation resource fee %q", ErrRPC, raw)
	}
	adjusted := math.Ceil(float64(fee) * c.resourceFeeMultiplier)
	if adjusted > float64(math.MaxUint32-BaseFee) {
		return 0, fmt.Errorf("%w: resource fee exceeds transaction limit", ErrRPC)
	}
	return uint32(adjusted), nil
}

func (c *Client) getAccountSequence(ctx context.Context, account string) (int64, error) {
	raw, kind, err := DecodeStrkey(account)
	if err != nil || kind != StrkeyAccount {
		return 0, fmt.Errorf("%w: invalid source account", ErrInvalidConfig)
	}
	var key xdrWriter
	key.writeInt32(0) // LedgerEntryType::Account
	key.writeInt32(keyTypeEd25519)
	key.writeRaw(raw)
	encodedKey := base64.StdEncoding.EncodeToString(key.bytes())
	var result ledgerEntriesResult
	if err := c.call(ctx, "getLedgerEntries", map[string]interface{}{"keys": []string{encodedKey}}, &result); err != nil {
		return 0, err
	}
	if len(result.Entries) != 1 {
		return 0, fmt.Errorf("%w: source account was not found", ErrRPC)
	}
	entry, err := base64.StdEncoding.DecodeString(result.Entries[0].XDR)
	if err != nil {
		return 0, fmt.Errorf("%w: decoding account ledger entry: %v", ErrXDR, err)
	}
	r := xdrReader{buf: entry}
	if _, err = r.readUint32(); err != nil { // lastModifiedLedgerSeq
		return 0, err
	}
	entryType, err := r.readInt32()
	if err != nil || entryType != 0 {
		return 0, fmt.Errorf("%w: ledger entry is not an account", ErrXDR)
	}
	publicKeyType, err := r.readInt32()
	if err != nil || publicKeyType != keyTypeEd25519 {
		return 0, fmt.Errorf("%w: account uses an unsupported public key", ErrXDR)
	}
	if _, err = r.readRaw(32); err != nil {
		return 0, err
	}
	if _, err = r.readUint64(); err != nil { // balance
		return 0, err
	}
	seq, err := r.readUint64()
	return int64(seq), err
}

func (c *Client) restoreFootprint(ctx context.Context, sequence int64, maxTime uint64, preamble *struct {
	TransactionData string `json:"transactionData"`
	MinResourceFee  string `json:"minResourceFee"`
}) error {
	fee, err := c.resourceFee(preamble.MinResourceFee)
	if err != nil {
		return err
	}
	envelope, err := BuildRestoreFootprintEnvelope(RestoreSpec{
		SourceAccount: c.signer.Address(), TransactionData: preamble.TransactionData,
		Sequence: sequence, Fee: BaseFee + fee, MaxTime: maxTime,
	})
	if err != nil {
		return err
	}
	signed, err := c.signer.SignEnvelope(ctx, envelope, c.networkPassphrase)
	if err != nil {
		return fmt.Errorf("signing restore footprint: %w", err)
	}
	send, err := c.sendWithRetry(ctx, signed, "restore_footprint")
	if err != nil {
		return err
	}
	_, err = c.pollTransaction(ctx, send.Hash, "restore_footprint")
	return err
}

func (c *Client) sendWithRetry(ctx context.Context, signed, method string) (*SendResult, error) {
	for attempt := 0; ; attempt++ {
		var send SendResult
		if err := c.call(ctx, "sendTransaction", map[string]interface{}{"transaction": signed}, &send); err != nil {
			return nil, err
		}
		switch strings.ToUpper(send.Status) {
		case "PENDING", "DUPLICATE":
			return &send, nil
		case "TRY_AGAIN_LATER":
			if attempt >= c.pollAttempts {
				return nil, fmt.Errorf("%w: %s remained TRY_AGAIN_LATER", ErrTransactionFailed, method)
			}
			select {
			case <-ctx.Done():
				return nil, ctx.Err()
			case <-time.After(c.pollInterval):
			}
		case "ERROR":
			return nil, fmt.Errorf("%w: %s rejected: %s", ErrTransactionFailed, method, send.ErrorResultXDR)
		default:
			return nil, fmt.Errorf("%w: %s: unexpected send status %q", ErrTransactionFailed, method, send.Status)
		}
	}
}

// pollTransaction polls getTransaction until the transaction leaves NOT_FOUND
// or the context is cancelled.
func (c *Client) pollTransaction(ctx context.Context, hash, method string) (*TxResult, error) {
	for attempt := 0; ; attempt++ {
		var got GetTxResult
		if err := c.call(ctx, "getTransaction", map[string]interface{}{"hash": hash}, &got); err != nil {
			return nil, err
		}

		switch strings.ToUpper(got.Status) {
		case "SUCCESS":
			result := &TxResult{Hash: hash, Status: "SUCCESS", ReturnValue: got.ReturnValue}
			if got.ReturnValue != "" {
				if decoded, err := DecodeScValBase64(got.ReturnValue); err == nil {
					result.DecodedReturnValue = &decoded
				}
			}
			return result, nil
		case "FAILED":
			result := &TxResult{Hash: hash, Status: "FAILED", DiagnosticEvents: got.DiagnosticEvents}
			return result, fmt.Errorf("%w: %s: %s", ErrTransactionFailed, method, got.ResultXDR)
		case "NOT_FOUND":
			// Still in flight; fall through to the backoff below.
		default:
			return nil, fmt.Errorf("%w: %s: unexpected status %q", ErrTransactionFailed, method, got.Status)
		}

		if attempt >= c.pollAttempts {
			return nil, fmt.Errorf("%w: %s: not confirmed after %d polls", ErrTransactionFailed, method, attempt)
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(c.pollInterval):
		}
	}
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
