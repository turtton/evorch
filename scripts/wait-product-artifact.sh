#!/usr/bin/env bash
# Consumer side of the sealed product artifact (scripts/product-artifact.sh).
#
# Usage: scripts/wait-product-artifact.sh
#        scripts/wait-product-artifact.sh check-source <downloaded-artifact-dir>
#
# wait (default)
#   Blocks until the `product-artifact` job of THIS workflow run has completed
#   with conclusion success. Consumer jobs start next to that job instead of
#   after it (no `needs`), do the work that needs no product, then call this
#   before they download the artifact. Only a completed/success producer lets
#   the job go on: failure, cancellation, timeout, skip, a missing, duplicated
#   or contradictory job, a malformed or another run's response, and a listing
#   longer than one page all fail the job (fail closed).
#
#   A response whose total_count differs from the length of its array, with
#   total_count <= 100 and no Link rel="next", is not a second page: the two
#   come from separate reads and can disagree while jobs or artifacts are still
#   being created. Such a response decides nothing (no producer state, no
#   artifact verdict); it is polled again inside the budget below, and a budget
#   spent on it ends with a failure that names the count mismatch. The same
#   goes for the artifacts response. total_count > 100 or a next link stays a
#   hard failure.
#
#   Environment: GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_TOKEN (or GH_TOKEN),
#   GITHUB_RUN_ATTEMPT (recorded only, never branched on), GITHUB_API_URL
#   (default https://api.github.com), GITHUB_OUTPUT (optional).
#
#   One request per poll, always the same one:
#     GET /repos/{repository}/actions/runs/{run_id}/jobs?filter=latest&per_page=100
#   `filter=latest` is the latest attempt of each job, so a "re-run failed jobs"
#   attempt still sees the producer's earlier success. No per-attempt query and
#   no other run is consulted.
#
#   Once the producer has succeeded there is exactly one more request (plus
#   retries of it):
#     GET /repos/{repository}/actions/runs/{run_id}/artifacts?name=product-core&per_page=100
#   The run must hold exactly one artifact of that name and it must not have
#   expired; zero, several (a failed attempt's leftover next to a newer upload),
#   an expired one, another run's entry, a malformed or a paged response all
#   fail the job (fail closed). The producer's upload replaces an older
#   same-name artifact (overwrite: true); this check is what notices that it
#   did not. Which artifact is downloaded stays the download action's business:
#   nothing here selects by id or by attempt, and no id is ever reported.
#
#   Budget (shared by both requests): at most 24 requests (retries included) and
#   1800 seconds from the start. Request 1 is immediate, requests 2-14 are at least 45 s after the
#   previous one, request 15 on at least 120 s, each plus a random 0-5 s. Rate
#   limit responses (429, and 403 that carries Retry-After, an exhausted
#   X-RateLimit-Remaining or a rate-limit message) are retried within the same
#   budget, waiting for what the server said (Retry-After, X-RateLimit-Reset)
#   or backing off from 60 s up to 300 s; a reset after the deadline fails at
#   once. 5xx and transport failures are retried within the same budget; 401,
#   other 403 and 404 fail with the cause. The token only reaches curl through
#   a private config file, never argv or the log.
#
#   The result (producer job id, run_attempt, completed_at, requests, waited
#   seconds, last rate-limit remainder) is logged and written to GITHUB_OUTPUT.
#   It never exposes an artifact id.
#
# check-source <dir>
#   The downloaded manifest must name the commit this job checked out
#   (manifest.source_sha == GITHUB_SHA). This is a provenance check on top of,
#   not instead of, the content, mode and bundle verification of
#   `scripts/product-artifact.sh install`.
set -euo pipefail

producer_name=product-artifact
artifact_name=product-core
deadline_seconds=1800
max_requests=24
fast_requests=14   # requests 2..14 follow the short gap
fast_gap=45
slow_gap=120
backoff_start=60
backoff_max=300
connect_timeout=10
request_timeout=30
jitter_max=${WAIT_PRODUCT_JITTER_MAX:-5}

log() { echo "wait-product-artifact: $*"; }
die() {
  echo "::error::wait-product-artifact: $*"
  exit 1
}
now() { date +%s; }

cmd=${1:-wait}
case "$cmd" in
  check-source)
    dir=${2:?check-source needs <downloaded-artifact-dir>}
    manifest=$dir/product-manifest.json
    expected=${GITHUB_SHA:?GITHUB_SHA is required}
    [[ -f "$manifest" ]] || die "no manifest at $manifest"
    actual=$(jq -er '.source_sha | select(type == "string" and length > 0)' "$manifest") \
      || die "$manifest has no source_sha"
    [[ "$actual" == "$expected" ]] \
      || die "the product artifact was built from $actual, this job checked out $expected"
    log "product artifact source_sha matches GITHUB_SHA ($expected)"
    exit 0
    ;;
  wait) ;;
  *) die "usage: $0 [wait | check-source <dir>]" ;;
esac

command -v jq > /dev/null || die "jq is required"
command -v curl > /dev/null || die "curl is required"
repo=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}
run_id=${GITHUB_RUN_ID:?GITHUB_RUN_ID is required}
token=${GITHUB_TOKEN:-${GH_TOKEN:-}}
[[ -n "$token" ]] || die "GITHUB_TOKEN is required"
[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "unexpected GITHUB_REPOSITORY: $repo"
[[ "$run_id" =~ ^[0-9]+$ ]] || die "unexpected GITHUB_RUN_ID: $run_id"
# The token goes into a curl config file below: refuse anything that could break it.
[[ "$token" =~ ^[A-Za-z0-9_.-]+$ ]] || die "the token has an unexpected shape"
[[ "$jitter_max" =~ ^[0-9]+$ ]] || die "WAIT_PRODUCT_JITTER_MAX must be a number"
attempt=${GITHUB_RUN_ATTEMPT:-unknown}
api=${GITHUB_API_URL:-https://api.github.com}
jobs_url="$api/repos/$repo/actions/runs/$run_id/jobs?filter=latest&per_page=100"
artifacts_url="$api/repos/$repo/actions/runs/$run_id/artifacts?name=$artifact_name&per_page=100"
url=$jobs_url
phase='jobs' # jobs: waiting for the producer; artifacts: the one-off artifact check

tmp=$(umask 077 && mktemp -d)
child=''
cleanup() { rm -rf "$tmp"; }
on_signal() {
  trap - TERM INT
  if [[ -n "$child" ]]; then kill "$child" 2> /dev/null || true; fi
  cleanup
  exit "$1"
}
trap cleanup EXIT
trap 'on_signal 143' TERM
trap 'on_signal 130' INT
(umask 077 && printf 'header = "Authorization: Bearer %s"\n' "$token" > "$tmp/curl.cfg")

# Run a command in the background so that a signal reaches the trap at once
# instead of after the command has finished.
run_interruptible() {
  "$@" &
  child=$!
  local status=0
  wait "$child" || status=$?
  child=''
  return "$status"
}

start=$(now)
deadline=$((start + deadline_seconds))
requests=0
backoff=$backoff_start
next_wait=0
last_remaining=unknown
rate_limited=0
count_mismatch='' # set while the latest response's total_count and array length disagree

elapsed() { echo $(($(now) - start)); }

emit_outputs() { # key=value lines to GITHUB_OUTPUT
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then printf '%s\n' "$@" >> "$GITHUB_OUTPUT"; fi
}

fail_with() {
  log "giving up after $requests request(s), $(elapsed) s (consumer run_attempt=$attempt, rate_limited=$rate_limited, ratelimit_remaining=$last_remaining)"
  die "$1"
}

# A budget that ran out: when the latest response was a count mismatch, say so,
# so that a persistent mismatch is not mistaken for a producer that never finished.
budget_fail() { # <message>
  if [[ -n "$count_mismatch" ]]; then
    fail_with "$1 (the latest response was a count mismatch, never a consistent listing: $count_mismatch)"
  fi
  fail_with "$1"
}

header_value() { # <name> from the response header file; empty when absent
  awk -v want="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" '
    { sub(/\r$/, "") }
    index($0, ":") { name = tolower(substr($0, 1, index($0, ":") - 1)); value = substr($0, index($0, ":") + 1)
      sub(/^[[:space:]]+/, "", value); if (name == want) found = value }
    END { if (found != "") print found }' "$tmp/headers"
}

body_message() { jq -r 'if type == "object" and (.message | type) == "string" then .message else "" end' "$tmp/body" 2> /dev/null | head -c 300 || true; }

gap_before() { # <request number>
  local gap=$fast_gap
  if (($1 > fast_requests)); then gap=$slow_gap; fi
  local jitter=0
  if ((jitter_max > 0)); then jitter=$((RANDOM % (jitter_max + 1))); fi
  echo $((gap + jitter))
}

finish_waiting() { # the artifact check passed
  local waited
  waited=$(elapsed)
  log "the product artifact job succeeded: job_id=$job_id producer_run_attempt=$producer_attempt producer_completed_at=$completed_at consumer_run_attempt=$attempt requests=$requests waited=${waited}s rate_limited=$rate_limited ratelimit_remaining=$last_remaining"
  emit_outputs "product_job_id=$job_id" "product_run_attempt=$producer_attempt" \
    "product_completed_at=$completed_at" "requests=$requests" "waited_seconds=$waited" \
    "ratelimit_remaining=$last_remaining"
  exit 0
}

log "waiting for the '$producer_name' job of run $run_id (consumer run_attempt=$attempt, deadline ${deadline_seconds} s, at most $max_requests requests)"

while true; do
  if ((next_wait > 0)); then
    remaining=$((deadline - $(now)))
    ((remaining > 0)) || budget_fail "the product artifact job did not succeed within ${deadline_seconds} s"
    if ((next_wait > remaining)); then next_wait=$remaining; fi
    run_interruptible sleep "$next_wait"
  fi
  remaining=$((deadline - $(now)))
  timeout=$request_timeout
  if ((remaining < timeout)); then timeout=$remaining; fi
  ((timeout >= 1)) || budget_fail "the product artifact job did not succeed within ${deadline_seconds} s"

  requests=$((requests + 1))
  : > "$tmp/headers"
  : > "$tmp/body"
  : > "$tmp/code"
  curl_status=0
  run_interruptible curl --silent --show-error --retry 0 \
    --connect-timeout "$connect_timeout" --max-time "$timeout" \
    --config "$tmp/curl.cfg" \
    --header 'Accept: application/vnd.github+json' \
    --header 'X-GitHub-Api-Version: 2022-11-28' \
    --dump-header "$tmp/headers" --output "$tmp/body" --write-out '%{http_code}' \
    "$url" > "$tmp/code" 2> "$tmp/curl.err" || curl_status=$?
  code=$(tr -dc '0-9' < "$tmp/code")
  remaining_header=$(header_value x-ratelimit-remaining)
  if [[ -n "$remaining_header" ]]; then last_remaining=$remaining_header; fi

  # outcome: parse (HTTP 200) | ratelimit | retry (try again after the normal gap; wait_for is a longer server-asked wait)
  wait_for=0
  if ((curl_status != 0)) || [[ -z "$code" || "$code" == 000 ]]; then
    log "request $requests: transport failure (curl exit $curl_status): $(head -c 200 "$tmp/curl.err" | tr '\n' ' ')"
    outcome=retry
  else
    case "$code" in
      200) outcome=parse ;;
      429) outcome=ratelimit ;;
      403)
        message=$(body_message)
        if [[ -n "$(header_value retry-after)" || "$(header_value x-ratelimit-remaining)" == 0 ]] \
          || grep -qiE 'rate limit|abuse detection' <<< "$message"; then
          outcome=ratelimit
        else
          fail_with "HTTP 403 (permission denied; the job needs 'actions: read'): $message"
        fi
        ;;
      401) fail_with "HTTP 401 (the token was rejected): $(body_message)" ;;
      404) fail_with "HTTP 404 (run $run_id or the repository is not visible to this token): $(body_message)" ;;
      5??)
        log "request $requests: HTTP $code, retrying"
        outcome=retry
        ;;
      *) fail_with "unexpected HTTP $code: $(body_message)" ;;
    esac
  fi

  if [[ "$outcome" == ratelimit ]]; then
    rate_limited=$((rate_limited + 1))
    retry_after=$(header_value retry-after)
    reset=$(header_value x-ratelimit-reset)
    server_wait=0
    if [[ "$retry_after" =~ ^[0-9]+$ ]]; then
      server_wait=$retry_after
    elif [[ "$(header_value x-ratelimit-remaining)" == 0 && "$reset" =~ ^[0-9]+$ ]]; then
      server_wait=$((reset - $(now) + 1))
      if ((server_wait < 1)); then server_wait=1; fi
    else
      server_wait=$backoff
      backoff=$((backoff * 2))
      if ((backoff > backoff_max)); then backoff=$backoff_max; fi
    fi
    if (($(now) + server_wait > deadline)); then
      fail_with "rate limited (HTTP $code); the limit lifts in ${server_wait} s, after the ${deadline_seconds} s deadline"
    fi
    log "request $requests: rate limited (HTTP $code), the server asks for ${server_wait} s (remaining=$last_remaining)"
    wait_for=$server_wait
    outcome=retry
  fi

  if [[ "$outcome" == parse && "$phase" == artifacts ]]; then
    backoff=$backoff_start
    jq -e --arg run "$run_id" '
      type == "object" and (.total_count | type) == "number" and (.artifacts | type) == "array"
      and all(.artifacts[]; type == "object"
        and (.name | type) == "string" and (.expired | type) == "boolean"
        and (.id | type) == "number" and ((.workflow_run.id | tostring) == $run))' "$tmp/body" > /dev/null 2>&1 \
      || fail_with "the artifacts response is malformed or belongs to another run"
    total=$(jq -r '.total_count' "$tmp/body")
    listed=$(jq -r '.artifacts | length' "$tmp/body")
    if ((total > 100)) || [[ "$(header_value link)" == *'rel="next"'* ]]; then
      fail_with "the run has more artifacts ($total) than one page lists ($listed); this script supports one page"
    fi
    if ((listed != total)); then
      # Not a second page: total_count and the array come from separate reads
      # and can disagree for a moment. Decide nothing from this response, poll again.
      count_mismatch="$listed of $total artifacts listed"
      log "request $requests: count mismatch, $count_mismatch; not a second page, asking again"
      outcome=retry
    else
      count_mismatch=''
      named=$(jq -r --arg name "$artifact_name" '[.artifacts[] | select(.name == $name)] | length' "$tmp/body")
      live=$(jq -r --arg name "$artifact_name" '[.artifacts[] | select(.name == $name and .expired == false)] | length' "$tmp/body")
      log "request $requests: $named artifact(s) named '$artifact_name', $live not expired"
      ((named == 1)) || fail_with "run $run_id holds $named artifacts named '$artifact_name', expected exactly 1 (a leftover of an earlier attempt, or the producer's upload is missing)"
      ((live == 1)) || fail_with "the '$artifact_name' artifact of run $run_id has expired"
      finish_waiting
    fi
  fi

  if [[ "$outcome" == parse ]]; then
    backoff=$backoff_start
    jq -e --arg run "$run_id" '
      type == "object" and (.total_count | type) == "number" and (.jobs | type) == "array"
      and all(.jobs[]; type == "object"
        and (.name | type) == "string" and (.status | type) == "string"
        and (.id | type) == "number" and ((.run_id | tostring) == $run)
        and ((.conclusion | type) == "string" or .conclusion == null))' "$tmp/body" > /dev/null 2>&1 \
      || fail_with "the jobs response is malformed or belongs to another run"
    total=$(jq -r '.total_count' "$tmp/body")
    listed=$(jq -r '.jobs | length' "$tmp/body")
    if ((total > 100)) || [[ "$(header_value link)" == *'rel="next"'* ]]; then
      fail_with "the run has more jobs ($total) than one page lists ($listed); this script supports one page of at most 100 jobs"
    fi
    if ((listed != total)); then
      # Not a second page: total_count and the array come from separate reads
      # and can disagree for a moment (jobs still being created). The producer's
      # state is not decided from such a response; poll again.
      count_mismatch="$listed of $total jobs listed"
      log "request $requests: count mismatch, $count_mismatch; not a second page, asking again"
      outcome=retry
    else
      count_mismatch=''
    fi
  fi

  if [[ "$outcome" == parse ]]; then
    matches=$(jq -c --arg name "$producer_name" '[.jobs[] | select(.name == $name)]' "$tmp/body")
    case "$(jq -r 'length' <<< "$matches")" in
      0) log "request $requests: '$producer_name' is not listed yet" ;;
      1)
        status=$(jq -r '.[0].status' <<< "$matches")
        conclusion=$(jq -r '.[0].conclusion // "null"' <<< "$matches")
        job_id=$(jq -r '.[0].id' <<< "$matches")
        producer_attempt=$(jq -r '.[0].run_attempt // "unknown"' <<< "$matches")
        completed_at=$(jq -r '.[0].completed_at // "null"' <<< "$matches")
        log "request $requests: job $job_id status=$status conclusion=$conclusion run_attempt=$producer_attempt completed_at=$completed_at"
        case "$status" in
          completed)
            case "$conclusion" in
              success)
                # The producer is done: check its artifact next, at once (this
                # request is not a poll, so no gap), still inside the budget.
                phase=artifacts
                url=$artifacts_url
                next_wait=0
                ((requests < max_requests)) || fail_with "no request left within $max_requests requests to check the '$artifact_name' artifact"
                continue
                ;;
              null) fail_with "job $job_id is completed without a conclusion" ;;
              *) fail_with "the product artifact job finished with conclusion $conclusion (job $job_id)" ;;
            esac
            ;;
          queued | in_progress | waiting | requested | pending)
            [[ "$conclusion" == null ]] || fail_with "job $job_id is $status but has conclusion $conclusion"
            ;;
          *) fail_with "job $job_id has the unknown status $status" ;;
        esac
        ;;
      *) fail_with "more than one job is named '$producer_name'" ;;
    esac
    outcome=retry
  fi

  ((requests < max_requests)) || budget_fail "the product artifact job did not succeed within $max_requests requests"
  (($(now) < deadline)) || budget_fail "the product artifact job did not succeed within ${deadline_seconds} s"
  next_wait=$(gap_before $((requests + 1)))
  if ((wait_for > next_wait)); then next_wait=$wait_for; fi
done
