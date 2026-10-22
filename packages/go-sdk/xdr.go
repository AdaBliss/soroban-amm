package gosdk

import (
	"encoding/base32"
	"errors"
	"fmt"
	"strings"
)

// ErrXDR is returned when an XDR buffer is malformed or truncated.
var ErrXDR = errors.New("xdr")

// xdrWriter accumulates big-endian XDR, padding every opaque field to a
// four-byte boundary as the format requires.
type xdrWriter struct {
	buf []byte
}

func (w *xdrWriter) writeUint32(v uint32) {
	w.buf = append(w.buf, byte(v>>24), byte(v>>16), byte(v>>8), byte(v))
}

func (w *xdrWriter) writeInt32(v int32) { w.writeUint32(uint32(v)) }

func (w *xdrWriter) writeUint64(v uint64) {
	w.writeUint32(uint32(v >> 32))
	w.writeUint32(uint32(v))
}

func (w *xdrWriter) writeInt64(v int64) { w.writeUint64(uint64(v)) }

func (w *xdrWriter) writeBool(v bool) {
	if v {
		w.writeUint32(1)
		return
	}
	w.writeUint32(0)
}

// writeBytes writes a variable-length opaque field: length then contents,
// padded to four bytes.
func (w *xdrWriter) writeBytes(b []byte) {
	w.writeUint32(uint32(len(b)))
	w.writeRaw(b)
}

// writeRaw writes fixed-length contents with padding but no length prefix.
func (w *xdrWriter) writeRaw(b []byte) {
	w.buf = append(w.buf, b...)
	if pad := (4 - len(b)%4) % 4; pad > 0 {
		w.buf = append(w.buf, make([]byte, pad)...)
	}
}

func (w *xdrWriter) bytes() []byte { return w.buf }

// xdrReader consumes big-endian XDR.
type xdrReader struct {
	buf []byte
	pos int
}

func (r *xdrReader) readRaw(n int) ([]byte, error) {
	padded := n + (4-n%4)%4
	if r.pos+padded > len(r.buf) {
		return nil, fmt.Errorf("%w: truncated: need %d bytes at offset %d, have %d", ErrXDR, padded, r.pos, len(r.buf)-r.pos)
	}
	out := r.buf[r.pos : r.pos+n]
	r.pos += padded
	return out, nil
}

func (r *xdrReader) readUint32() (uint32, error) {
	if r.pos+4 > len(r.buf) {
		return 0, fmt.Errorf("%w: truncated uint32 at offset %d", ErrXDR, r.pos)
	}
	v := uint32(r.buf[r.pos])<<24 | uint32(r.buf[r.pos+1])<<16 | uint32(r.buf[r.pos+2])<<8 | uint32(r.buf[r.pos+3])
	r.pos += 4
	return v, nil
}

func (r *xdrReader) readInt32() (int32, error) {
	v, err := r.readUint32()
	return int32(v), err
}

func (r *xdrReader) readUint64() (uint64, error) {
	hi, err := r.readUint32()
	if err != nil {
		return 0, err
	}
	lo, err := r.readUint32()
	if err != nil {
		return 0, err
	}
	return uint64(hi)<<32 | uint64(lo), nil
}

func (r *xdrReader) readBytes() ([]byte, error) {
	n, err := r.readLength()
	if err != nil {
		return nil, err
	}
	return r.readRaw(int(n))
}

// readLength reads a length prefix and rejects one larger than the remaining
// buffer, so a corrupt value cannot drive a huge allocation.
func (r *xdrReader) readLength() (uint32, error) {
	n, err := r.readUint32()
	if err != nil {
		return 0, err
	}
	if int(n) > len(r.buf)-r.pos {
		return 0, fmt.Errorf("%w: length %d exceeds %d remaining bytes", ErrXDR, n, len(r.buf)-r.pos)
	}
	return n, nil
}

// StrkeyKind identifies which strkey version byte an address carries.
type StrkeyKind int

const (
	// StrkeyUnknown is an address whose version byte is not recognised.
	StrkeyUnknown StrkeyKind = iota
	// StrkeyAccount is a G... ed25519 public key.
	StrkeyAccount
	// StrkeyContract is a C... contract id.
	StrkeyContract
	// StrkeySeed is an S... ed25519 secret seed.
	StrkeySeed
)

const (
	versionByteAccount  byte = 6 << 3  // 0x30 -> 'G'
	versionByteContract byte = 2 << 3  // 0x10 -> 'C'
	versionByteSeed     byte = 18 << 3 // 0x90 -> 'S'
)

var base32NoPad = base32.StdEncoding.WithPadding(base32.NoPadding)

// DecodeStrkey decodes a Stellar strkey into its 32 raw bytes, verifying the
// trailing CRC16-XModem checksum.
func DecodeStrkey(s string) ([]byte, StrkeyKind, error) {
	if s == "" {
		return nil, StrkeyUnknown, fmt.Errorf("%w: empty address", ErrXDR)
	}
	raw, err := base32NoPad.DecodeString(strings.ToUpper(s))
	if err != nil {
		return nil, StrkeyUnknown, fmt.Errorf("%w: address %q is not base32: %v", ErrXDR, s, err)
	}
	if len(raw) != 35 {
		return nil, StrkeyUnknown, fmt.Errorf("%w: address %q decodes to %d bytes, want 35", ErrXDR, s, len(raw))
	}

	payload, want := raw[:33], raw[33:]
	if got := crc16XModem(payload); got[0] != want[0] || got[1] != want[1] {
		return nil, StrkeyUnknown, fmt.Errorf("%w: address %q has a bad checksum", ErrXDR, s)
	}

	var kind StrkeyKind
	switch payload[0] {
	case versionByteAccount:
		kind = StrkeyAccount
	case versionByteContract:
		kind = StrkeyContract
	case versionByteSeed:
		kind = StrkeySeed
	default:
		return nil, StrkeyUnknown, fmt.Errorf("%w: address %q has unknown version byte 0x%02x", ErrXDR, s, payload[0])
	}
	return payload[1:], kind, nil
}

// EncodeStrkey encodes 32 raw bytes as a strkey of the given kind.
func EncodeStrkey(raw []byte, kind StrkeyKind) (string, error) {
	if len(raw) != 32 {
		return "", fmt.Errorf("%w: strkey payload is %d bytes, want 32", ErrXDR, len(raw))
	}

	var version byte
	switch kind {
	case StrkeyAccount:
		version = versionByteAccount
	case StrkeyContract:
		version = versionByteContract
	case StrkeySeed:
		version = versionByteSeed
	default:
		return "", fmt.Errorf("%w: cannot encode unknown strkey kind", ErrXDR)
	}

	payload := make([]byte, 0, 35)
	payload = append(payload, version)
	payload = append(payload, raw...)
	sum := crc16XModem(payload)
	payload = append(payload, sum[0], sum[1])

	return base32NoPad.EncodeToString(payload), nil
}

// crc16XModem computes the CRC16-XModem checksum strkeys carry, little-endian.
func crc16XModem(data []byte) [2]byte {
	var crc uint16
	for _, b := range data {
		crc ^= uint16(b) << 8
		for i := 0; i < 8; i++ {
			if crc&0x8000 != 0 {
				crc = crc<<1 ^ 0x1021
			} else {
				crc <<= 1
			}
		}
	}
	return [2]byte{byte(crc), byte(crc >> 8)}
}
