package gosdk

import (
	"context"
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/base64"
	"errors"
	"net/http"
	"testing"
)

func TestKeypairSignerSignsStellarTransactionHash(t *testing.T) {
	seed := make([]byte, ed25519.SeedSize)
	for i := range seed {
		seed[i] = byte(i + 1)
	}
	secret, err := EncodeStrkey(seed, StrkeySeed)
	if err != nil {
		t.Fatal(err)
	}
	signer, err := NewKeypairSigner(secret)
	if err != nil {
		t.Fatal(err)
	}
	envelope, err := BuildInvokeEnvelope(InvokeSpec{
		SourceAccount: signer.Address(), ContractID: zeroContract,
		Method: "swap", NetworkPassphrase: NetworkTestnet,
	})
	if err != nil {
		t.Fatal(err)
	}
	signed, err := signer.SignEnvelope(context.Background(), envelope, NetworkTestnet)
	if err != nil {
		t.Fatal(err)
	}
	raw, err := base64.StdEncoding.DecodeString(signed)
	if err != nil {
		t.Fatal(err)
	}
	if len(raw) < 76 {
		t.Fatalf("signed envelope too short: %d", len(raw))
	}
	signatureStart := len(raw) - 64
	countOffset := len(raw) - 76
	if raw[countOffset+3] != 1 || raw[countOffset] != 0 || raw[countOffset+1] != 0 || raw[countOffset+2] != 0 {
		t.Fatal("signature vector count is not one")
	}
	publicRaw, _, err := DecodeStrkey(signer.Address())
	if err != nil {
		t.Fatal(err)
	}
	if string(raw[countOffset+4:countOffset+8]) != string(publicRaw[28:]) {
		t.Fatal("decorated signature hint does not match public key")
	}
	networkID := sha256.Sum256([]byte(NetworkTestnet))
	preimage := append(append(append([]byte{}, networkID[:]...), raw[:4]...), raw[4:countOffset]...)
	hash := sha256.Sum256(preimage)
	if !ed25519.Verify(ed25519.PublicKey(publicRaw), hash[:], raw[signatureStart:]) {
		t.Fatal("signature does not verify against the Stellar transaction hash")
	}
}

func corruptStrkey(addr string) string {
	corrupt := []byte(addr)
	if corrupt[10] == 'A' {
		corrupt[10] = 'B'
	} else {
		corrupt[10] = 'A'
	}
	return string(corrupt)
}

func TestIsAccountAddressRejectsCorruptedChecksum(t *testing.T) {
	corrupt := corruptStrkey(zeroAccount)

	if IsAccountAddress(corrupt) {
		t.Fatalf("checksum-corrupted account address %q must be rejected", corrupt)
	}
}

func TestIsContractAddressRejectsCorruptedChecksum(t *testing.T) {
	corrupt := corruptStrkey(zeroContract)

	if IsContractAddress(corrupt) {
		t.Fatalf("checksum-corrupted contract address %q must be rejected", corrupt)
	}
}

func TestAddressHelpersAcceptValidAddresses(t *testing.T) {
	if !IsAccountAddress(zeroAccount) {
		t.Fatalf("zeroAccount should be accepted as a valid account address")
	}

	if IsContractAddress(zeroAccount) {
		t.Fatalf("zeroAccount must not be accepted as a contract address")
	}

	if !IsContractAddress(zeroContract) {
		t.Fatalf("zeroContract should be accepted as a valid contract address")
	}

	if IsAccountAddress(zeroContract) {
		t.Fatalf("zeroContract must not be accepted as an account address")
	}
}

func TestNewClientRejectsChecksumCorruptedSourceAccount(t *testing.T) {
	corrupt := corruptStrkey(zeroAccount)

	client, err := NewClient(Config{
		RPCURL:            "http://localhost:8000",
		NetworkPassphrase: NetworkTestnet,
		SourceAccount:     corrupt,
	})
	if !errors.Is(err, ErrInvalidConfig) {
		t.Fatalf("expected ErrInvalidConfig, got %v", err)
	}
	if client != nil {
		t.Fatal("NewClient must not return a client for a checksum-corrupted source account")
	}
}

func TestValidateSignerRejectsChecksumCorruptedAddress(t *testing.T) {
	corrupt := corruptStrkey(zeroAccount)

	signer := SignerFunc{
		Addr: corrupt,
		Sign: func(ctx context.Context, envelopeXDR string, networkPassphrase string) (string, error) {
			return envelopeXDR, nil
		},
	}

	err := ValidateSigner(signer)
	if !errors.Is(err, ErrSignerAddress) {
		t.Fatalf("expected ErrSignerAddress, got %v", err)
	}
}

func TestSharesOfRejectsChecksumCorruptedProviderBeforeRPC(t *testing.T) {
	corrupt := corruptStrkey(zeroAccount)

	client, err := NewClient(Config{
		RPCURL:            "http://localhost:8000",
		NetworkPassphrase: NetworkTestnet,
		SourceAccount:     zeroAccount,
	})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}

	client.WithHTTPClient(&http.Client{
		Transport: roundTripFunc(func(req *http.Request) (*http.Response, error) {
			t.Fatalf("SharesOf must reject the provider before making an HTTP request")
			return nil, nil
		}),
	})

	_, err = client.SharesOf(context.Background(), zeroContract, corrupt)
	if !errors.Is(err, ErrInvalidConfig) {
		t.Fatalf("expected ErrInvalidConfig, got %v", err)
	}
}

type roundTripFunc func(*http.Request) (*http.Response, error)

func (f roundTripFunc) RoundTrip(req *http.Request) (*http.Response, error) {
	return f(req)
}
