#!/usr/bin/env bash
# Upload six fixed GUI screenshots from a completed PR CI run. This script is
# executed from the default branch by gui-pr-evidence.yml, never from PR code.
set -Eeuo pipefail

fail() { printf 'GUI PR evidence: %s\n' "$*" >&2; exit 1; }
[[ $# == 5 ]] || fail 'Usage: post-gui-pr-evidence.sh REPO RUN_ID PREVIEW_DIR HEAD_REPO HEAD_BRANCH'
repo=$1 run_id=$2 preview_dir=$3 head_repo=$4 head_branch=$5
[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail 'Invalid repository name.'
[[ "$head_repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail 'Invalid run head repository.'
[[ "$run_id" =~ ^[1-9][0-9]*$ ]] || fail 'Invalid run ID.'
[[ -n "$head_branch" ]] || fail 'Missing run head branch.'
[[ -d "$preview_dir" && ! -L "$preview_dir" ]] || fail 'Missing preview directory.'
[[ -f "$preview_dir/pr.json" && ! -L "$preview_dir/pr.json" ]] || fail 'Missing PR metadata.'
case ${GH_TOKEN:-} in
    ghp_*|gho_*|github_pat_*) ;;
    *) fail 'GUI_EVIDENCE_TOKEN must be an OAuth token or PAT with repository write access; GITHUB_TOKEN cannot upload attachments.' ;;
esac
gh pr comment --help | grep -- '--attach' >/dev/null || fail 'GitHub CLI does not support --attach.'

pr=$(jq -er '.pr | select(type == "number") | tostring' "$preview_dir/pr.json") \
    || fail 'PR metadata has no PR number.'
head_sha=$(jq -er '.head_sha | strings' "$preview_dir/pr.json") \
    || fail 'PR metadata has no head SHA.'
metadata_run=$(jq -er '.run_id | select(type == "number") | tostring' "$preview_dir/pr.json") \
    || fail 'PR metadata has no run ID.'
[[ "$pr" =~ ^[1-9][0-9]*$ && "$head_sha" =~ ^[0-9a-f]{40}$ ]] \
    || fail 'PR metadata contains an invalid number or SHA.'
[[ "$metadata_run" == "$run_id" ]] || fail 'Preview belongs to a different CI run.'

# workflow_run.pull_requests may be empty even for a PR. When GitHub provides
# it, require agreement; always verify the current PR head and repository too.
if [[ -f ${GITHUB_EVENT_PATH:-} ]] \
    && jq -e '.workflow_run.pull_requests | length > 0' "$GITHUB_EVENT_PATH" >/dev/null; then
    jq -e --argjson pr "$pr" '.workflow_run.pull_requests | any(.number == $pr)' \
        "$GITHUB_EVENT_PATH" >/dev/null || fail 'CI run is not associated with this PR.'
fi
pr_json=$(gh api "repos/$repo/pulls/$pr") || fail 'Cannot read the target PR.'
if ! jq -e --arg repo "$repo" --arg sha "$head_sha" --arg source "$head_repo" \
    --arg branch "$head_branch" \
    '.state == "open" and .base.repo.full_name == $repo and .head.sha == $sha and .head.repo.full_name == $source and .head.ref == $branch' \
    <<<"$pr_json" >/dev/null; then
    printf 'GUI PR evidence: skip closed or updated PR #%s.\n' "$pr"
    exit 0
fi

names=(01-workbench.png 02-conversation.png 03-tasks.png 04-provider-settings.png
    05-approvals.png 06-high-contrast.png)
for name in "${names[@]}"; do
    path=$preview_dir/$name
    [[ -f "$path" && ! -L "$path" ]] || fail "Missing regular screenshot: $name"
    size=$(stat -c %s -- "$path")
    (( size > 8 && size <= 10000000 )) || fail "Screenshot has an invalid size: $name"
    signature=$(od -An -tx1 -N8 -- "$path" | tr -d '[:space:]')
    [[ "$signature" == 89504e470d0a1a0a ]] || fail "Screenshot is not PNG: $name"
done

body=$(mktemp)
trap 'rm -f -- "$body"' EXIT
cat > "$body" <<EOF
<!-- evorch-gui-qa-run:$run_id -->
### GUI QA screenshots

Six representative screens from [CI run $run_id](https://github.com/$repo/actions/runs/$run_id). The run's **gui-evidence** artifact contains the full screen matrix and native input evidence.

<details><summary>Show screenshots</summary>

**Workbench**

![Workbench](./01-workbench.png)

**Conversation**

![Conversation](./02-conversation.png)

**Tasks**

![Tasks](./03-tasks.png)

**Provider settings**

![Provider settings](./04-provider-settings.png)

**Pending approvals**

![Pending approvals](./05-approvals.png)

**High contrast**

![High contrast](./06-high-contrast.png)

</details>
EOF
cd -- "$preview_dir"
gh pr comment "$pr" -R "$repo" --body-file "$body" \
    --attach ./01-workbench.png --attach ./02-conversation.png \
    --attach ./03-tasks.png --attach ./04-provider-settings.png \
    --attach ./05-approvals.png --attach ./06-high-contrast.png
