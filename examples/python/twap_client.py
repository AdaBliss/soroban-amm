#!/usr/bin/env python3
import argparse
import os
import sys
from typing import Any

from stellar_sdk import Keypair, Network, scval
from stellar_sdk.contract import ContractClient

from common import (
    format_json,
    required_env,
    simulate_contract_call,
    submit_contract_call,
)

# Prices returned by the TWAP consumer use the same fixed-point scale as the
# AMM spot price. See the "Use the TWAP Oracle" section of the README.
PRICE_SCALE = 1_000_000

# Exit code used when the TWAP read fails because no sufficiently old snapshot
# exists yet.  This is an *expected* transient failure on the first run; callers
# can distinguish it from unexpected errors by checking for this code.
EXIT_NO_SNAPSHOT = 2


def main() -> int:
    argparse.ArgumentParser(description="Soroban AMM integration example").parse_args()
    rpc_url = os.getenv("STELLAR_RPC_URL", "https://soroban-testnet.stellar.org")
    network_passphrase = os.getenv(
        "STELLAR_NETWORK_PASSPHRASE",
        Network.TESTNET_NETWORK_PASSPHRASE,
    )

    twap_contract_id = required_env("TWAP_CONTRACT_ID")
    pool_contract_id = required_env("POOL_CONTRACT_ID")
    source_secret = required_env("SOURCE_SECRET")
    window_seconds = int(os.getenv("WINDOW_SECONDS", "60"))
    max_deviation_bps = int(os.getenv("MAX_DEVIATION_BPS", "500"))
    spot_price = os.getenv("SPOT_PRICE")
    # Set SAVE_SNAPSHOT=false to skip writing a fresh snapshot and only read.
    save = os.getenv("SAVE_SNAPSHOT", "true").lower() not in ("false", "0", "no")

    source_keypair = Keypair.from_secret(source_secret)

    client = ContractClient(
        contract_id=twap_contract_id,
        rpc_url=rpc_url,
        network_passphrase=network_passphrase,
    )

    print(f"Connected to {rpc_url}")
    print(f"TWAP consumer contract: {twap_contract_id}")
    print(f"Pool: {pool_contract_id}")
    print(f"Window: {window_seconds} seconds")

    # 1. Save a snapshot of the pool's cumulative price (state-changing).
    #    A TWAP over `window_seconds` needs a snapshot taken roughly
    #    `window_seconds` ago, so this is normally run on a schedule (e.g. a
    #    keeper invoking it every minute) rather than once.
    if save:
        print("\n1. Saving a price snapshot...")
        try:
            save_snapshot(client, source_keypair, pool_contract_id)
            print("Snapshot saved.")
        except Exception as e:
            print(f"Failed to save snapshot: {e}")
            client.server.close()
            return 1
    else:
        print("\n1. Skipping snapshot (SAVE_SNAPSHOT=false).")

    # 2. Read the time-weighted average price for token A in terms of token B.
    print(f"\n2. Reading TWAP over the last {window_seconds}s...")
    try:
        twap = get_twap_price(client, pool_contract_id, window_seconds)
        print(f"TWAP (A in terms of B): {twap} (raw), {twap / PRICE_SCALE} (scaled)")
    except Exception as e:
        print(
            "Could not read TWAP yet. A snapshot taken about "
            f"{window_seconds}s ago must exist first: {e}"
        )
        client.server.close()
        # EXIT_NO_SNAPSHOT (2) signals that this is an *expected* first-run
        # transient, not an unexpected failure.  Callers and CI can distinguish
        # it from a real error (exit 1) by checking the exit code.
        return EXIT_NO_SNAPSHOT

    # 3. Read both directions in a single call.
    print(f"\n3. Reading both TWAP directions over the last {window_seconds}s...")
    try:
        twap_a_to_b, twap_b_to_a = get_twap_both(client, pool_contract_id, window_seconds)
        print(f"TWAP A->B: {twap_a_to_b}")
        print(f"TWAP B->A: {twap_b_to_a}")
    except Exception as e:
        print(f"Failed to read both directions: {e}")
        client.server.close()
        return 1

    # 4. Optionally validate a real-time spot price against the TWAP.
    if spot_price:
        spot = int(spot_price)
        print(
            f"\n4. Validating spot price {spot} against TWAP "
            f"(max deviation {max_deviation_bps} bps)..."
        )
        try:
            validation = validate_price_against_twap(
                client, pool_contract_id, window_seconds, spot, max_deviation_bps
            )
            print(format_json(validation))
            if isinstance(validation, dict) and validation.get("is_deviation"):
                print("Spot price deviates from TWAP beyond the threshold.")
            else:
                print("Spot price is within the TWAP deviation threshold.")
        except Exception as e:
            print(f"Failed to validate price: {e}")
            client.server.close()
            return 1
    else:
        print("\n4. Skipping spot-price validation (SPOT_PRICE not set).")

    # 5. List every pool the consumer is tracking snapshots for.
    print("\n5. Reading tracked pools...")
    try:
        pools = get_tracked_pools(client)
        print(format_json(pools))
    except Exception as e:
        print(f"Failed to read tracked pools: {e}")
        client.server.close()
        return 1

    client.server.close()
    return 0


def save_snapshot(client: ContractClient, source_kp: Keypair, pool_id: str) -> None:
    submit_contract_call(
        client,
        source_kp,
        "save_snapshot",
        scval.to_address(pool_id),
    )


def get_twap_price(client: ContractClient, pool_id: str, window_seconds: int) -> int:
    result = simulate_contract_call(
        client,
        "get_twap_price",
        scval.to_address(pool_id),
        scval.to_uint64(window_seconds),
    )
    return int(result)


def get_twap_both(client: ContractClient, pool_id: str, window_seconds: int) -> Any:
    result = simulate_contract_call(
        client,
        "get_twap_both",
        scval.to_address(pool_id),
        scval.to_uint64(window_seconds),
    )
    # A Rust tuple `(i128, i128)` decodes to a two-element list.
    return int(result[0]), int(result[1])


def validate_price_against_twap(
    client: ContractClient,
    pool_id: str,
    window_seconds: int,
    spot_price: int,
    max_deviation_bps: int,
) -> dict[str, Any]:
    return simulate_contract_call(
        client,
        "validate_price_against_twap",
        scval.to_address(pool_id),
        scval.to_uint64(window_seconds),
        scval.to_int128(spot_price),
        scval.to_int128(max_deviation_bps),
    )


def get_tracked_pools(client: ContractClient) -> Any:
    return simulate_contract_call(client, "get_tracked_pools")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(exc, file=sys.stderr)
        raise SystemExit(1) from exc
