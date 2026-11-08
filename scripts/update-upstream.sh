#!/usr/bin/env bash
# Moves the web search submodule (lrclib-homepage) to the newest commit of its default branch and updates Cargo.lock,
# and writes the pull request text to $1 (title on the first line, then the body): the upstream commits since the
# current one, changelog additions when upstream keeps a changelog, and whether the patches still apply. Exits 3 when
# nothing changed. Leaves the changes uncommitted.
set -euo pipefail
cd "$(dirname "$0")/.."
out=$1

declare -A old new
for name in lrclib-homepage; do
  old[$name]=$(git ls-tree HEAD "upstream/$name" | awk '{print $3}')
done
git submodule update --init --remote --checkout upstream/lrclib-homepage
changed=()
for name in lrclib-homepage; do
  new[$name]=$(git -C "upstream/$name" rev-parse HEAD)
  [[ ${old[$name]} == "${new[$name]}" ]] || changed+=("$name")
done

# What cargo changed, e.g. "Updating tokio v1.54.1 -> v1.55.0".
if ! output=$(cargo update 2>&1); then
  echo "$output" >&2
  exit 1
fi
lock_report=$(sed -nE 's/^ *((Updating|Adding|Removing|Downgrading) [^ ]+ v.*)/\1/p' <<< "$output")
lock_changed=false
git diff --quiet -- Cargo.lock || lock_changed=true
if (( ${#changed[@]} == 0 )) && ! $lock_changed; then
  echo "nothing changed"
  exit 3
fi

summary=()
for name in "${changed[@]}"; do summary+=("$name ${new[$name]:0:7}"); done
$lock_changed && summary+=("Cargo.lock")
{
  joined=$(printf '%s, ' "${summary[@]}")
  echo "build(deps): update upstream (${joined%, })"
  echo "Upstream changes since the last update. Merging publishes them as \`v2\` and the \`v2\` image;"
  echo "until then this branch is built as the \`v2-dev\` pre-release and the \`v2-dev\` image."
  for name in "${changed[@]}"; do
    repo=https://github.com/tranxuanthang/$name
    range="${old[$name]}..${new[$name]}"
    echo
    echo "## [$name]($repo/compare/${old[$name]:0:12}...${new[$name]:0:12})"
    echo
    git -C "upstream/$name" log --reverse --format="- [\`%h\`]($repo/commit/%H) %s" "$range"
    changelog=$(git -C "upstream/$name" diff --name-only "$range" -- 'CHANGELOG*' 'changelog*' 'CHANGES*' | head -1)
    if [[ -n $changelog ]]; then
      echo
      echo "<details><summary>$changelog</summary>"
      echo
      git -C "upstream/$name" diff "$range" -- "$changelog" | sed -n 's/^+\([^+]\)/\1/p; s/^+$//p'
      echo
      echo "</details>"
    fi
  done
  if $lock_changed; then
    echo
    echo "## Cargo.lock"
    echo
    echo '```'
    echo "$lock_report"
    echo '```'
  fi
  echo
  echo "## Patches"
  echo
  if report=$(scripts/patch-upstream.sh --check 2>&1); then
    echo "All of \`patches/\` still apply."
  else
    echo "**Some patches no longer apply**: CI fails until they are updated."
  fi
  echo
  echo '```'
  echo "$report"
  echo '```'
} > "$out"
