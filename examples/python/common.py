"""Shared, importable helpers for the Soroban AMM Python examples."""
from __future__ import annotations

import json
import os
from typing import Any

from stellar_sdk import Network, scval
from stellar_sdk.address import Address
from stellar_sdk.contract import ContractClient

I128_MIN = -(1 << 127)
I128_MAX = (1 << 127) - 1


class ConfigError(ValueError):
    """Raised when required example configuration is absent or invalid."""


class InvocationError(RuntimeError):
    """Raised when a Soroban simulation or submission fails."""


def required_env(name: str) -> str:
    """Return a non-empty environment variable or raise ConfigError."""
    value = os.getenv(name)
    if not value:
        raise ConfigError(f"Missing required environment variable: {name}")
    return value


def optional_env(name: str, default: str | None = None) -> str | None:
    """Return an optional environment variable, using *default* when absent."""
    return os.getenv(name, default)


def parse_i128(value: str) -> int:
    """Parse and range-check a signed Soroban i128 decimal value."""
    if not isinstance(value, str) or not value.strip() or not value.strip().lstrip("-").isdigit():
        raise ValueError(f'Expected an integer i128 value, got "{value}"')
    parsed = int(value)
    if parsed < I128_MIN or parsed > I128_MAX:
        raise ValueError(f"Value {value} is outside the i128 range")
    return parsed


def encode_i128(value: int) -> Any:
    """Encode an integer as a Soroban i128 ScVal."""
    parse_i128(str(value))
    return scval.to_int128(value)


def encode_address(value: str) -> Any:
    """Encode a Stellar account or contract address as an ScVal."""
    return scval.to_address(value)


def encode_bool(value: bool) -> Any:
    """Encode a boolean as an ScVal."""
    return scval.to_bool(value)


def encode_string(value: str) -> Any:
    """Encode a string as a Soroban string ScVal."""
    return scval.to_string(value)


def decode_scval(value: Any) -> Any:
    """Decode an ScVal through the SDK's native conversion helper."""
    return scval.to_native(value)


def decode_enum_variant(value: Any) -> str:
    """Decode a Soroban unit-enum ScVal to its variant name string.

    ``scval.to_native`` decodes a unit enum variant (e.g. ``ProposalStatus::Active``)
    as a one-element list ``['Active']``.  Dict-style variants (e.g. a tuple variant)
    come back as ``{'VariantName': [...args]}``.  This helper normalises both shapes
    to a plain string so callers can compare with ``== "Active"`` instead of
    special-casing every possible shape.
    """
    if isinstance(value, list) and len(value) == 1 and isinstance(value[0], str):
        # Unit variant: ['Active']
        return value[0]
    if isinstance(value, dict) and len(value) == 1:
        # Tuple/struct variant: {'Active': [...]} — return the key
        return next(iter(value.keys()))
    if isinstance(value, str):
        return value
    return str(value)


def build_client(contract_id: str) -> ContractClient:
    """Build a ContractClient from the common Stellar environment settings."""
    rpc = optional_env("STELLAR_RPC_URL", "https://soroban-testnet.stellar.org") or "https://soroban-testnet.stellar.org"
    net = optional_env("STELLAR_NETWORK_PASSPHRASE", Network.TESTNET_NETWORK_PASSPHRASE) or Network.TESTNET_NETWORK_PASSPHRASE
    return ContractClient(
        contract_id=contract_id,
        rpc_url=rpc,
        network_passphrase=net,
    )


def invoke(client: ContractClient, method: str, *parameters: Any) -> Any:
    """Run a read-only call and normalize failures into InvocationError."""
    try:
        return client.invoke(method, parameters=list(parameters), parse_result_xdr_fn=scval.to_native).result()
    except Exception as exc:
        raise InvocationError(f"Contract call {method!r} failed; see docs/error-codes.md: {exc}") from exc


def submit(client: ContractClient, source_keypair: Any, method: str, *parameters: Any) -> Any:
    """Submit a signed call and normalize failures into InvocationError."""
    try:
        assembled = client.invoke(method, parameters=list(parameters), source=source_keypair.public_key, signer=source_keypair, parse_result_xdr_fn=scval.to_native)
        assembled.sign_auth_entries(source_keypair)
        return assembled.sign_and_submit()
    except Exception as exc:
        raise InvocationError(f"Contract submission {method!r} failed; see docs/error-codes.md: {exc}") from exc


def format_json(value: Any) -> str:
    """Format SDK-native values as readable JSON."""
    return json.dumps(normalize_for_json(value), indent=2)


def normalize_for_json(value: Any) -> Any:
    """Convert Stellar addresses, bytes, mappings, lists, and integers to JSON values."""
    if isinstance(value, Address):
        return value.address
    if isinstance(value, bytes):
        return value.hex()
    if isinstance(value, int):
        return value
    if isinstance(value, list):
        return [normalize_for_json(entry) for entry in value]
    if isinstance(value, dict):
        return {
            str(normalize_for_json(key)): normalize_for_json(entry)
            for key, entry in value.items()
        }
    return value


def simulate_contract_call(client: ContractClient, method: str, *parameters: Any) -> Any:
    """Backward-compatible name for a safe read-only invocation."""
    return invoke(client, method, *parameters)


def submit_contract_call(client: ContractClient, source_keypair: Any, method: str, *parameters: Any) -> Any:
    """Backward-compatible name for a safe signed invocation."""
    return submit(client, source_keypair, method, *parameters)
