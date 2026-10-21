#!/usr/bin/env bash
# Exercise the real guard in disposable repositories, never the caller's index.
set -euo pipefail
guard="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check_repo_hygiene.sh"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
# Do not inherit a caller's index/worktree overrides into fixture repositories.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
count=0

new_repo() {
    count=$((count + 1))
    mkdir "$scratch/$count"
    cd "$scratch/$count"
    git init -q
    printf 'original\n' > source.txt
    printf '**/test_snapshots/\n' > .gitignore
    git add source.txt .gitignore
    git -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm baseline
}

reject() {
    if bash "$guard" "$1" > "$scratch/output" 2>&1; then
        echo "FAIL: $2 was accepted" >&2
        exit 1
    fi
    grep -q "$3" "$scratch/output"
    echo "PASS: $2"
}

new_repo
bash "$guard" tracked
bash "$guard" clean
echo 'PASS: clean repository'

for artifact in 'contracts/pool/test_snapshots/example.1.json' '.DS_Store' 'nested/.DS_Store' 'nested/__pycache__/cache' 'nested/module.pyc'; do
    new_repo
    mkdir -p "$(dirname "$artifact")"
    printf 'generated\n' > "$artifact"
    git add -f -- "$artifact"
    # A scratch commit proves the index guard rejects a PR introducing artifacts,
    # even when git status itself is clean.
    git -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm 'scratch: tracked artifact'
    bash "$guard" clean
    reject tracked "committed $artifact" 'git rm -r --cached'
done

new_repo
printf 'test changed source\n' >> source.txt
reject clean 'test modifying a tracked file' 'dirty working tree'
git add source.txt
reject clean 'staged modification' 'dirty working tree'

new_repo
printf 'new artifact\n' > untracked.txt
reject clean 'untracked output' 'dirty working tree'

new_repo
mkdir -p contracts/pool/test_snapshots
printf 'local snapshot\n' > contracts/pool/test_snapshots/local.json
bash "$guard" tracked
bash "$guard" clean
echo 'PASS: ignored local snapshots'

new_repo
printf 'legitimate\n' > test_snapshots.md
git add test_snapshots.md
git -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm 'legitimate source'
bash "$guard" tracked
bash "$guard" clean
echo 'PASS: similarly named source is allowed'
echo 'All repository hygiene tests passed.'
