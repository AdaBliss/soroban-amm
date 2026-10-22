package gosdk

import (
	"errors"
	"math/big"
	"testing"
)

// Independently-known strkeys. The account is the all-zero ed25519 account id,
// whose textual form is fixed by the strkey spec, so decoding it exercises the
// checksum against a literal this package did not generate.
const (
	zeroAccount  = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
	zeroContract = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
)

func TestStrkeyDecodesKnownAccount(t *testing.T) {
	raw, kind, err := DecodeStrkey(zeroAccount)
	if err != nil {
		t.Fatalf("DecodeStrkey: %v", err)
	}
	if kind != StrkeyAccount {
		t.Fatalf("kind = %v, want StrkeyAccount", kind)
	}
	if len(raw) != 32 {
		t.Fatalf("payload is %d bytes, want 32", len(raw))
	}
	for i, b := range raw {
		if b != 0 {
			t.Fatalf("byte %d = %d, want 0", i, b)
		}
	}
}

func TestStrkeyRoundTrip(t *testing.T) {
	for _, tc := range []struct {
		addr string
		kind StrkeyKind
	}{
		{zeroAccount, StrkeyAccount},
		{zeroContract, StrkeyContract},
	} {
		raw, kind, err := DecodeStrkey(tc.addr)
		if err != nil {
			t.Fatalf("DecodeStrkey(%s): %v", tc.addr, err)
		}
		if kind != tc.kind {
			t.Fatalf("kind = %v, want %v", kind, tc.kind)
		}
		back, err := EncodeStrkey(raw, kind)
		if err != nil {
			t.Fatalf("EncodeStrkey: %v", err)
		}
		if back != tc.addr {
			t.Fatalf("round-trip = %q, want %q", back, tc.addr)
		}
	}
}

func TestStrkeyRejectsBadInput(t *testing.T) {
	// Flip one character of a valid address; the checksum must catch it.
	corrupt := []byte(zeroAccount)
	corrupt[10] = 'B'
	cases := map[string]string{
		"empty":           "",
		"not base32":      "G!!!",
		"too short":       zeroAccount[:40],
		"bad checksum":    string(corrupt),
		"truncated by 1":  zeroAccount[:55],
		"unknown version": "MAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
	}
	for name, addr := range cases {
		if _, _, err := DecodeStrkey(addr); err == nil {
			t.Fatalf("%s: expected an error for %q", name, addr)
		}
	}
}

func TestEncodeStrkeyRejectsWrongLength(t *testing.T) {
	if _, err := EncodeStrkey(make([]byte, 31), StrkeyAccount); err == nil {
		t.Fatal("expected an error for a 31-byte payload")
	}
	if _, err := EncodeStrkey(make([]byte, 32), StrkeyUnknown); err == nil {
		t.Fatal("expected an error for an unknown strkey kind")
	}
}

func TestIsAddressHelpers(t *testing.T) {
	if !IsAccountAddress(zeroAccount) {
		t.Fatal("zeroAccount should be an account address")
	}
	if IsContractAddress(zeroAccount) {
		t.Fatal("zeroAccount is not a contract address")
	}
	if !IsContractAddress(zeroContract) {
		t.Fatal("zeroContract should be a contract address")
	}
	if IsAccountAddress("G-not-base32-and-too-short") {
		t.Fatal("malformed input should not pass IsAccountAddress")
	}
	if IsAccountAddress(zeroAccount[:55] + "1") {
		t.Fatal("'1' is not in the base32 alphabet")
	}
}

func TestScValRoundTrip(t *testing.T) {
	cases := []struct {
		name string
		val  ScValue
	}{
		{"bool true", Bool(true)},
		{"bool false", Bool(false)},
		{"void", Void()},
		{"u32", U32(4_294_967_295)},
		{"i32 negative", I32(-2_147_483_648)},
		{"u64", U64(18_446_744_073_709_551_615)},
		{"i64 negative", I64(-9_223_372_036_854_775_808)},
		{"i128 max", I128(new(big.Int).Set(MaxI128))},
		{"i128 min", I128(new(big.Int).Set(MinI128))},
		{"i128 negative", I128(big.NewInt(-42))},
		{"u128 max", U128(new(big.Int).Set(MaxU128))},
		{"bytes", Bytes([]byte{0x00, 0xff, 0x10})},
		{"bytes empty", Bytes(nil)},
		{"string", Str("hello world")},
		{"symbol", Symbol("get_info")},
		{"account address", Addr(zeroAccount)},
		{"contract address", Addr(zeroContract)},
		{"vec", Vec(U32(1), Str("two"), I128(big.NewInt(3)))},
		{"vec empty", Vec()},
		{"nested vec", Vec(Vec(U32(1), U32(2)), Vec())},
		{"map", Map(
			ScMapEntry{Key: Symbol("amount_out"), Val: I128(big.NewInt(999))},
			ScMapEntry{Key: Symbol("token_a"), Val: Addr(zeroContract)},
		)},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			encoded, err := EncodeScValBase64(tc.val)
			if err != nil {
				t.Fatalf("encode: %v", err)
			}
			got, err := DecodeScValBase64(encoded)
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			assertScValEqual(t, got, tc.val)
		})
	}
}

func TestEncodeScValRejectsBadValues(t *testing.T) {
	over := new(big.Int).Add(MaxI128, big.NewInt(1))
	if _, err := EncodeScVal(I128(over)); !errors.Is(err, ErrI128OutOfRange) {
		t.Fatalf("expected ErrI128OutOfRange, got %v", err)
	}
	if _, err := EncodeScVal(U128(big.NewInt(-1))); !errors.Is(err, ErrU128OutOfRange) {
		t.Fatalf("expected ErrU128OutOfRange, got %v", err)
	}
	if _, err := EncodeScVal(Symbol("this_symbol_is_far_too_long_to_fit_in_32_chars")); err == nil {
		t.Fatal("expected an error for an over-long symbol")
	}
	if _, err := EncodeScVal(Addr("not-an-address")); err == nil {
		t.Fatal("expected an error for a malformed address")
	}
	if _, err := EncodeScVal(ScValue{Type: 99}); !errors.Is(err, ErrScValue) {
		t.Fatalf("expected ErrScValue for an unsupported type, got %v", err)
	}
}

func TestDecodeScValRejectsTruncatedBuffer(t *testing.T) {
	encoded, err := EncodeScVal(I128(big.NewInt(1234)))
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	for n := 1; n < len(encoded); n++ {
		if _, err := DecodeScVal(encoded[:n]); err == nil {
			t.Fatalf("expected an error decoding %d of %d bytes", n, len(encoded))
		}
	}
	if _, err := DecodeScValBase64("!!!"); !errors.Is(err, ErrScValue) {
		t.Fatalf("expected ErrScValue for invalid base64, got %v", err)
	}
}

func TestDecodeScValRejectsKnownUnsupportedTypes(t *testing.T) {
	for _, typ := range []int32{
		scvError,
		scvTimepoint,
		scvDuration,
		scvU256,
		scvI256,
		scvContractInstance,
		scvLedgerKeyContractInstance,
		scvLedgerKeyNonce,
	} {
		var w xdrWriter
		w.writeInt32(typ)
		if _, err := DecodeScVal(w.bytes()); !errors.Is(err, ErrScValue) {
			t.Fatalf("type %d: expected ErrScValue, got %v", typ, err)
		}
	}
}

func TestScValAccessors(t *testing.T) {
	m := Map(
		ScMapEntry{Key: Symbol("amount"), Val: I128(big.NewInt(7))},
		ScMapEntry{Key: Symbol("who"), Val: Addr(zeroAccount)},
	)

	amount, err := mapFieldBigInt(m, "amount")
	if err != nil {
		t.Fatalf("mapFieldBigInt: %v", err)
	}
	if amount.Int64() != 7 {
		t.Fatalf("amount = %s, want 7", amount)
	}

	who, err := mapFieldAddress(m, "who")
	if err != nil {
		t.Fatalf("mapFieldAddress: %v", err)
	}
	if who != zeroAccount {
		t.Fatalf("who = %q, want %q", who, zeroAccount)
	}

	if _, err := mapFieldBigInt(m, "missing"); err == nil {
		t.Fatal("expected an error for a missing field")
	}
	if _, err := mapFieldAddress(m, "amount"); err == nil {
		t.Fatal("expected an error reading an integer field as an address")
	}
	if _, ok := U32(1).MapField("anything"); ok {
		t.Fatal("MapField on a non-map should report not-found")
	}
	if _, err := Str("x").BigInt(); err == nil {
		t.Fatal("expected an error converting a string to an integer")
	}

	// Every integer width converts through BigInt.
	for _, tc := range []struct {
		val  ScValue
		want int64
	}{
		{U32(5), 5},
		{I32(-5), -5},
		{U64(6), 6},
		{I64(-6), -6},
		{I128(big.NewInt(-7)), -7},
	} {
		got, err := tc.val.BigInt()
		if err != nil {
			t.Fatalf("BigInt: %v", err)
		}
		if got.Int64() != tc.want {
			t.Fatalf("BigInt = %s, want %d", got, tc.want)
		}
	}
}

// assertScValEqual compares two ScValues structurally, so a round-trip failure
// points at the field that differs.
func assertScValEqual(t *testing.T, got, want ScValue) {
	t.Helper()

	if got.Type != want.Type {
		t.Fatalf("type = %d, want %d", got.Type, want.Type)
	}
	switch want.Type {
	case scvBool:
		if got.Bool != want.Bool {
			t.Fatalf("bool = %v, want %v", got.Bool, want.Bool)
		}
	case scvU32:
		if got.U32 != want.U32 {
			t.Fatalf("u32 = %d, want %d", got.U32, want.U32)
		}
	case scvI32:
		if got.I32 != want.I32 {
			t.Fatalf("i32 = %d, want %d", got.I32, want.I32)
		}
	case scvU64:
		if got.U64 != want.U64 {
			t.Fatalf("u64 = %d, want %d", got.U64, want.U64)
		}
	case scvI64:
		if got.I64 != want.I64 {
			t.Fatalf("i64 = %d, want %d", got.I64, want.I64)
		}
	case scvI128, scvU128:
		if got.Int.Cmp(want.Int) != 0 {
			t.Fatalf("128-bit = %s, want %s", got.Int, want.Int)
		}
	case scvBytes:
		if len(got.Bytes) != len(want.Bytes) {
			t.Fatalf("bytes length = %d, want %d", len(got.Bytes), len(want.Bytes))
		}
		for i := range want.Bytes {
			if got.Bytes[i] != want.Bytes[i] {
				t.Fatalf("byte %d = %d, want %d", i, got.Bytes[i], want.Bytes[i])
			}
		}
	case scvString, scvSymbol:
		if got.Str != want.Str {
			t.Fatalf("string = %q, want %q", got.Str, want.Str)
		}
	case scvAddress:
		if got.Addr != want.Addr {
			t.Fatalf("address = %q, want %q", got.Addr, want.Addr)
		}
	case scvVec:
		if len(got.Vec) != len(want.Vec) {
			t.Fatalf("vec length = %d, want %d", len(got.Vec), len(want.Vec))
		}
		for i := range want.Vec {
			assertScValEqual(t, got.Vec[i], want.Vec[i])
		}
	case scvMap:
		if len(got.Map) != len(want.Map) {
			t.Fatalf("map length = %d, want %d", len(got.Map), len(want.Map))
		}
		for i := range want.Map {
			assertScValEqual(t, got.Map[i].Key, want.Map[i].Key)
			assertScValEqual(t, got.Map[i].Val, want.Map[i].Val)
		}
	}
}
