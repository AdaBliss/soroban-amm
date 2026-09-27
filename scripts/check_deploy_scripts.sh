#!/usr/bin/env bash
# check_deploy_scripts.sh — fail when a deployable workspace contract is not
# wired into scripts/deploy.sh, so the deploy set cannot silently fall out of
# sync with the workspace again (issue #959).
#
# A deployable contract is a workspace member under contracts/ that builds a
# cdylib and defines a #[contract]. For each one this checks that:
#   1. scripts/deploy/<crate>.sh exists;
#   2. <crate> is a step in ALL_CONTRACTS (scripts/deploy/common.sh), which
#      is also the list deploy.sh sources its modules from;
#   3. deploy.sh's main() calls a deploy_* function that module defines.
# It also rejects ALL_CONTRACTS entries that are neither a contract nor a
# known synthetic step.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Detect working python executable (avoiding WindowsApps stubs)
if command -v python3 >/dev/null 2>&1 && python3 -c "import sys" >/dev/null 2>&1; then
    PYTHON_CMD="python3"
elif command -v python >/dev/null 2>&1 && python -c "import sys" >/dev/null 2>&1; then
    PYTHON_CMD="python"
else
    echo "ERROR: Neither python3 nor python is working in PATH" >&2
    exit 1
fi

export ROOT_DIR

"$PYTHON_CMD" - << 'EOF'
import glob
import os
import re
import sys

try:
    import tomllib
except ModuleNotFoundError:
    print("ERROR: Python 3.11+ is required (tomllib)", file=sys.stderr)
    sys.exit(1)

root = os.environ.get("ROOT_DIR", ".")

# Steps with a module but no contract crate of their own.
SYNTHETIC_STEPS = {"pools"}
# Contracts whose WASM deploy_factory uploads and whose instances
# deploy_pools creates through the factory, so main() never calls their own
# deploy_* function directly.
DEPLOYED_VIA_FACTORY = {"amm", "concentrated_liquidity"}

CONTRACT_ATTR = re.compile(r"^\s*#\[contract\]\s*$", re.M)


def load_toml(path):
    with open(path, "rb") as f:
        return tomllib.load(f)


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


contracts = []
for member in load_toml(os.path.join(root, "Cargo.toml"))["workspace"]["members"]:
    if not member.startswith("contracts/"):
        continue
    crate_dir = os.path.join(root, member)
    manifest = load_toml(os.path.join(crate_dir, "Cargo.toml"))
    if "cdylib" not in manifest.get("lib", {}).get("crate-type", []):
        continue
    sources = [
        p for p in glob.glob(os.path.join(crate_dir, "src", "**", "*.rs"), recursive=True)
        # Test-only contracts (e.g. amm-sdk's version_test.rs) are not deployable.
        if "test" not in os.path.basename(p) and f"{os.sep}tests{os.sep}" not in p
    ]
    if not any(CONTRACT_ATTR.search(read(p)) for p in sources):
        continue
    contracts.append(manifest["package"]["name"])

common = read(os.path.join(root, "scripts", "deploy", "common.sh"))
steps_block = re.search(r"^ALL_CONTRACTS=\(\n(.*?)^\)", common, re.S | re.M)
if not steps_block:
    print("ERROR: could not find ALL_CONTRACTS in scripts/deploy/common.sh", file=sys.stderr)
    sys.exit(1)
steps = [line.strip() for line in steps_block.group(1).splitlines() if line.strip()]

deploy_sh = read(os.path.join(root, "scripts", "deploy.sh"))
main_body = re.search(r"^main\(\) \{\n(.*?)^\}", deploy_sh, re.S | re.M)
if not main_body:
    print("ERROR: could not find main() in scripts/deploy.sh", file=sys.stderr)
    sys.exit(1)
main_calls = set(re.findall(r"^\s*(deploy_\w+)\s*$", main_body.group(1), re.M))

errors = []
for name in sorted(contracts):
    module = os.path.join(root, "scripts", "deploy", f"{name}.sh")
    if not os.path.isfile(module):
        errors.append(f"{name}: no deploy script (expected scripts/deploy/{name}.sh)")
        continue
    if name not in steps:
        errors.append(f"{name}: not listed in ALL_CONTRACTS in scripts/deploy/common.sh")
    defined = set(re.findall(r"^(deploy_\w+)\(\)", read(module), re.M))
    if not defined:
        errors.append(f"{name}: scripts/deploy/{name}.sh defines no deploy_* function")
    elif name not in DEPLOYED_VIA_FACTORY and not defined & main_calls:
        errors.append(
            f"{name}: main() in scripts/deploy.sh never calls "
            f"{' / '.join(sorted(defined))}"
        )

for step in steps:
    if step not in contracts and step not in SYNTHETIC_STEPS:
        errors.append(f"ALL_CONTRACTS lists '{step}', which is not a deployable workspace contract")

if errors:
    print("Deploy scripts are out of sync with the workspace:", file=sys.stderr)
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)

print(f"OK: all {len(contracts)} deployable contracts have a deploy script wired into scripts/deploy.sh")
EOF
