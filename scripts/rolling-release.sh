#!/usr/bin/env bash
# Points the rolling release TAG (v2) at $GITHUB_SHA, creating the tag and the release when missing.
# FLAG: --latest (GitHub's latest release) or --prerelease.
# Usage: rolling-release.sh TAG TITLE FLAG; needs GH_TOKEN and GH_REPO (the bot's token: Publish then uploads).
set -euo pipefail
cd "$(dirname "$0")/.."
tag=$1 title=$2 flag=$3

notes=$(mktemp)
cat > "$notes" <<NOTES
Built from \`${GITHUB_REF_NAME:-main}\` at ${GITHUB_SHA:0:7}, rebuilt on every change.

Image: \`ghcr.io/nikarh/lrclib-mirror:$tag\`. Verify the files with
\`SHA256SUMS.asc\` (see the README).
NOTES

if gh api "repos/$GH_REPO/git/ref/tags/$tag" > /dev/null 2>&1; then
  gh api -X PATCH "repos/$GH_REPO/git/refs/tags/$tag" -f sha="$GITHUB_SHA" -F force=true > /dev/null
else
  gh api "repos/$GH_REPO/git/refs" -f ref="refs/tags/$tag" -f sha="$GITHUB_SHA" > /dev/null
fi
if [[ $flag == --latest ]]; then
  args=(--prerelease=false --latest)
else
  args=(--prerelease --latest=false)
fi
if gh release view "$tag" > /dev/null 2>&1; then
  gh release edit "$tag" "${args[@]}" --title "$title" --notes-file "$notes"
else
  gh release create "$tag" --verify-tag "${args[@]}" --title "$title" --notes-file "$notes"
fi
