#!/usr/bin/env bash
# Reject generated files in the index, or changes left behind by tests.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
case "${1:-tracked}" in
    tracked)
        paths=$(mktemp)
        trap 'rm -f "$paths"' EXIT
        git ls-files -z > "$paths"
        failed=0
        while IFS= read -r -d '' path; do
            case "/$path/" in
                */test_snapshots/*|*/.DS_Store/|*/__pycache__/*|*.pyc/)
                    printf 'Generated artifact is tracked: %q\n' "$path" >&2
                    failed=1
                    ;;
            esac
        done < "$paths"
        if (( failed )); then
            echo 'Untrack these artifacts with git rm -r --cached -- <path> (keeps local files), then commit the removal.' >&2
            exit 1
        fi
        echo 'Repository hygiene: no generated artifacts are tracked.'
        ;;
    clean)
        status=$(git status --porcelain --untracked-files=all)
        if [[ -n "$status" ]]; then
            echo 'Tests/build steps left a dirty working tree:' >&2
            printf '%s\n' "$status" >&2
            echo 'Tests must not modify tracked files or leave unignored artifacts. Inspect the changes; untrack generated files with git rm --cached and add appropriate ignore rules.' >&2
            exit 1
        fi
        echo 'Repository hygiene: working tree is clean.'
        ;;
    *)
        echo 'Usage: bash scripts/check_repo_hygiene.sh [tracked|clean]' >&2
        exit 2
        ;;
esac
