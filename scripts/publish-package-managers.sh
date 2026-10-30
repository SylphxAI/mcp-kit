#!/usr/bin/env bash
# Called by the package-managers composite action in the source workflow job.
set -euo pipefail
if [ -n "$TAP" ] && [ -z "$TAP_TOKEN" ]; then echo '::error::Homebrew token is empty'; exit 1; fi
if [ -n "$BUCKET" ] && [ -z "$BUCKET_TOKEN" ]; then echo '::error::Scoop token is empty'; exit 1; fi
work=$(mktemp -d "${RUNNER_TEMP:?}/package-managers.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/dl"
gh release download "v$V" --repo "$GITHUB_REPOSITORY" --dir "$work/dl" \
  --pattern "$NAME-*.tar.gz" --pattern "$NAME-*.zip"
(cd "$work/dl" && sha256sum -- * > "$work/SHA256SUMS")
args=(--name "$NAME" --version "$V" --repo "$GITHUB_REPOSITORY" --sums "$work/SHA256SUMS"
  --description "$DESC" --homepage "$HOMEPAGE" --license "$LICENSE_ID" --version-arg "$VERSION_ARG")
if [ -n "$PACKAGE_JSON" ]; then args+=(--package-json "$GITHUB_WORKSPACE/$PACKAGE_JSON"); fi
if [ -n "$TAP" ]; then args+=(--formula-out "$work/tap/Formula/$NAME.rb"); fi
if [ -n "$BUCKET" ]; then args+=(--scoop-out "$work/bucket/bucket/$NAME.json"); fi
node "$(dirname "${BASH_SOURCE[0]}")/package-managers.mjs" "${args[@]}"
push_to() {
  local repo=$1 src=$2 file=$3
  local GH_TOKEN=$4
  export GH_TOKEN
  [ -n "$GH_TOKEN" ] || { echo '::error::destination token is empty'; exit 1; }
  # Authentication stays in memory; the remote URL contains no credential.
  git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
    clone -q --depth 1 "https://github.com/$repo.git" "$work/repos/$repo"
  mkdir -p "$work/repos/$repo/$(dirname "$file")"
  cp "$src/$file" "$work/repos/$repo/$file"
  (
    cd "$work/repos/$repo"
    git add "$file"
    if git diff --cached --quiet; then echo "= $repo $file already at $V"; exit 0; fi
    git -c user.name='sylphx-ai[bot]' -c user.email='sylphx-ai[bot]@users.noreply.github.com' \
      commit -q -m "$NAME $V"
    for _ in 1 2 3; do
      if git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push -q origin HEAD; then
        echo "+ $repo $file $V"; exit 0
      fi
      git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
        pull -q --rebase origin "$(git rev-parse --abbrev-ref HEAD)"
    done
    echo "::error::could not push $file to $repo"; exit 1
  )
}
if [ -n "$TAP" ]; then push_to "$TAP" "$work/tap" "Formula/$NAME.rb" "$TAP_TOKEN"; fi
if [ -n "$BUCKET" ]; then push_to "$BUCKET" "$work/bucket" "bucket/$NAME.json" "$BUCKET_TOKEN"; fi
