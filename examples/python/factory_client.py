#!/usr/bin/env python3
import argparse
import os
import sys

from stellar_sdk import Keypair, Network, scval
from stellar_sdk.address import Address
from stellar_sdk.contract import ContractClient

from common import (
    format_json,
    required_env,
    simulate_contract_call,
    submit_contract_call,
)


def main() -> int:
    argparse.ArgumentParser(description="Soroban AMM integration example").parse_args()
    rpc_url = os.getenv("STELLAR_RPC_URL", "https://soroban-testnet.stellar.org")
    network_passphrase = os.getenv(
        "STELLAR_NETWORK_PASSPHRASE",
        Network.TESTNET_NETWORK_PASSPHRASE,
    )

    factory_contract_id = required_env("FACTORY_CONTRACT_ID")
    source_secret = required_env("SOURCE_SECRET")
    token_a_contract_id = required_env("TOKEN_A_CONTRACT_ID")
    token_b_contract_id = required_env("TOKEN_B_CONTRACT_ID")
    fee_bps = int(os.getenv("FEE_BPS", "30"))

    source_keypair = Keypair.from_secret(source_secret)

    client = ContractClient(
        contract_id=factory_contract_id,
        rpc_url=rpc_url,
        network_passphrase=network_passphrase,
    )

    print(f"Connected to {rpc_url}")
    print(f"Factory contract: {factory_contract_id}")

    # 1. get_pool (read-only query)
    print("\n1. Querying if pool exists for pair...")
    pool_address = get_pool(client, token_a_contract_id, token_b_contract_id)
    print(f"Pool address: {pool_address}")

    # 2. create_pool if it doesn't exist
    if not pool_address:
        print(f"\n2. Creating pool for pair with fee_bps={fee_bps}...")
        try:
            pool_address, governance_address = create_pool(
                client, source_keypair, source_keypair.public_key, token_a_contract_id, token_b_contract_id, fee_bps
            )
            print(f"Pool created successfully at: {pool_address}")
            if governance_address:
                print(f"Governance contract deployed at: {governance_address}")
            else:
                print("No governance contract was deployed (governance_wasm_hash not supplied).")
        except Exception as e:
            print(f"Failed to create pool (might already exist): {e}")
            client.server.close()
            return 1
    else:
        print("\n2. Pool already exists, skipping create_pool.")

    # 3. all_pools
    print("\n3. Listing all pools deployed by this factory...")
    pools = all_pools(client)
    print(format_json(pools))

    client.server.close()
    return 0


def create_pool(
    client: ContractClient,
    source_kp: Keypair,
    caller: str,
    token_a: str,
    token_b: str,
    fee_bps: int,
    governance_wasm_hash: bytes | None = None,
) -> tuple[str, str | None]:
    result = submit_contract_call(
        client,
        source_kp,
        "create_pool",
        scval.to_address(caller),
        scval.to_address(token_a),
        scval.to_address(token_b),
        scval.to_int128(fee_bps),
        scval.to_bytes(governance_wasm_hash) if governance_wasm_hash is not None else scval.to_void(),
    )

    pool_result, governance_result = result

    pool_address = pool_result.address if isinstance(pool_result, Address) else str(pool_result)
    governance_address: str | None = None
    if isinstance(governance_result, Address):
        governance_address = governance_result.address
    elif governance_result is not None:
        governance_address = str(governance_result)

    return pool_address, governance_address


def get_pool(client: ContractClient, token_a: str, token_b: str) -> str | None:
    result = simulate_contract_call(
        client,
        "get_pool",
        scval.to_address(token_a),
        scval.to_address(token_b),
    )
    if not result:
        return None
    if isinstance(result, Address):
        return result.address
    return str(result)


def all_pools(client: ContractClient) -> list[str]:
    result = simulate_contract_call(client, "all_pools")
    if not result:
        return []
    if isinstance(result, list):
        return [r.address if isinstance(r, Address) else str(r) for r in result]
    return [str(result)]


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(exc, file=sys.stderr)
        raise SystemExit(1) from exc
