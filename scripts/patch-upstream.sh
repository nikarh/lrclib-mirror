#!/usr/bin/env bash
# Applies patches/<project>/*.patch to upstream/<project> (the submodules). Patches already applied are skipped, so
# running it again is harmless. `--check` only reports whether every patch applies, and changes nothing.
# Works without git metadata (a Docker build context): `git apply` is used as a plain patch tool there.
set -euo pipefail
cd "$(dirname "$0")/.."

check=false
[[ ${1:-} == --check ]] && check=true

failed=0
for dir in patches/*/; do
  project=$(basename "$dir")
  target=upstream/$project
  for patch in "$dir"*.patch; do
    [[ -e $patch ]] || continue
    # Relative to the target directory; outside a repository git apply needs --directory instead of -C.
    if git -C "$target" apply --reverse --check "$PWD/$patch" 2>/dev/null; then
      echo "$patch: already applied"
    elif git -C "$target" apply --check "$PWD/$patch" 2>/dev/null; then
      if $check; then
        echo "$patch: applies"
      else
        git -C "$target" apply "$PWD/$patch"
        echo "$patch: applied"
      fi
    else
      echo "$patch: does not apply to $target" >&2
      failed=1
    fi
  done
done
exit $failed
