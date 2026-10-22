package gosdk

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

// recordedRPC serves canned JSON-RPC responses keyed by method, so every test
// runs without network access.
type recordedRPC struct {
	t        *testing.T
	handlers map[string]func(params json.RawMessage) (interface{}, *rpcError)
	calls    []string
	requests map[string]json.RawMessage
}

func newRecordedRPC(t *testing.T) *recordedRPC {
	r := &recordedRPC{
		t:        t,
		handlers: map[string]func(json.RawMessage) (interface{}, *rpcError){},
		requests: map[string]json.RawMessage{},
	}
	r.respond("getLedgerEntries", map[string]interface{}{"entries": []map[string]string{{"xdr": accountEntryXDR(t, 42)}}})
	return r
}

func accountEntryXDR(t *testing.T, sequence int64) string {
	t.Helper()
	var w xdrWriter
	w.writeUint32(100) // last modified ledger
	w.writeInt32(0)    // LedgerEntryType::Account
	w.writeInt32(keyTypeEd25519)
	w.writeRaw(make([]byte, 32))
	w.writeInt64(0) // balance
	w.writeInt64(sequence)
	return base64.StdEncoding.EncodeToString(w.bytes())
}

func (r *recordedRPC) on(method string, fn func(params json.RawMessage) (interface{}, *rpcError)) *recordedRPC {
	r.handlers[method] = fn
	return r
}

func (r *recordedRPC) respond(method string, result interface{}) *recordedRPC {
	return r.on(method, func(json.RawMessage) (interface{}, *rpcError) { return result, nil })
}

func (r *recordedRPC) ServeHTTP(w http.ResponseWriter, req *http.Request) {
	body, _ := io.ReadAll(req.Body)

	var in struct {
		Method string          `json:"method"`
		Params json.RawMessage `json:"params"`
		ID     int             `json:"id"`
	}
	if err := json.Unmarshal(body, &in); err != nil {
		r.t.Fatalf("server received malformed request: %v", err)
	}
	r.calls = append(r.calls, in.Method)
	r.requests[in.Method] = in.Params

	handler, ok := r.handlers[in.Method]
	if !ok {
		r.t.Fatalf("server received an unexpected method %q", in.Method)
	}
	result, rpcErr := handler(in.Params)

	out := map[string]interface{}{"jsonrpc": "2.0", "id": in.ID}
	if rpcErr != nil {
		out["error"] = rpcErr
	} else {
		out["result"] = result
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(out)
}

// newTestClient wires a client to a recorded server with fast polling.
func newTestClient(t *testing.T, rec *recordedRPC, signer Signer) *Client {
	t.Helper()
	srv := httptest.NewServer(rec)
	t.Cleanup(srv.Close)

	c, err := NewClient(Config{
		RPCURL:            srv.URL,
		NetworkPassphrase: NetworkTestnet,
		Signer:            signer,
	})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	return c.WithPolling(time.Millisecond, 5)
}

// simulationOf wraps an ScValue as a successful simulateTransaction result.
func simulationOf(t *testing.T, v ScValue) map[string]interface{} {
	t.Helper()
	encoded, err := EncodeScValBase64(v)
	if err != nil {
		t.Fatalf("encoding simulation result: %v", err)
	}
	return map[string]interface{}{
		"results":         []map[string]interface{}{{"xdr": encoded, "auth": []string{}}},
		"latestLedger":    100,
		"minResourceFee":  "0",
		"transactionData": base64.StdEncoding.EncodeToString([]byte{0, 0, 0, 1}),
	}
}

func poolInfoScVal() ScValue {
	return Map(
		ScMapEntry{Key: Symbol("admin"), Val: Addr(zeroAccount)},
		ScMapEntry{Key: Symbol("fee_bps"), Val: I128(big.NewInt(30))},
		ScMapEntry{Key: Symbol("fee_recipient"), Val: Addr(zeroAccount)},
		ScMapEntry{Key: Symbol("flash_loan_fee_bps"), Val: I128(big.NewInt(5))},
		ScMapEntry{Key: Symbol("lp_rebate_bps"), Val: I128(big.NewInt(1000))},
		ScMapEntry{Key: Symbol("protocol_fee_bps"), Val: I128(big.NewInt(5))},
		ScMapEntry{Key: Symbol("reserve_a"), Val: I128(big.NewInt(1_000_000))},
		ScMapEntry{Key: Symbol("reserve_b"), Val: I128(big.NewInt(2_000_000))},
		ScMapEntry{Key: Symbol("token_a"), Val: Addr(zeroContract)},
		ScMapEntry{Key: Symbol("token_b"), Val: Addr(zeroContract)},
		ScMapEntry{Key: Symbol("total_shares"), Val: I128(big.NewInt(1_414_213))},
	)
}

func swapQuoteScVal() ScValue {
	return Map(
		ScMapEntry{Key: Symbol("amount_out"), Val: I128(big.NewInt(1_980))},
		ScMapEntry{Key: Symbol("effective_price"), Val: I128(big.NewInt(1_980_000))},
		ScMapEntry{Key: Symbol("fee_amount"), Val: I128(big.NewInt(3))},
		ScMapEntry{Key: Symbol("price_impact_bps"), Val: I128(big.NewInt(12))},
		ScMapEntry{Key: Symbol("spot_price"), Val: I128(big.NewInt(2_000_000))},
	)
}

// ── Config validation ─────────────────────────────────────────────────────────

func TestNewClientValidatesConfig(t *testing.T) {
	cases := map[string]Config{
		"no rpc url":    {NetworkPassphrase: NetworkTestnet},
		"no passphrase": {RPCURL: "http://localhost:8000"},
		"bad source": {
			RPCURL:            "http://localhost:8000",
			NetworkPassphrase: NetworkTestnet,
			SourceAccount:     "not-an-account",
		},
		"bad signer address": {
			RPCURL:            "http://localhost:8000",
			NetworkPassphrase: NetworkTestnet,
			Signer:            SignerFunc{Addr: "nope"},
		},
	}

	for name, cfg := range cases {
		t.Run(name, func(t *testing.T) {
			c, err := NewClient(cfg)
			if !errors.Is(err, ErrInvalidConfig) {
				t.Fatalf("expected ErrInvalidConfig, got %v", err)
			}
			if c != nil {
				t.Fatal("a rejected config must not yield a client")
			}
		})
	}
}

func TestNewClientDefaults(t *testing.T) {
	c, err := NewClient(Config{RPCURL: "http://localhost:8000", NetworkPassphrase: NetworkTestnet})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	if c.sourceAccount != DefaultSimulationAccount {
		t.Fatalf("source = %q, want the default simulation account", c.sourceAccount)
	}
	if c.http.Timeout != DefaultTimeout {
		t.Fatalf("timeout = %v, want %v", c.http.Timeout, DefaultTimeout)
	}
	if c.HasSigner() {
		t.Fatal("a client built without a signer must report HasSigner() == false")
	}
}

func TestDefaultSimulationAccountIsAValidStrkey(t *testing.T) {
	if _, _, err := DecodeStrkey(DefaultSimulationAccount); err != nil {
		t.Fatalf("DefaultSimulationAccount does not decode: %v", err)
	}
}

// ── Read methods ──────────────────────────────────────────────────────────────

func TestGetInfo(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, poolInfoScVal()))
	c := newTestClient(t, rec, nil)

	info, err := c.GetInfo(context.Background(), zeroContract)
	if err != nil {
		t.Fatalf("GetInfo: %v", err)
	}
	if info.TokenA != zeroContract || info.TokenB != zeroContract {
		t.Fatalf("tokens = %q/%q", info.TokenA, info.TokenB)
	}
	if info.ReserveA.Int64() != 1_000_000 || info.ReserveB.Int64() != 2_000_000 {
		t.Fatalf("reserves = %s/%s", info.ReserveA, info.ReserveB)
	}
	if info.FeeBps.Int64() != 30 || info.LpRebateBps.Int64() != 1000 {
		t.Fatalf("fee_bps = %s lp_rebate_bps = %s", info.FeeBps, info.LpRebateBps)
	}
	if info.Admin != zeroAccount {
		t.Fatalf("admin = %q", info.Admin)
	}
}

func TestReadMethodsWorkWithoutASigner(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, poolInfoScVal()))
	c := newTestClient(t, rec, nil)

	if _, err := c.GetInfo(context.Background(), zeroContract); err != nil {
		t.Fatalf("GetInfo without a signer: %v", err)
	}
	for _, method := range rec.calls {
		if method == "sendTransaction" {
			t.Fatal("a read method must never submit a transaction")
		}
	}
}

func TestGetReserves(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, poolInfoScVal()))
	c := newTestClient(t, rec, nil)

	res, err := c.GetReserves(context.Background(), zeroContract)
	if err != nil {
		t.Fatalf("GetReserves: %v", err)
	}
	if res.ReserveA.Int64() != 1_000_000 || res.ReserveB.Int64() != 2_000_000 {
		t.Fatalf("reserves = %s/%s", res.ReserveA, res.ReserveB)
	}
}

func TestSimulateSwap(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, swapQuoteScVal()))
	c := newTestClient(t, rec, nil)

	quote, err := c.SimulateSwap(context.Background(), zeroContract, zeroContract, big.NewInt(2_000))
	if err != nil {
		t.Fatalf("SimulateSwap: %v", err)
	}
	if quote.AmountOut.Int64() != 1_980 {
		t.Fatalf("amount_out = %s, want 1980", quote.AmountOut)
	}
	if quote.FeeAmount.Int64() != 3 || quote.PriceImpactBps.Int64() != 12 {
		t.Fatalf("fee = %s impact = %s", quote.FeeAmount, quote.PriceImpactBps)
	}
}

func TestGetAmountOutAndIn(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, I128(big.NewInt(4_242))))
	c := newTestClient(t, rec, nil)

	out, err := c.GetAmountOut(context.Background(), zeroContract, zeroContract, big.NewInt(10))
	if err != nil {
		t.Fatalf("GetAmountOut: %v", err)
	}
	if out.Int64() != 4_242 {
		t.Fatalf("amount out = %s, want 4242", out)
	}

	in, err := c.GetAmountIn(context.Background(), zeroContract, zeroContract, big.NewInt(10))
	if err != nil {
		t.Fatalf("GetAmountIn: %v", err)
	}
	if in.Int64() != 4_242 {
		t.Fatalf("amount in = %s, want 4242", in)
	}
}

func TestSharesOf(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, I128(big.NewInt(500))))
	c := newTestClient(t, rec, nil)

	shares, err := c.SharesOf(context.Background(), zeroContract, zeroAccount)
	if err != nil {
		t.Fatalf("SharesOf: %v", err)
	}
	if shares.Int64() != 500 {
		t.Fatalf("shares = %s, want 500", shares)
	}

	if _, err := c.SharesOf(context.Background(), zeroContract, "bogus"); !errors.Is(err, ErrInvalidConfig) {
		t.Fatalf("expected ErrInvalidConfig for a malformed provider, got %v", err)
	}
}

func TestReadMethodsRejectNonPositiveAmounts(t *testing.T) {
	rec := newRecordedRPC(t)
	c := newTestClient(t, rec, nil)
	ctx := context.Background()

	for name, call := range map[string]func() error{
		"SimulateSwap zero": func() error {
			_, err := c.SimulateSwap(ctx, zeroContract, zeroContract, big.NewInt(0))
			return err
		},
		"GetAmountOut negative": func() error {
			_, err := c.GetAmountOut(ctx, zeroContract, zeroContract, big.NewInt(-1))
			return err
		},
		"GetAmountIn nil": func() error {
			_, err := c.GetAmountIn(ctx, zeroContract, zeroContract, nil)
			return err
		},
	} {
		if err := call(); !errors.Is(err, ErrZeroAmount) {
			t.Fatalf("%s: expected ErrZeroAmount, got %v", name, err)
		}
	}
	if len(rec.calls) != 0 {
		t.Fatalf("argument validation must fail before any RPC call, saw %v", rec.calls)
	}
}

func TestReadMethodRejectsBadContractID(t *testing.T) {
	rec := newRecordedRPC(t)
	c := newTestClient(t, rec, nil)

	if _, err := c.GetInfo(context.Background(), zeroAccount); !errors.Is(err, ErrInvalidConfig) {
		t.Fatalf("expected ErrInvalidConfig for an account used as a pool id, got %v", err)
	}
}

// ── Error decoding ────────────────────────────────────────────────────────────

func TestSimulationContractErrorIsTyped(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", map[string]interface{}{
		"error": "HostError: Error(Contract, #5)",
	})
	c := newTestClient(t, rec, nil)

	_, err := c.GetAmountOut(context.Background(), zeroContract, zeroContract, big.NewInt(1))
	if !errors.Is(err, ErrSlippageExceeded) {
		t.Fatalf("expected ErrSlippageExceeded, got %v", err)
	}

	var ce *ContractError
	if !errors.As(err, &ce) {
		t.Fatalf("expected a *ContractError, got %T", err)
	}
	if ce.Code != 5 {
		t.Fatalf("code = %d, want 5", ce.Code)
	}
}

func TestDecodeContractErrorCoversEveryDiscriminant(t *testing.T) {
	want := map[int]error{
		1: ErrAlreadyInitialized, 2: ErrInvalidFeeBps, 3: ErrInsufficientShares,
		4: ErrDeadlineExceeded, 5: ErrSlippageExceeded, 6: ErrPaused,
		7: ErrUnauthorized, 8: ErrZeroAmount, 9: ErrInvalidToken,
		10: ErrEmptyPool, 11: ErrInsufficientLiquidity, 12: ErrNoPendingAdmin,
		13: ErrWrongAdmin, 14: ErrReentrant, 15: ErrCircuitBreaker,
		16: ErrFotSlippage, 17: ErrOracleDeviationExceeded, 18: ErrFlashLoanRepaymentFailed,
		19: ErrAlreadyExecuted, 20: ErrProposalExpired, 21: ErrNotInitialized,
	}
	for code, sentinel := range want {
		raw := "HostError: Error(Contract, #" + itoa(code) + ")"
		got := DecodeContractError(raw)
		if !errors.Is(got, sentinel) {
			t.Fatalf("code %d decoded to %v, want %v", code, got, sentinel)
		}
		if got.Code != code {
			t.Fatalf("code = %d, want %d", got.Code, code)
		}
		if !strings.Contains(got.Error(), sentinel.Error()) {
			t.Fatalf("message %q should mention %q", got.Error(), sentinel.Error())
		}
	}
}

func TestDecodeContractErrorHandlesUnknownShapes(t *testing.T) {
	unknown := DecodeContractError("HostError: Error(Contract, #999)")
	if unknown.Code != 999 {
		t.Fatalf("code = %d, want 999", unknown.Code)
	}
	if errors.Unwrap(unknown) != nil {
		t.Fatal("an unmapped discriminant must not claim a sentinel")
	}

	unparsed := DecodeContractError("some transport failure")
	if unparsed.Code != 0 {
		t.Fatalf("code = %d, want 0", unparsed.Code)
	}
	if !strings.Contains(unparsed.Error(), "some transport failure") {
		t.Fatalf("raw text should survive, got %q", unparsed.Error())
	}
}

func TestRPCErrorIsSurfaced(t *testing.T) {
	rec := newRecordedRPC(t).on("simulateTransaction", func(json.RawMessage) (interface{}, *rpcError) {
		return nil, &rpcError{Code: -32601, Message: "method not found"}
	})
	c := newTestClient(t, rec, nil)

	_, err := c.GetInfo(context.Background(), zeroContract)
	if !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC, got %v", err)
	}
}

func TestHTTPErrorIsSurfaced(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		http.Error(w, "upstream exploded", http.StatusBadGateway)
	}))
	t.Cleanup(srv.Close)

	c, err := NewClient(Config{RPCURL: srv.URL, NetworkPassphrase: NetworkTestnet})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	if _, err := c.GetInfo(context.Background(), zeroContract); !errors.Is(err, ErrRPC) {
		t.Fatalf("expected ErrRPC, got %v", err)
	}
}

// ── Write methods ─────────────────────────────────────────────────────────────

// testSigner records what it was asked to sign and returns a marked envelope.
type testSigner struct {
	addr       string
	seen       string
	passphrase string
	err        error
}

func (s *testSigner) Address() string { return s.addr }

func (s *testSigner) SignEnvelope(_ context.Context, envelopeXDR, passphrase string) (string, error) {
	if s.err != nil {
		return "", s.err
	}
	s.seen = envelopeXDR
	s.passphrase = passphrase
	return envelopeXDR, nil
}

func successfulWriteRPC(t *testing.T) *recordedRPC {
	return newRecordedRPC(t).
		respond("simulateTransaction", simulationOf(t, I128(big.NewInt(1_980)))).
		respond("sendTransaction", map[string]interface{}{"status": "PENDING", "hash": "abc123"}).
		respond("getTransaction", map[string]interface{}{"status": "SUCCESS", "returnValue": "AAAA"})
}

func validSwap() SwapParams {
	return SwapParams{
		PoolID:   zeroContract,
		Trader:   zeroAccount,
		TokenIn:  zeroContract,
		AmountIn: big.NewInt(1_000),
		MinOut:   big.NewInt(900),
		Deadline: 1_900_000_000,
	}
}

func TestSwapSubmitsAndPolls(t *testing.T) {
	rec := successfulWriteRPC(t)
	signer := &testSigner{addr: zeroAccount}
	c := newTestClient(t, rec, signer)

	res, err := c.Swap(context.Background(), validSwap())
	if err != nil {
		t.Fatalf("Swap: %v", err)
	}
	if res.Hash != "abc123" || res.Status != "SUCCESS" {
		t.Fatalf("result = %+v", res)
	}
	if signer.seen == "" {
		t.Fatal("the signer was never asked to sign")
	}
	if signer.passphrase != NetworkTestnet {
		t.Fatalf("signed for %q, want %q", signer.passphrase, NetworkTestnet)
	}

	wantOrder := []string{"getLedgerEntries", "simulateTransaction", "sendTransaction", "getTransaction"}
	if len(rec.calls) != len(wantOrder) {
		t.Fatalf("calls = %v, want %v", rec.calls, wantOrder)
	}
	for i, method := range wantOrder {
		if rec.calls[i] != method {
			t.Fatalf("call %d = %q, want %q", i, rec.calls[i], method)
		}
	}
}

func TestWriteMethodsRequireASigner(t *testing.T) {
	rec := newRecordedRPC(t)
	c := newTestClient(t, rec, nil)
	ctx := context.Background()

	if _, err := c.Swap(ctx, validSwap()); !errors.Is(err, ErrNoSigner) {
		t.Fatalf("Swap: expected ErrNoSigner, got %v", err)
	}
	add := AddLiquidityParams{
		PoolID: zeroContract, Provider: zeroAccount,
		AmountA: big.NewInt(1), AmountB: big.NewInt(1),
		MinShares: big.NewInt(0), Deadline: 1_900_000_000,
	}
	if _, err := c.AddLiquidity(ctx, add); !errors.Is(err, ErrNoSigner) {
		t.Fatalf("AddLiquidity: expected ErrNoSigner, got %v", err)
	}
	remove := RemoveLiquidityParams{
		PoolID: zeroContract, Provider: zeroAccount, Shares: big.NewInt(1),
		MinA: big.NewInt(0), MinB: big.NewInt(0), Deadline: 1_900_000_000,
	}
	if _, err := c.RemoveLiquidity(ctx, remove); !errors.Is(err, ErrNoSigner) {
		t.Fatalf("RemoveLiquidity: expected ErrNoSigner, got %v", err)
	}
}

func TestWriteMethodsRequireADeadline(t *testing.T) {
	rec := newRecordedRPC(t)
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount})

	p := validSwap()
	p.Deadline = 0
	if _, err := c.Swap(context.Background(), p); !errors.Is(err, ErrDeadlineRequired) {
		t.Fatalf("expected ErrDeadlineRequired, got %v", err)
	}
	if len(rec.calls) != 0 {
		t.Fatalf("a missing deadline must fail before any RPC call, saw %v", rec.calls)
	}
}

func TestWriteMethodsRequireASlippageBound(t *testing.T) {
	rec := newRecordedRPC(t)
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount})
	ctx := context.Background()

	swap := validSwap()
	swap.MinOut = nil
	if _, err := c.Swap(ctx, swap); !errors.Is(err, ErrSlippageRequired) {
		t.Fatalf("Swap: expected ErrSlippageRequired, got %v", err)
	}

	add := AddLiquidityParams{
		PoolID: zeroContract, Provider: zeroAccount,
		AmountA: big.NewInt(1), AmountB: big.NewInt(1),
		Deadline: 1_900_000_000,
	}
	if _, err := c.AddLiquidity(ctx, add); !errors.Is(err, ErrSlippageRequired) {
		t.Fatalf("AddLiquidity: expected ErrSlippageRequired, got %v", err)
	}

	remove := RemoveLiquidityParams{
		PoolID: zeroContract, Provider: zeroAccount, Shares: big.NewInt(1),
		MinA: big.NewInt(0), MinB: big.NewInt(-1), Deadline: 1_900_000_000,
	}
	if _, err := c.RemoveLiquidity(ctx, remove); !errors.Is(err, ErrSlippageRequired) {
		t.Fatalf("RemoveLiquidity: expected ErrSlippageRequired, got %v", err)
	}
}

func TestAddAndRemoveLiquiditySubmit(t *testing.T) {
	ctx := context.Background()

	add := AddLiquidityParams{
		PoolID: zeroContract, Provider: zeroAccount,
		AmountA: big.NewInt(1_000), AmountB: big.NewInt(2_000),
		MinShares: big.NewInt(1), Deadline: 1_900_000_000,
	}
	c := newTestClient(t, successfulWriteRPC(t), &testSigner{addr: zeroAccount})
	if _, err := c.AddLiquidity(ctx, add); err != nil {
		t.Fatalf("AddLiquidity: %v", err)
	}

	remove := RemoveLiquidityParams{
		PoolID: zeroContract, Provider: zeroAccount, Shares: big.NewInt(10),
		MinA: big.NewInt(1), MinB: big.NewInt(1), Deadline: 1_900_000_000,
	}
	c2 := newTestClient(t, successfulWriteRPC(t), &testSigner{addr: zeroAccount})
	if _, err := c2.RemoveLiquidity(ctx, remove); err != nil {
		t.Fatalf("RemoveLiquidity: %v", err)
	}
}

func TestWriteSurfacesSimulationContractError(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", map[string]interface{}{
		"error": "HostError: Error(Contract, #6)",
	})
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount})

	_, err := c.Swap(context.Background(), validSwap())
	if !errors.Is(err, ErrPaused) {
		t.Fatalf("expected ErrPaused, got %v", err)
	}
	for _, method := range rec.calls {
		if method == "sendTransaction" {
			t.Fatal("a failing simulation must not be submitted")
		}
	}
}

func TestWriteSurfacesSendRejection(t *testing.T) {
	rec := newRecordedRPC(t).
		respond("simulateTransaction", simulationOf(t, I128(big.NewInt(1)))).
		respond("sendTransaction", map[string]interface{}{"status": "ERROR", "errorResultXdr": "AAAA"})
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount})

	if _, err := c.Swap(context.Background(), validSwap()); !errors.Is(err, ErrTransactionFailed) {
		t.Fatalf("expected ErrTransactionFailed, got %v", err)
	}
}

func TestWriteSurfacesFailedTransaction(t *testing.T) {
	rec := newRecordedRPC(t).
		respond("simulateTransaction", simulationOf(t, I128(big.NewInt(1)))).
		respond("sendTransaction", map[string]interface{}{"status": "PENDING", "hash": "deadbeef"}).
		respond("getTransaction", map[string]interface{}{"status": "FAILED", "resultXdr": "AAAA"})
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount})

	if _, err := c.Swap(context.Background(), validSwap()); !errors.Is(err, ErrTransactionFailed) {
		t.Fatalf("expected ErrTransactionFailed, got %v", err)
	}
}

func TestWritePollsUntilConfirmed(t *testing.T) {
	polls := 0
	rec := newRecordedRPC(t).
		respond("simulateTransaction", simulationOf(t, I128(big.NewInt(1)))).
		respond("sendTransaction", map[string]interface{}{"status": "PENDING", "hash": "abc"}).
		on("getTransaction", func(json.RawMessage) (interface{}, *rpcError) {
			polls++
			if polls < 3 {
				return map[string]interface{}{"status": "NOT_FOUND"}, nil
			}
			return map[string]interface{}{"status": "SUCCESS", "returnValue": "AAAA"}, nil
		})
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount})

	if _, err := c.Swap(context.Background(), validSwap()); err != nil {
		t.Fatalf("Swap: %v", err)
	}
	if polls != 3 {
		t.Fatalf("polled %d times, want 3", polls)
	}
}

func TestWriteGivesUpAfterPollBudget(t *testing.T) {
	rec := newRecordedRPC(t).
		respond("simulateTransaction", simulationOf(t, I128(big.NewInt(1)))).
		respond("sendTransaction", map[string]interface{}{"status": "PENDING", "hash": "abc"}).
		respond("getTransaction", map[string]interface{}{"status": "NOT_FOUND"})
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount})

	if _, err := c.Swap(context.Background(), validSwap()); !errors.Is(err, ErrTransactionFailed) {
		t.Fatalf("expected ErrTransactionFailed, got %v", err)
	}
}

func TestWriteSurfacesSignerFailure(t *testing.T) {
	rec := newRecordedRPC(t).respond("simulateTransaction", simulationOf(t, I128(big.NewInt(1))))
	boom := errors.New("hsm unavailable")
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount, err: boom})

	if _, err := c.Swap(context.Background(), validSwap()); !errors.Is(err, boom) {
		t.Fatalf("expected the signer's error to propagate, got %v", err)
	}
}

func TestContextCancellationStopsPolling(t *testing.T) {
	rec := newRecordedRPC(t).
		respond("simulateTransaction", simulationOf(t, I128(big.NewInt(1)))).
		respond("sendTransaction", map[string]interface{}{"status": "PENDING", "hash": "abc"}).
		respond("getTransaction", map[string]interface{}{"status": "NOT_FOUND"})
	c := newTestClient(t, rec, &testSigner{addr: zeroAccount}).WithPolling(50*time.Millisecond, 100)

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()

	if _, err := c.Swap(ctx, validSwap()); err == nil {
		t.Fatal("expected an error once the context expires")
	}
}

// ── Signer helpers ────────────────────────────────────────────────────────────

func TestValidateSigner(t *testing.T) {
	if err := ValidateSigner(nil); !errors.Is(err, ErrNoSigner) {
		t.Fatalf("expected ErrNoSigner, got %v", err)
	}
	if err := ValidateSigner(SignerFunc{Addr: "nope"}); !errors.Is(err, ErrSignerAddress) {
		t.Fatalf("expected ErrSignerAddress, got %v", err)
	}
	if err := ValidateSigner(SignerFunc{Addr: zeroAccount}); err != nil {
		t.Fatalf("a well-formed signer should validate, got %v", err)
	}
}

func TestSignerFuncWithoutAFunctionFails(t *testing.T) {
	s := SignerFunc{Addr: zeroAccount}
	if _, err := s.SignEnvelope(context.Background(), "envelope", NetworkTestnet); !errors.Is(err, ErrNoSigner) {
		t.Fatalf("expected ErrNoSigner, got %v", err)
	}
}

// itoa avoids importing strconv for one call in a table.
func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var digits []byte
	for n > 0 {
		digits = append([]byte{byte('0' + n%10)}, digits...)
		n /= 10
	}
	return string(digits)
}
