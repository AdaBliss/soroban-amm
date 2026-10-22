package gosdk

import (
	"encoding/base64"
	"errors"
	"fmt"
	"math/big"
)

// ErrScValue is returned when a value cannot be encoded to, or decoded from,
// the Soroban ScVal representation.
var ErrScValue = errors.New("scval")

// ScVal discriminants follow SCValType in Stellar-contract.x. Keep the complete
// sequence here even when a value is unsupported so later variants cannot drift.
const (
	scvBool int32 = iota
	scvVoid
	scvError
	scvU32
	scvI32
	scvU64
	scvI64
	scvTimepoint
	scvDuration
	scvU128
	scvI128
	scvU256
	scvI256
	scvBytes
	scvString
	scvSymbol
	scvVec
	scvMap
	scvAddress
	scvContractInstance
	scvLedgerKeyContractInstance
	scvLedgerKeyNonce

	scoAccount       int32 = 0
	scoContract      int32 = 1
	scAddressAccount int32 = 0
	scAddressCntrct  int32 = 1
)

// ScValue is a typed Soroban contract value. Exactly one field is meaningful,
// selected by Type.
type ScValue struct {
	// Type is the ScVal discriminant.
	Type int32
	// Bool holds the value when Type is a boolean.
	Bool bool
	// U32 holds the value when Type is u32.
	U32 uint32
	// I32 holds the value when Type is i32.
	I32 int32
	// U64 holds the value when Type is u64.
	U64 uint64
	// I64 holds the value when Type is i64.
	I64 int64
	// Int holds the value when Type is i128 or u128.
	Int *big.Int
	// Bytes holds the value when Type is bytes.
	Bytes []byte
	// Str holds the value when Type is string or symbol.
	Str string
	// Addr holds the strkey when Type is address.
	Addr string
	// Vec holds the elements when Type is vec.
	Vec []ScValue
	// Map holds the entries when Type is map, in key order.
	Map []ScMapEntry
}

// ScMapEntry is one key/value pair of an ScVal map.
type ScMapEntry struct {
	// Key is the entry's key.
	Key ScValue
	// Val is the entry's value.
	Val ScValue
}

// Bool returns a boolean ScValue.
func Bool(v bool) ScValue { return ScValue{Type: scvBool, Bool: v} }

// Void returns the unit ScValue.
func Void() ScValue { return ScValue{Type: scvVoid} }

// U32 returns a u32 ScValue.
func U32(v uint32) ScValue { return ScValue{Type: scvU32, U32: v} }

// I32 returns an i32 ScValue.
func I32(v int32) ScValue { return ScValue{Type: scvI32, I32: v} }

// U64 returns a u64 ScValue.
func U64(v uint64) ScValue { return ScValue{Type: scvU64, U64: v} }

// I64 returns an i64 ScValue.
func I64(v int64) ScValue { return ScValue{Type: scvI64, I64: v} }

// I128 returns an i128 ScValue. The value is range-checked at encode time.
func I128(v *big.Int) ScValue { return ScValue{Type: scvI128, Int: v} }

// U128 returns a u128 ScValue. The value is range-checked at encode time.
func U128(v *big.Int) ScValue { return ScValue{Type: scvU128, Int: v} }

// Bytes returns a bytes ScValue.
func Bytes(v []byte) ScValue { return ScValue{Type: scvBytes, Bytes: v} }

// Str returns a string ScValue.
func Str(v string) ScValue { return ScValue{Type: scvString, Str: v} }

// Symbol returns a symbol ScValue. Symbols name contract functions and struct
// fields.
func Symbol(v string) ScValue { return ScValue{Type: scvSymbol, Str: v} }

// Vec returns a vec ScValue.
func Vec(items ...ScValue) ScValue { return ScValue{Type: scvVec, Vec: items} }

// Map returns a map ScValue.
func Map(entries ...ScMapEntry) ScValue { return ScValue{Type: scvMap, Map: entries} }

// Addr returns an address ScValue from a G... account or C... contract strkey.
func Addr(strkey string) ScValue { return ScValue{Type: scvAddress, Addr: strkey} }

// EncodeScVal serialises an ScValue to XDR.
func EncodeScVal(v ScValue) ([]byte, error) {
	var w xdrWriter
	if err := writeScVal(&w, v); err != nil {
		return nil, err
	}
	return w.bytes(), nil
}

// EncodeScValBase64 serialises an ScValue to base64-encoded XDR, the form the
// JSON-RPC layer carries.
func EncodeScValBase64(v ScValue) (string, error) {
	b, err := EncodeScVal(v)
	if err != nil {
		return "", err
	}
	return base64.StdEncoding.EncodeToString(b), nil
}

// DecodeScVal parses an ScValue from XDR.
func DecodeScVal(b []byte) (ScValue, error) {
	r := &xdrReader{buf: b}
	v, err := readScVal(r)
	if err != nil {
		return ScValue{}, err
	}
	return v, nil
}

// DecodeScValBase64 parses an ScValue from base64-encoded XDR.
func DecodeScValBase64(s string) (ScValue, error) {
	b, err := base64.StdEncoding.DecodeString(s)
	if err != nil {
		return ScValue{}, fmt.Errorf("%w: invalid base64: %v", ErrScValue, err)
	}
	return DecodeScVal(b)
}

func writeScVal(w *xdrWriter, v ScValue) error {
	w.writeInt32(v.Type)

	switch v.Type {
	case scvBool:
		w.writeBool(v.Bool)
	case scvVoid:
	case scvU32:
		w.writeUint32(v.U32)
	case scvI32:
		w.writeInt32(v.I32)
	case scvU64:
		w.writeUint64(v.U64)
	case scvI64:
		w.writeInt64(v.I64)
	case scvI128:
		p, err := EncodeI128(v.Int)
		if err != nil {
			return err
		}
		// i128 is encoded high limb first, as { hi: int64, lo: uint64 }.
		w.writeUint64(p.Hi)
		w.writeUint64(p.Lo)
	case scvU128:
		p, err := EncodeU128(v.Int)
		if err != nil {
			return err
		}
		w.writeUint64(p.Hi)
		w.writeUint64(p.Lo)
	case scvBytes:
		w.writeBytes(v.Bytes)
	case scvString:
		w.writeBytes([]byte(v.Str))
	case scvSymbol:
		if len(v.Str) > 32 {
			return fmt.Errorf("%w: symbol %q exceeds 32 characters", ErrScValue, v.Str)
		}
		w.writeBytes([]byte(v.Str))
	case scvVec:
		// A vec is an optional array; a present vec writes 1 then the array.
		w.writeUint32(1)
		w.writeUint32(uint32(len(v.Vec)))
		for _, item := range v.Vec {
			if err := writeScVal(w, item); err != nil {
				return err
			}
		}
	case scvMap:
		w.writeUint32(1)
		w.writeUint32(uint32(len(v.Map)))
		for _, e := range v.Map {
			if err := writeScVal(w, e.Key); err != nil {
				return err
			}
			if err := writeScVal(w, e.Val); err != nil {
				return err
			}
		}
	case scvAddress:
		return writeScAddress(w, v.Addr)
	default:
		return fmt.Errorf("%w: unsupported type %d for encoding", ErrScValue, v.Type)
	}
	return nil
}

func writeScAddress(w *xdrWriter, strkey string) error {
	raw, kind, err := DecodeStrkey(strkey)
	if err != nil {
		return err
	}
	switch kind {
	case StrkeyAccount:
		w.writeInt32(scAddressAccount)
		// PublicKey is itself a union discriminated on key type.
		w.writeInt32(scoAccount)
		w.writeRaw(raw)
	case StrkeyContract:
		w.writeInt32(scAddressCntrct)
		w.writeRaw(raw)
	default:
		return fmt.Errorf("%w: address %q is neither an account nor a contract", ErrScValue, strkey)
	}
	return nil
}

func readScVal(r *xdrReader) (ScValue, error) {
	t, err := r.readInt32()
	if err != nil {
		return ScValue{}, err
	}
	v := ScValue{Type: t}

	switch t {
	case scvBool:
		b, err := r.readUint32()
		if err != nil {
			return ScValue{}, err
		}
		v.Bool = b != 0
	case scvVoid:
	case scvU32:
		if v.U32, err = r.readUint32(); err != nil {
			return ScValue{}, err
		}
	case scvI32:
		if v.I32, err = r.readInt32(); err != nil {
			return ScValue{}, err
		}
	case scvU64:
		if v.U64, err = r.readUint64(); err != nil {
			return ScValue{}, err
		}
	case scvI64:
		u, err := r.readUint64()
		if err != nil {
			return ScValue{}, err
		}
		v.I64 = int64(u)
	case scvI128, scvU128:
		hi, err := r.readUint64()
		if err != nil {
			return ScValue{}, err
		}
		lo, err := r.readUint64()
		if err != nil {
			return ScValue{}, err
		}
		if t == scvI128 {
			v.Int = DecodeI128(Parts128{Hi: hi, Lo: lo})
		} else {
			v.Int = DecodeU128(Parts128{Hi: hi, Lo: lo})
		}
	case scvBytes:
		if v.Bytes, err = r.readBytes(); err != nil {
			return ScValue{}, err
		}
	case scvString, scvSymbol:
		b, err := r.readBytes()
		if err != nil {
			return ScValue{}, err
		}
		v.Str = string(b)
	case scvVec:
		present, err := r.readUint32()
		if err != nil {
			return ScValue{}, err
		}
		if present == 0 {
			return v, nil
		}
		n, err := r.readLength()
		if err != nil {
			return ScValue{}, err
		}
		v.Vec = make([]ScValue, 0, n)
		for i := uint32(0); i < n; i++ {
			item, err := readScVal(r)
			if err != nil {
				return ScValue{}, err
			}
			v.Vec = append(v.Vec, item)
		}
	case scvMap:
		present, err := r.readUint32()
		if err != nil {
			return ScValue{}, err
		}
		if present == 0 {
			return v, nil
		}
		n, err := r.readLength()
		if err != nil {
			return ScValue{}, err
		}
		v.Map = make([]ScMapEntry, 0, n)
		for i := uint32(0); i < n; i++ {
			k, err := readScVal(r)
			if err != nil {
				return ScValue{}, err
			}
			val, err := readScVal(r)
			if err != nil {
				return ScValue{}, err
			}
			v.Map = append(v.Map, ScMapEntry{Key: k, Val: val})
		}
	case scvAddress:
		if v.Addr, err = readScAddress(r); err != nil {
			return ScValue{}, err
		}
	case scvError:
		// Error values are surfaced through DecodeContractError, not here.
		return ScValue{}, fmt.Errorf("%w: value is a contract error", ErrScValue)
	case scvTimepoint, scvDuration, scvU256, scvI256,
		scvContractInstance, scvLedgerKeyContractInstance, scvLedgerKeyNonce:
		return ScValue{}, fmt.Errorf("%w: unsupported type %d for decoding", ErrScValue, t)
	default:
		return ScValue{}, fmt.Errorf("%w: unsupported type %d for decoding", ErrScValue, t)
	}
	return v, nil
}

func readScAddress(r *xdrReader) (string, error) {
	kind, err := r.readInt32()
	if err != nil {
		return "", err
	}
	switch kind {
	case scAddressAccount:
		if _, err := r.readInt32(); err != nil { // PublicKey type
			return "", err
		}
		raw, err := r.readRaw(32)
		if err != nil {
			return "", err
		}
		return EncodeStrkey(raw, StrkeyAccount)
	case scAddressCntrct:
		raw, err := r.readRaw(32)
		if err != nil {
			return "", err
		}
		return EncodeStrkey(raw, StrkeyContract)
	default:
		return "", fmt.Errorf("%w: unknown address kind %d", ErrScValue, kind)
	}
}

// MapField returns the value stored under a symbol key in a map ScValue.
// Contract structs are transmitted as maps keyed by their field names.
func (v ScValue) MapField(name string) (ScValue, bool) {
	if v.Type != scvMap {
		return ScValue{}, false
	}
	for _, e := range v.Map {
		if e.Key.Type == scvSymbol && e.Key.Str == name {
			return e.Val, true
		}
	}
	return ScValue{}, false
}

// BigInt returns the value as a *big.Int for any integer ScValue type.
func (v ScValue) BigInt() (*big.Int, error) {
	switch v.Type {
	case scvI128, scvU128:
		if v.Int == nil {
			return nil, fmt.Errorf("%w: 128-bit value is nil", ErrScValue)
		}
		return new(big.Int).Set(v.Int), nil
	case scvU32:
		return new(big.Int).SetUint64(uint64(v.U32)), nil
	case scvI32:
		return big.NewInt(int64(v.I32)), nil
	case scvU64:
		return new(big.Int).SetUint64(v.U64), nil
	case scvI64:
		return big.NewInt(v.I64), nil
	default:
		return nil, fmt.Errorf("%w: type %d is not an integer", ErrScValue, v.Type)
	}
}

// Address returns the strkey of an address ScValue.
func (v ScValue) Address() (string, error) {
	if v.Type != scvAddress {
		return "", fmt.Errorf("%w: type %d is not an address", ErrScValue, v.Type)
	}
	return v.Addr, nil
}

// mapFieldBigInt reads a named struct field and converts it to a *big.Int.
func mapFieldBigInt(v ScValue, name string) (*big.Int, error) {
	f, ok := v.MapField(name)
	if !ok {
		return nil, fmt.Errorf("%w: missing field %q", ErrScValue, name)
	}
	return f.BigInt()
}

// mapFieldAddress reads a named struct field and converts it to a strkey.
func mapFieldAddress(v ScValue, name string) (string, error) {
	f, ok := v.MapField(name)
	if !ok {
		return "", fmt.Errorf("%w: missing field %q", ErrScValue, name)
	}
	return f.Address()
}
