#!/usr/bin/env bash
#
# Fail if a `*_test.rs` file under contracts/*/src is never compiled.
#
# Cargo only builds the files a crate reaches through `mod` declarations, so a
# `foo_test.rs` that no `mod foo_test;` names is silently skipped. A test
# module gated behind a feature nothing enables is skipped the same way. This
# check requires every `*_test.rs` under a crate's src/ to be declared with a
# `mod` line in that crate. Files under tests/ are discovered by Cargo and are
# not checked here.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
status=0

while IFS= read -r file; do
    name="$(basename "$file" .rs)"
    crate_src="${file%%/src/*}/src"
    if ! grep -rqE "^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?mod[[:space:]]+${name}[[:space:]]*[;{]" \
        --include='*.rs' "$crate_src"; then
        echo "${file#"$ROOT"/}: no \`mod ${name};\` declares it, so cargo never compiles it" >&2
        status=1
    fi
done < <(find "$ROOT"/contracts/*/src -name '*_test.rs' -type f 2>/dev/null | sort)

if [ "$status" -eq 0 ]; then
    echo "every *_test.rs file under contracts/*/src is declared as a module"
fi
exit "$status"
