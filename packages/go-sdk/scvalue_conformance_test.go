package gosdk

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"math/big"
	"os"
	"testing"
)

type goldenFile struct {
	Vectors []goldenVector `json:"vectors"`
}

type goldenVector struct {
	Name        string         `json:"name"`
	XDR         string         `json:"xdr"`
	Logical     logicalScValue `json:"logical"`
	Unsupported bool           `json:"unsupported"`
}

type logicalScValue struct {
	Type  string          `json:"type"`
	Value json.RawMessage `json:"value"`
}

type logicalMapEntry struct {
	Key   logicalScValue `json:"key"`
	Value logicalScValue `json:"value"`
}

func TestScValReferenceVectors(t *testing.T) {
	raw, err := os.ReadFile("testdata/scval_vectors.json")
	if err != nil {
		t.Fatal(err)
	}
	var fixtures goldenFile
	if err := json.Unmarshal(raw, &fixtures); err != nil {
		t.Fatal(err)
	}

	for _, fixture := range fixtures.Vectors {
		t.Run(fixture.Name, func(t *testing.T) {
			decoded, err := DecodeScValBase64(fixture.XDR)
			if fixture.Unsupported {
				if !errors.Is(err, ErrScValue) {
					t.Fatalf("expected ErrScValue, got %v", err)
				}
				return
			}
			if err != nil {
				t.Fatalf("decode reference XDR: %v", err)
			}

			want := fixture.Logical.scValue(t)
			assertScValEqual(t, decoded, want)
			encoded, err := EncodeScValBase64(want)
			if err != nil {
				t.Fatalf("encode logical value: %v", err)
			}
			if encoded != fixture.XDR {
				t.Fatalf("XDR = %q, want %q", encoded, fixture.XDR)
			}
		})
	}
}

func (logical logicalScValue) scValue(t *testing.T) ScValue {
	t.Helper()
	switch logical.Type {
	case "void":
		return Void()
	case "bool":
		var v bool
		logical.decode(t, &v)
		return Bool(v)
	case "u32":
		var v uint32
		logical.decode(t, &v)
		return U32(v)
	case "i32":
		var v int32
		logical.decode(t, &v)
		return I32(v)
	case "u64", "i64", "u128", "i128":
		var text string
		logical.decode(t, &text)
		v, ok := new(big.Int).SetString(text, 10)
		if !ok {
			t.Fatalf("invalid integer %q", text)
		}
		switch logical.Type {
		case "u64":
			return U64(v.Uint64())
		case "i64":
			return I64(v.Int64())
		case "u128":
			return U128(v)
		default:
			return I128(v)
		}
	case "bytes":
		var text string
		logical.decode(t, &text)
		v, err := base64.StdEncoding.DecodeString(text)
		if err != nil {
			t.Fatal(err)
		}
		return Bytes(v)
	case "string", "symbol", "address":
		var text string
		logical.decode(t, &text)
		switch logical.Type {
		case "string":
			return Str(text)
		case "symbol":
			return Symbol(text)
		default:
			return Addr(text)
		}
	case "vec":
		var values []logicalScValue
		logical.decode(t, &values)
		items := make([]ScValue, len(values))
		for i := range values {
			items[i] = values[i].scValue(t)
		}
		return Vec(items...)
	case "map":
		var values []logicalMapEntry
		logical.decode(t, &values)
		entries := make([]ScMapEntry, len(values))
		for i := range values {
			entries[i] = ScMapEntry{Key: values[i].Key.scValue(t), Val: values[i].Value.scValue(t)}
		}
		return Map(entries...)
	default:
		t.Fatalf("unsupported logical type %q", logical.Type)
		return ScValue{}
	}
}

func (logical logicalScValue) decode(t *testing.T, dst any) {
	t.Helper()
	if err := json.Unmarshal(logical.Value, dst); err != nil {
		t.Fatalf("decode %s fixture: %v", logical.Type, err)
	}
}
