#!/usr/bin/env bash
# Refreshes the lockfiles within the manifests' ranges (Cargo.lock: `cargo update`; web/package-lock.json:
# `npm update`) and writes the pull request text to $1 (title on the first line, then the body): what changed in
# each. Exits 3 when nothing changed. Leaves the changes uncommitted.
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/.."
out=$1

# What cargo changed, e.g. "Updating tokio v1.54.1 -> v1.55.0".
if ! output=$(cargo update 2>&1); then
  echo "$output" >&2
  exit 1
fi
cargo_report=$(sed -nE 's/^ *((Updating|Adding|Removing|Downgrading) [^ ]+ v.*)/\1/p' <<< "$output")

# What npm changed: the packages of the lockfile whose version is another (or that came or went).
versions() { jq -r '.packages | to_entries[] | select(.key != "") | "\(.key | sub("^node_modules/"; "")) \(.value.version)"' web/package-lock.json | sort; }
before=$(versions)
if ! output=$(npm --prefix web update --no-audit --no-fund 2>&1); then
  echo "$output" >&2
  exit 1
fi
npm_report=$(join -a1 -a2 -e - -o 0,1.2,2.2 <(echo "$before") <(versions) |
  awk '$2 != $3 { print ($2 == "-" ? "Adding " $1 " " $3 : $3 == "-" ? "Removing " $1 " " $2 : "Updating " $1 " " $2 " -> " $3) }')

changed=()
git diff --quiet -- Cargo.lock || changed+=(Cargo.lock)
git diff --quiet -- web/package-lock.json || changed+=(web/package-lock.json)
if (( ${#changed[@]} == 0 )); then
  echo "nothing changed"
  exit 3
fi

{
  joined=$(printf '%s, ' "${changed[@]}")
  echo "build(deps): update dependencies (${joined%, })"
  echo "Dependency updates within the manifests' ranges. Merging publishes them as \`v2\` and the \`v2\` (\`main\`)"
  echo "image; until then this branch is built as the \`v2-dev\` pre-release and the \`v2-dev\` (\`dev\`) image."
  for file in "${changed[@]}"; do
    echo
    echo "## $file"
    echo
    echo '```'
    if [[ $file == Cargo.lock ]]; then
      echo "$cargo_report"
    else
      echo "$npm_report"
    fi
    echo '```'
  done
} > "$out"
