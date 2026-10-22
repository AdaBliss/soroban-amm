package gosdk

import (
	"context"
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/base64"
	"errors"
	"fmt"
)

// Signer signs transaction envelopes. It is an interface so callers can back it
// with a local keypair, an HSM, or a remote signing service; the client never
// holds a raw secret key.
type Signer interface {
	// Address returns the signer's account address (a G... strkey).
	Address() string
	// SignEnvelope signs a base64-encoded transaction envelope XDR for the
	// given network passphrase and returns the signed envelope, also base64.
	SignEnvelope(ctx context.Context, envelopeXDR string, networkPassphrase string) (string, error)
}

// ErrSignerAddress is returned when a Signer reports an address that is not a
// valid Stellar account strkey.
var ErrSignerAddress = errors.New("signer address is not a valid account address")

type keypairSigner struct {
	address string
	public  ed25519.PublicKey
	private ed25519.PrivateKey
}

// NewKeypairSigner creates a local ed25519 signer from an S... Stellar seed.
// The secret is retained only in the returned signer and is never serialized
// into transaction diagnostics or RPC requests.
func NewKeypairSigner(secret string) (Signer, error) {
	seed, kind, err := DecodeStrkey(secret)
	if err != nil || kind != StrkeySeed {
		return nil, fmt.Errorf("%w: invalid secret seed", ErrSignerAddress)
	}
	private := ed25519.NewKeyFromSeed(seed)
	public := private.Public().(ed25519.PublicKey)
	address, err := EncodeStrkey(public, StrkeyAccount)
	if err != nil {
		return nil, fmt.Errorf("%w: deriving address: %v", ErrSignerAddress, err)
	}
	return &keypairSigner{address: address, public: public, private: private}, nil
}

func (s *keypairSigner) Address() string { return s.address }

func (s *keypairSigner) SignEnvelope(_ context.Context, envelopeXDR, networkPassphrase string) (string, error) {
	raw, err := base64.StdEncoding.DecodeString(envelopeXDR)
	if err != nil || len(raw) < 12 {
		return "", fmt.Errorf("%w: invalid transaction envelope", ErrXDR)
	}
	if int32(uint32(raw[0])<<24|uint32(raw[1])<<16|uint32(raw[2])<<8|uint32(raw[3])) != envelopeTypeTx {
		return "", fmt.Errorf("%w: unsupported envelope type", ErrXDR)
	}
	if raw[len(raw)-4] != 0 || raw[len(raw)-3] != 0 || raw[len(raw)-2] != 0 || raw[len(raw)-1] != 0 {
		return "", fmt.Errorf("%w: envelope is already signed", ErrXDR)
	}

	networkID := sha256.Sum256([]byte(networkPassphrase))
	preimage := make([]byte, 0, 32+4+len(raw)-8)
	preimage = append(preimage, networkID[:]...)
	preimage = append(preimage, raw[:4]...)
	preimage = append(preimage, raw[4:len(raw)-4]...)
	hash := sha256.Sum256(preimage)
	signature := ed25519.Sign(s.private, hash[:])

	var w xdrWriter
	w.writeRaw(raw[:len(raw)-4])
	w.writeUint32(1)
	w.writeRaw(s.public[len(s.public)-4:])
	w.writeBytes(signature)
	return base64.StdEncoding.EncodeToString(w.bytes()), nil
}

// SignerFunc adapts a function to the Signer interface, pairing it with a fixed
// address.
type SignerFunc struct {
	// Addr is the account address the signing function signs for.
	Addr string
	// Sign performs the signing.
	Sign func(ctx context.Context, envelopeXDR string, networkPassphrase string) (string, error)
}

// Address returns the configured address.
func (s SignerFunc) Address() string { return s.Addr }

// SignEnvelope calls the configured signing function.
func (s SignerFunc) SignEnvelope(ctx context.Context, envelopeXDR string, networkPassphrase string) (string, error) {
	if s.Sign == nil {
		return "", ErrNoSigner
	}
	return s.Sign(ctx, envelopeXDR, networkPassphrase)
}

// ValidateSigner checks that a Signer reports a valid Stellar account address.
func ValidateSigner(s Signer) error {
	if s == nil {
		return ErrNoSigner
	}
	if !IsAccountAddress(s.Address()) {
		return ErrSignerAddress
	}
	return nil
}

// IsAccountAddress reports whether addr is a valid Stellar account strkey.
//
// Validation is delegated to DecodeStrkey so the helper verifies the version
// byte, base32 encoding, payload length, and CRC16-XModem checksum.
func IsAccountAddress(addr string) bool {
	_, kind, err := DecodeStrkey(addr)
	return err == nil && kind == StrkeyAccount
}

// IsContractAddress reports whether addr is a valid Soroban contract strkey.
//
// Validation is delegated to DecodeStrkey so the helper verifies the version
// byte, base32 encoding, payload length, and CRC16-XModem checksum.
func IsContractAddress(addr string) bool {
	_, kind, err := DecodeStrkey(addr)
	return err == nil && kind == StrkeyContract
}
