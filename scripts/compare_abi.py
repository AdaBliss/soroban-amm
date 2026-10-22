#!/usr/bin/env python3
"""Print a compact contract/function summary when two ABI JSON files differ."""

import json
import sys


def read_abi(path):
    with open(path, encoding="utf-8") as stream:
        data = json.load(stream)
    return data.get("contracts", {})


def function_entries(contract):
    entries = contract if isinstance(contract, list) else contract.get("functions", [])
    result = {}
    for entry in entries:
        function = entry.get("function_v0", entry)
        name = function.get("name") if isinstance(function, dict) else None
        if name:
            result[name] = function
    return result


def non_function_data(contract):
    if isinstance(contract, list):
        return [entry for entry in contract if "function_v0" not in entry]
    return {key: value for key, value in contract.items() if key != "functions"}


def names(values):
    return ", ".join(sorted(values)) or "(none)"


def main(expected_path, generated_path):
    expected = read_abi(expected_path)
    generated = read_abi(generated_path)
    expected_names = set(expected)
    generated_names = set(generated)

    print(f"Contracts added: {names(generated_names - expected_names)}")
    print(f"Contracts removed: {names(expected_names - generated_names)}")

    changes = 0
    for contract_name in sorted(expected_names | generated_names):
        before = expected.get(contract_name)
        after = generated.get(contract_name)
        if before is None or after is None:
            continue

        before_functions = function_entries(before)
        after_functions = function_entries(after)
        before_names = set(before_functions)
        after_names = set(after_functions)
        added = after_names - before_names
        removed = before_names - after_names
        changed = {
            name for name in before_names & after_names
            if before_functions[name] != after_functions[name]
        }
        other_changed = non_function_data(before) != non_function_data(after)
        if added or removed or changed or other_changed:
            changes += 1
            details = []
            if added:
                details.append(f"functions added [{names(added)}]")
            if removed:
                details.append(f"functions removed [{names(removed)}]")
            if changed:
                details.append(f"function signatures/specs changed [{names(changed)}]")
            if other_changed:
                details.append("non-function ABI entries changed")
            print(f"{contract_name}: " + "; ".join(details))

    if changes == 0 and expected == generated:
        print("ABI contents match; only JSON formatting or key ordering differs.")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("Usage: compare_abi.py EXPECTED.json GENERATED.json", file=sys.stderr)
        sys.exit(2)
    try:
        main(sys.argv[1], sys.argv[2])
    except (OSError, json.JSONDecodeError) as error:
        print(f"ERROR: Could not compare ABI JSON: {error}", file=sys.stderr)
        sys.exit(2)
