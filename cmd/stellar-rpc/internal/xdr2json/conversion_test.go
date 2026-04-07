package xdr2json

import (
	"encoding/json"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"

	"github.com/stellar/go-stellar-sdk/keypair"
	"github.com/stellar/go-stellar-sdk/xdr"
)

func TestConversion(t *testing.T) {
	// Make a structure to encode
	pubkey := keypair.MustRandom()
	asset := xdr.MustNewCreditAsset("ABCD", pubkey.Address())

	// Try the all-inclusive version
	jsi, err := ConvertInterface(asset)
	require.NoError(t, err)

	// Try the byte-and-interface version
	rawBytes, err := asset.MarshalBinary()
	require.NoError(t, err)
	jsb, err := ConvertBytes(xdr.Asset{}, rawBytes)
	require.NoError(t, err)

	for _, rawJs := range []json.RawMessage{jsi, jsb} {
		var dest map[string]interface{}
		require.NoError(t, json.Unmarshal(rawJs, &dest))

		require.Contains(t, dest, "credit_alphanum4")
		require.Contains(t, dest["credit_alphanum4"], "asset_code")
		require.Contains(t, dest["credit_alphanum4"], "issuer")
		require.IsType(t, map[string]interface{}{}, dest["credit_alphanum4"])
		if converted, ok := dest["credit_alphanum4"].(map[string]interface{}); assert.True(t, ok) {
			require.Equal(t, pubkey.Address(), converted["issuer"])
		}
	}
}

func TestEmptyConversion(t *testing.T) {
	js, err := ConvertBytes(xdr.SorobanTransactionData{}, []byte{})
	require.NoError(t, err)
	require.Empty(t, string(js))
}

func TestConvertBytesSlice(t *testing.T) {
	// Create multiple assets to convert
	const n = 10
	bytesSlice := make([][]byte, n)
	for i := 0; i < n; i++ {
		pubkey := keypair.MustRandom()
		asset := xdr.MustNewCreditAsset("ABCD", pubkey.Address())
		rawBytes, err := asset.MarshalBinary()
		require.NoError(t, err)
		bytesSlice[i] = rawBytes
	}

	// Convert using batch
	batchResults, err := ConvertBytesSlice(xdr.Asset{}, bytesSlice)
	require.NoError(t, err)
	require.Len(t, batchResults, n)

	// Convert individually and compare
	for i, raw := range bytesSlice {
		individual, err := ConvertBytes(xdr.Asset{}, raw)
		require.NoError(t, err)
		require.JSONEq(t, string(individual), string(batchResults[i]),
			"mismatch at index %d", i)
	}
}

func TestConvertBytesSliceEmpty(t *testing.T) {
	results, err := ConvertBytesSlice(xdr.Asset{}, nil)
	require.NoError(t, err)
	require.Empty(t, results)
}

func TestConvertBytesSliceWithEmptyElement(t *testing.T) {
	pubkey := keypair.MustRandom()
	asset := xdr.MustNewCreditAsset("ABCD", pubkey.Address())
	rawBytes, err := asset.MarshalBinary()
	require.NoError(t, err)

	results, err := ConvertBytesSlice(xdr.Asset{}, [][]byte{rawBytes, {}, rawBytes})
	require.NoError(t, err)
	require.Len(t, results, 3)
	require.NotEmpty(t, string(results[0]))
	require.Equal(t, "", string(results[1]))
	require.JSONEq(t, string(results[0]), string(results[2]))
}

func BenchmarkConvertBytesVsSlice(b *testing.B) {
	// Build a batch of DiagnosticEvent XDR byte buffers
	const batchSize = 50
	events := make([][]byte, batchSize)
	for i := 0; i < batchSize; i++ {
		pubkey := keypair.MustRandom()
		asset := xdr.MustNewCreditAsset("ABCD", pubkey.Address())
		raw, err := asset.MarshalBinary()
		require.NoError(b, err)
		events[i] = raw
	}

	b.Run("Individual", func(b *testing.B) {
		for b.Loop() {
			for _, ev := range events {
				_, err := ConvertBytes(xdr.Asset{}, ev)
				if err != nil {
					b.Fatal(err)
				}
			}
		}
	})

	b.Run("Batch", func(b *testing.B) {
		for b.Loop() {
			_, err := ConvertBytesSlice(xdr.Asset{}, events)
			if err != nil {
				b.Fatal(err)
			}
		}
	})
}
