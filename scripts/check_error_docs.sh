#!/usr/bin/env bash
set -euo pipefail

# Locate repository root relative to script location
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
import sys
import os
import glob
import re

if hasattr(sys.stdout, 'reconfigure'):
    sys.stdout.reconfigure(encoding='utf-8')

root_dir = os.environ.get('ROOT_DIR', '.')
contracts_dir = os.path.join(root_dir, 'contracts')
doc_file = os.path.join(root_dir, 'docs', 'error-codes.md')

if not os.path.exists(doc_file):
    print(f"ERROR: Documentation file missing at {doc_file}", file=sys.stderr)
    sys.exit(1)

# 1. Parse all #[contracterror] enums from Rust files under contracts/
rs_files = sorted(glob.glob(os.path.join(contracts_dir, '**', '*.rs'), recursive=True))
code_enums = {}
conflicts = []

for path in rs_files:
    with open(path, 'r', encoding='utf-8') as f:
        content = f.read()
    if '#[contracterror]' in content:
        # Match #[contracterror] block followed by pub enum EnumName { ... }
        matches = re.finditer(r'#\[contracterror\]\s*(?:#\[[^\]]+\]\s*)*pub\s+enum\s+(\w+)\s*\{([^}]+)\}', content, re.MULTILINE)
        for m in matches:
            enum_name = m.group(1)
            body = m.group(2)
            variants = {}
            for line in body.splitlines():
                line = line.strip()
                if not line or line.startswith('//') or line.startswith('///') or line.startswith('#'):
                    continue
                v_match = re.match(r'(\w+)\s*=\s*(\d+)', line)
                if v_match:
                    variants[v_match.group(1)] = int(v_match.group(2))
            rel_path = os.path.relpath(path, root_dir)
            if enum_name not in code_enums:
                code_enums[enum_name] = {'file': rel_path, 'variants': variants}
                continue
            # The same enum name can be declared more than once, e.g. a
            # client-side mirror that only lists the variants a caller needs.
            # Merge them instead of letting the last file read win, and flag
            # any variant the copies disagree on.
            known = code_enums[enum_name]
            for var, code_val in variants.items():
                if known['variants'].get(var, code_val) != code_val:
                    conflicts.append(
                        f"`{enum_name}::{var}` is {known['variants'][var]} in {known['file']} "
                        f"but {code_val} in {rel_path}"
                    )
                known['variants'].setdefault(var, code_val)

# 2. Parse docs/error-codes.md
with open(doc_file, 'r', encoding='utf-8') as f:
    doc_content = f.read()

doc_enums = {}
duplicate_enums = set()
current_enum = None

lines = doc_content.splitlines()
for line in lines:
    # Match "Defined in ... as `EnumName`." or "Defined in ... as EnumName"
    m_defined = re.search(r'Defined in .* as [`\s]*(\w+)[`\s\.]*', line)
    if m_defined:
        candidate = m_defined.group(1)
        if candidate in code_enums:
            if candidate in doc_enums:
                duplicate_enums.add(candidate)
            current_enum = candidate
            if current_enum not in doc_enums:
                doc_enums[current_enum] = {}
            continue

    # Match markdown table row: | 1 | `VariantName` | ...
    table_match = re.match(r'^\|\s*(\d+)\s*\|\s*[`\s]*(\w+)[`\s]*\|', line)
    if table_match and current_enum:
        code_val = int(table_match.group(1))
        symbol_val = table_match.group(2)
        doc_enums[current_enum][symbol_val] = code_val

# 3. Validate code vs docs
errors = [f"Conflicting definitions: {c}" for c in conflicts]

for enum_name, data in code_enums.items():
    code_vars = data['variants']
    file_path = data['file']
    
    if enum_name not in doc_enums:
        errors.append(f"Missing section for enum `{enum_name}` (defined in {file_path}) in docs/error-codes.md")
        continue

    doc_vars = doc_enums[enum_name]

    # Check for missing variants or mismatched numeric values
    for var, code_val in code_vars.items():
        if var not in doc_vars:
            errors.append(f"Missing variant `{var}` (code {code_val}) for `{enum_name}` in docs/error-codes.md")
        elif doc_vars[var] != code_val:
            errors.append(f"Mismatched discriminant for `{enum_name}::{var}`: code has {code_val}, docs has {doc_vars[var]}")

    # Check for undocumented / obsolete variants in docs
    for doc_var, doc_val in doc_vars.items():
        if doc_var not in code_vars:
            errors.append(f"Extra variant `{doc_var}` documented for `{enum_name}` in docs/error-codes.md but does not exist in code")

for dup in duplicate_enums:
    errors.append(f"Duplicate documentation section found for enum `{dup}` in docs/error-codes.md")

if errors:
    print("❌ Error code documentation sync check FAILED:\n", file=sys.stderr)
    for err in errors:
        print(f"  • {err}", file=sys.stderr)
    sys.exit(1)
else:
    print(f"✅ Success: All {len(code_enums)} contracterror enums ({sum(len(d['variants']) for d in code_enums.values())} variants) match docs/error-codes.md perfectly.")
    sys.exit(0)
EOF
