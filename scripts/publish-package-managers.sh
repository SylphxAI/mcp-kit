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
publish_to() {
  local repo=$1 src=$2 file=$3
  local GH_TOKEN=$4
  export GH_TOKEN
  [ -n "$GH_TOKEN" ] || { echo '::error::destination token is empty'; exit 1; }
  # Authentication stays in memory; the remote URL contains no credential.
  git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
    clone -q --depth 1 "https://github.com/$repo.git" "$work/repos/$repo"
  (
    cd "$work/repos/$repo"
    if [ -f "$file" ] && cmp -s "$src/$file" "$file"; then
      echo "= $repo $file already at $V"; exit 0
    fi
    base=$(git symbolic-ref --short refs/remotes/origin/HEAD)
    base=${base#origin/}
    branch="release/$NAME-$V"
    if git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
      ls-remote --exit-code --heads origin "$branch" > "$work/branch-ref"; then
      git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
        fetch -q --depth 1 origin "refs/heads/$branch"
      git checkout -q -b "$branch" FETCH_HEAD
    else
      status=$?
      [ "$status" = 2 ] || exit "$status"
      git checkout -q -b "$branch"
    fi
    mkdir -p "$(dirname "$file")"
    cp "$src/$file" "$file"
    git add "$file"
    if ! git diff --cached --quiet; then
      git -c user.name='sylphx-ai[bot]' -c user.email='sylphx-ai[bot]@users.noreply.github.com' \
        commit -q -m "$NAME $V"
    fi
    head=$(git rev-parse HEAD)
    # No default-branch writes or force pushes. A concurrent update fails and
    # the next run resumes from the actual remote branch and existing PR.
    git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
      push -q origin "HEAD:refs/heads/$branch"
    gh pr list --repo "$repo" --head "$branch" --base "$base" --state all \
      --json number,state > "$work/prs.json"
    count=$(jq 'length' "$work/prs.json")
    if [ "$count" = 0 ]; then
      printf 'Publish %s %s from https://github.com/%s/releases/tag/v%s.\n\nGenerated from the exact release archive digests.\n' \
        "$NAME" "$V" "$GITHUB_REPOSITORY" "$V" > "$work/pr-body.md"
      url=$(gh pr create --repo "$repo" --head "$branch" --base "$base" \
        --title "$NAME $V" --body-file "$work/pr-body.md")
      number=${url##*/}
      [[ "$number" =~ ^[0-9]+$ ]] || { echo '::error::invalid created PR response'; exit 1; }
    else
      [ "$count" = 1 ] && [ "$(jq -r '.[0].state' "$work/prs.json")" = OPEN ] || {
        echo "::error::publication branch has a closed, merged or ambiguous PR in $repo"; exit 1;
      }
      number=$(jq -r '.[0].number' "$work/prs.json")
    fi
    # GitHub applies the destination's checks and merge queue. Never use admin.
    if gh pr merge "$number" --repo "$repo" --auto --squash --match-head-commit "$head" 2> "$work/arm-error"; then
      cat "$work/arm-error" >&2
    else
      status=$?
      cat "$work/arm-error" >&2
      # A new PR can become CLEAN after gh selected enableAutoMerge from an
      # UNKNOWN snapshot. GitHub then refuses that mutation without arming it.
      # Only this explicit refusal permits one refreshed, normal merge request.
      grep -Fq 'is in clean status (enablePullRequestAutoMerge)' "$work/arm-error" || exit "$status"
      gh pr view "$number" --repo "$repo" --json state,headRefOid,mergeStateStatus > "$work/arm-pr.json"
      [ "$(jq -r '.headRefOid' "$work/arm-pr.json")" = "$head" ] || {
        echo "::error::publication PR head changed in $repo"; exit 1;
      }
      state=$(jq -r '.state' "$work/arm-pr.json")
      if [ "$state" != MERGED ]; then
        if [ "$state" != OPEN ] || [ "$(jq -r '.mergeStateStatus' "$work/arm-pr.json")" != CLEAN ]; then
          echo "::error::publication PR is not clean after auto-merge refusal in $repo"; exit 1
        fi
        # No --auto on a confirmed CLEAN PR: use the ordinary merge/queue
        # operation. GitHub still enforces all current destination requirements.
        gh pr merge "$number" --repo "$repo" --squash --match-head-commit "$head"
      fi
    fi
    wait_seconds=${PACKAGE_MANAGERS_WAIT_SECONDS:-900}
    poll_seconds=${PACKAGE_MANAGERS_POLL_SECONDS:-10}
    [[ "$wait_seconds" =~ ^[0-9]+$ && "$poll_seconds" =~ ^[0-9]+$ ]] || exit 1
    deadline=$((SECONDS + wait_seconds))
    while :; do
      gh pr view "$number" --repo "$repo" --json state,headRefOid,mergeCommit > "$work/pr.json"
      [ "$(jq -r '.headRefOid' "$work/pr.json")" = "$head" ] || {
        echo "::error::publication PR head changed in $repo"; exit 1;
      }
      state=$(jq -r '.state' "$work/pr.json")
      case "$state" in
        MERGED) break ;;
        OPEN) ;;
        *) echo "::error::publication PR is $state in $repo"; exit 1 ;;
      esac
      [ "$SECONDS" -lt "$deadline" ] || { echo "::error::publication PR did not merge before deadline in $repo"; exit 1; }
      sleep "$poll_seconds"
    done
    merge=$(jq -er '.mergeCommit.oid | select(type == "string" and length > 0)' "$work/pr.json")
    gh api "repos/$repo/contents/$file?ref=$merge" > "$work/merged.json"
    jq -er 'select(.encoding == "base64") | .content' "$work/merged.json" | base64 --decode > "$work/merged-file"
    cmp "$src/$file" "$work/merged-file" || { echo "::error::merged manifest differs in $repo"; exit 1; }
    echo "+ $repo $file $V merged as $merge"
  )
}
if [ -n "$TAP" ]; then publish_to "$TAP" "$work/tap" "Formula/$NAME.rb" "$TAP_TOKEN"; fi
if [ -n "$BUCKET" ]; then publish_to "$BUCKET" "$work/bucket" "bucket/$NAME.json" "$BUCKET_TOKEN"; fi
