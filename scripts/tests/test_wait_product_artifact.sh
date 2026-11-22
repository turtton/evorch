#!/usr/bin/env bash
# Regression tests for scripts/wait-product-artifact.sh.
#
# The script runs unmodified. A mock `curl` replays the scripted responses of one
# case, and mock `sleep` and `date` keep a virtual clock (sleep advances it), so
# nothing here waits and no network is used. Event-driven only: the cancellation
# case synchronizes with the mock through a FIFO.
# shellcheck disable=SC2016 # some strings hold literal $ on purpose
set -euo pipefail
cd "$(dirname "$0")/../.."
script=$PWD/scripts/wait-product-artifact.sh
real_date=$(command -v date)
real_sleep=$(command -v sleep)
work=$(mktemp -d "${TMPDIR:-/tmp}/test-wait-product-artifact.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
mkdir "$work/bin"

cat > "$work/bin/curl" << 'EOF'
#!/usr/bin/env bash
set -u
d=$MOCK_DIR
n=$(($(cat "$d/count" 2> /dev/null || echo 0) + 1))
echo "$n" > "$d/count"
printf '%s\n' "$*" >> "$d/argv.log"
body='' hdr='' cfg='' url=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output) body=$2; shift 2 ;;
    --dump-header) hdr=$2; shift 2 ;;
    --config) cfg=$2; shift 2 ;;
    --connect-timeout | --max-time | --header | --write-out) shift 2 ;;
    --*) shift ;;
    *) url=$1; shift ;;
  esac
done
echo "$url" >> "$d/urls.log"
[[ -z "$cfg" ]] || cat "$cfg" >> "$d/auth.log"
resp=resp
if [[ "$url" == *"/artifacts?"* ]]; then
  # The artifact check has its own scripted responses (default: exactly one live artifact).
  resp=resp_art
  an=$(($(cat "$d/art_count" 2> /dev/null || echo 0) + 1))
  echo "$an" > "$d/art_count"
  n=$an
  if [[ ! -d "$d/resp_art/$n" && ! -d "$d/resp_art/last" ]]; then
    printf '%s' '{"total_count":1,"artifacts":[{"id":9001,"name":"product-core","expired":false,"workflow_run":{"id":4242}}]}' > "$body"
    printf 200
    exit 0
  fi
fi
if [[ -p "$d/fifo" ]]; then
  # Cancellation case: tell the test we are in flight, then stay alive holding the
  # write end of the FIFO until the script under test kills us.
  exec 3> "$d/fifo"
  echo started >&3
  exec "$REAL_SLEEP" 600
fi
r=$d/$resp/$n
[[ -d "$r" ]] || r=$d/$resp/last
if [[ -f "$r/exit" ]]; then
  echo "curl: (28) Operation timed out" >&2
  exit "$(cat "$r/exit")"
fi
cat "$r/body" > "$body"
if [[ -f "$r/headers" ]]; then cat "$r/headers" > "$hdr"; fi
printf '%s' "$(cat "$r/status")"
EOF
cat > "$work/bin/sleep" << 'EOF'
#!/usr/bin/env bash
echo "$1" >> "$MOCK_DIR/sleeps.log"
echo $(($(cat "$MOCK_DIR/clock") + $1)) > "$MOCK_DIR/clock"
EOF
cat > "$work/bin/date" << 'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == +%s ]]; then cat "$MOCK_DIR/clock"; else exec "$REAL_DATE" "$@"; fi
EOF
chmod +x "$work/bin/curl" "$work/bin/sleep" "$work/bin/date"

token=ghs_SECRET.token-value
epoch=1700000000
fails=0
cases=0
eq() { # <description> <got> <wanted>
  if [[ "$2" != "$3" ]]; then
    echo "FAIL: $1 (wanted '$3', got '$2')" >&2
    fails=$((fails + 1))
  fi
}
check() { # <description> <command...>: the command must succeed
  local what=$1
  shift
  if ! "$@" > /dev/null 2>&1; then
    echo "FAIL: $what" >&2
    fails=$((fails + 1))
  fi
}
absent() { ! grep -q -- "$1" "$2"; }

d=''
new_case() {
  cases=$((cases + 1))
  d=$work/case.$cases
  mkdir -p "$d/resp"
  echo "$epoch" > "$d/clock"
  : > "$d/sleeps.log"
  : > "$d/gh_output"
  rc=0
}
# respond <n|last> <status> <body-json-or-text> [header line...]
respond() {
  local n=$1 status=$2 body=$3
  shift 3
  mkdir -p "$d/resp/$n"
  echo "$status" > "$d/resp/$n/status"
  printf '%s' "$body" > "$d/resp/$n/body"
  if (($# > 0)); then printf '%s\r\n' "$@" > "$d/resp/$n/headers"; fi
}
# respond_art <n|last> <status> <body> [header line...]: the artifact check's scripted responses
respond_art() {
  local n=$1 status=$2 body=$3
  shift 3
  mkdir -p "$d/resp_art/$n"
  echo "$status" > "$d/resp_art/$n/status"
  printf '%s' "$body" > "$d/resp_art/$n/body"
  if (($# > 0)); then printf '%s\r\n' "$@" > "$d/resp_art/$n/headers"; fi
}
# art_entry <name> <expired> <run_id>; art_list <entry...>
art_entry() { jq -nc --arg n "${1:-product-core}" --argjson e "${2:-false}" --argjson run "${3:-4242}" '{id: 9001, name: $n, expired: $e, workflow_run: {id: $run}}'; }
art_list() { printf '%s\n' "$@" | jq -sc '{total_count: length, artifacts: .}'; }
transport_failure() { mkdir -p "$d/resp/$1"; echo "$2" > "$d/resp/$1/exit"; }
# jobs <status> <conclusion|null> [run_id]: a listing with the producer among
# decoys whose names merely contain the producer's.
jobs() {
  jq -nc --arg st "$1" --arg c "$2" --argjson run "${3:-4242}" '
    def job($id; $name; $st; $c): {id: $id, run_id: $run, name: $name, status: $st,
      conclusion: (if $c == "null" then null else $c end), run_attempt: 1};
    {total_count: 4, jobs: [
      job(1; "scope-gate"; "completed"; "success"),
      job(2; "product-artifact-lint"; "completed"; "failure"),
      (job(77; "product-artifact"; $st; $c) + {run_attempt: 2,
        completed_at: (if $st == "completed" then "2026-05-01T10:20:30Z" else null end)}),
      job(3; "tests (runtime 1/4)"; "in_progress"; "null")]}'
}
run_wait() { # [VAR=value...]: runs the script in the current case
  rc=0
  env GITHUB_REPOSITORY=owner/repo GITHUB_RUN_ID=4242 GITHUB_RUN_ATTEMPT=2 GITHUB_TOKEN="$token" \
    GITHUB_OUTPUT="$d/gh_output" WAIT_PRODUCT_JITTER_MAX=0 GITHUB_SHA=deadbeef \
    PATH="$work/bin:$PATH" MOCK_DIR="$d" REAL_DATE="$real_date" REAL_SLEEP="$real_sleep" \
    "$@" "$script" > "$d/out" 2>&1 || rc=$?
}
requests() { cat "$d/count" 2> /dev/null || echo 0; }
sleeps() { tr '\n' ' ' < "$d/sleeps.log" | sed 's/ $//'; }
repeat() { # <n> <word>
  local out='' i
  for ((i = 0; i < $1; i++)); do out+="$2 "; done
  printf '%s' "${out% }"
}
failed() { eq "$1: fails" "$([[ $rc -ne 0 ]] && echo yes || echo no)" yes; }

# --- success, and what it reports -------------------------------------------
new_case
respond 1 200 "$(jobs completed success)" 'X-RateLimit-Remaining: 4321'
run_wait
eq "immediate success: exit" "$rc" 0
eq "immediate success: the jobs poll and the artifact check" "$(requests)" 2
eq "immediate success: no wait" "$(sleeps)" ''
eq "the requests are the latest-attempt jobs listing, then the product-core artifacts, of this run" "$(cat "$d/urls.log")" \
  'https://api.github.com/repos/owner/repo/actions/runs/4242/jobs?filter=latest&per_page=100
https://api.github.com/repos/owner/repo/actions/runs/4242/artifacts?name=product-core&per_page=100'
check "producer job id, its run_attempt and completed_at are reported" \
  grep -q 'job_id=77 producer_run_attempt=2 producer_completed_at=2026-05-01T10:20:30Z consumer_run_attempt=2 requests=2' "$d/out"
check "outputs are written for the workflow" grep -qx 'product_job_id=77' "$d/gh_output"
check "producer attempt is an output" grep -qx 'product_run_attempt=2' "$d/gh_output"
check "completion time is an output" grep -qx 'product_completed_at=2026-05-01T10:20:30Z' "$d/gh_output"
check "request count is an output" grep -qx 'requests=2' "$d/gh_output"
check "the rate-limit remainder is an output" grep -qx 'ratelimit_remaining=4321' "$d/gh_output"
check "no artifact id is reported in the outputs" absent -i 'artifact_id' "$d/gh_output"
check "no artifact id is reported in the log" absent 9001 "$d/out"
check "the token reaches curl through the config file" grep -q "Authorization: Bearer $token" "$d/auth.log"
check "the token is not in curl's arguments" absent "$token" "$d/argv.log"
check "the token is not in the log" absent "$token" "$d/out"
check "the token is not in the outputs" absent "$token" "$d/gh_output"

# A re-run of failed jobs: this attempt (3) sees the producer's earlier success.
new_case
respond 1 200 "$(jobs completed success)"
run_wait GITHUB_RUN_ATTEMPT=3
eq "partial re-run: exit" "$rc" 0
check "partial re-run: both attempts are logged" grep -q 'producer_run_attempt=2 .* consumer_run_attempt=3' "$d/out"

# --- waiting -----------------------------------------------------------------
new_case
respond 1 200 "$(jobs queued null)"
respond 2 200 "$(jobs in_progress null)"
respond 3 200 "$(jobs completed success)"
run_wait
eq "queued, running, done: exit" "$rc" 0
eq "queued, running, done: requests" "$(requests)" 4
eq "queued, running, done: 45 s between requests" "$(sleeps)" '45 45'

new_case
respond 1 200 '{"total_count":1,"jobs":[{"id":1,"run_id":4242,"name":"scope-gate","status":"completed","conclusion":"success"}]}'
respond 2 200 "$(jobs completed success)"
run_wait
eq "producer not listed yet is waited for: exit" "$rc" 0
eq "producer not listed yet: requests" "$(requests)" 3

new_case
respond last 200 '{"total_count":0,"jobs":[]}'
run_wait
failed "a producer that never appears"
eq "a producer that never appears uses the whole request budget" "$(requests)" 24

new_case
respond 1 200 "$(jobs queued null)" # jitter stays within 0-5 s
respond 2 200 "$(jobs queued null)"
respond 3 200 "$(jobs queued null)"
respond 4 200 "$(jobs completed success)"
run_wait WAIT_PRODUCT_JITTER_MAX=5
eq "jittered wait: exit" "$rc" 0
eq "jittered wait: every gap is 45-50 s" \
  "$(awk '$1 >= 45 && $1 <= 50 { ok++ } END { print ok }' "$d/sleeps.log")" 3

# --- the producer did not succeed --------------------------------------------
for conclusion in failure cancelled timed_out skipped neutral action_required stale; do
  new_case
  respond last 200 "$(jobs completed "$conclusion")"
  run_wait
  failed "producer $conclusion"
  eq "producer $conclusion: not retried" "$(requests)" 1
  check "producer $conclusion: the cause is reported" grep -q "conclusion $conclusion" "$d/out"
done

# --- contradictory or unusable responses fail closed -------------------------
new_case
respond last 200 "$(jobs completed null)"
run_wait
failed "completed without a conclusion"
new_case
respond last 200 "$(jobs queued success)"
run_wait
failed "running with a conclusion"
new_case
respond last 200 "$(jobs frobnicating null)"
run_wait
failed "unknown status"
new_case
respond last 200 "$(jobs completed success 9999)"
run_wait
failed "another run's jobs"
eq "another run's jobs: not retried" "$(requests)" 1
new_case
respond last 200 '<html>not json</html>'
run_wait
failed "not JSON"
new_case
respond last 200 '{"total_count":1}'
run_wait
failed "no jobs array"
new_case
respond last 200 '{"total_count":1,"jobs":[{"id":77,"run_id":4242,"status":"completed","conclusion":"success"}]}'
run_wait
failed "a job without a name"
new_case
dup=$(jobs completed success | jq -c '.total_count = 5 | .jobs += [.jobs[2]]')
respond last 200 "$dup"
run_wait
failed "two jobs named like the producer"
new_case
respond last 200 "$(jobs completed success | jq -c '.total_count = 101')"
run_wait
failed "more jobs than one page"
new_case
respond last 200 "$(jobs completed success)" 'Link: <https://api.github.com/x?page=2>; rel="next"'
run_wait
failed "a next page exists"
new_case
respond last 200 "$(jobs completed success | jq -c '.total_count = 7')" 'Link: <https://api.github.com/x?page=2>; rel="next"'
run_wait
failed "a next page exists next to a count mismatch"
eq "a next page exists next to a count mismatch: not retried" "$(requests)" 1

# --- total_count and the array disagree without a second page ----------------
# The two come from separate reads (run 45010801038: 22 against 21 listed, then
# 24 and 24). Such a response is polled again and decides nothing by itself.
new_case
respond 1 200 "$(jobs completed success | jq -c '.total_count = 22')"
respond 2 200 "$(jobs completed success)"
run_wait
eq "a transient count mismatch: exit" "$rc" 0
eq "a transient count mismatch: asked again after the normal gap" "$(sleeps)" 45
eq "a transient count mismatch: two jobs polls and the artifact check" "$(requests)" 3
check "a transient count mismatch is logged" grep -q 'count mismatch, 4 of 22 jobs listed' "$d/out"
new_case
respond 1 200 "$(jobs completed success | jq -c '.total_count = 3')"
respond 2 200 "$(jobs completed success)"
run_wait
eq "more jobs listed than total_count: exit" "$rc" 0
eq "more jobs listed than total_count: asked again" "$(requests)" 3
# A mismatching listing is no evidence about the producer, whichever state it shows.
new_case
respond 1 200 "$(jobs completed failure | jq -c '.total_count = 22')"
respond 2 200 "$(jobs completed success)"
run_wait
eq "a failed producer in a mismatching listing is not believed: exit" "$rc" 0
new_case
respond last 200 "$(jobs completed success | jq -c '.total_count = 7')"
run_wait
failed "a count mismatch that never settles"
eq "a count mismatch that never settles: uses the request budget" "$(requests)" 24
check "a count mismatch that never settles: says it was a count mismatch" grep -q 'count mismatch, never a consistent listing: 4 of 7 jobs listed' "$d/out"
check "a mismatching listing never starts the artifact check" absent '/artifacts?' "$d/urls.log"
new_case
respond 1 200 "$(jobs completed success | jq -c '.total_count = 7')"
respond 2 200 "$(jobs queued null)"
respond last 200 "$(jobs queued null)"
run_wait
failed "a producer that never finishes after a count mismatch"
check "a consistent listing clears the count mismatch from the final message" absent 'count mismatch, never' "$d/out"

# --- permanent HTTP errors ---------------------------------------------------
for status in 401 404 403 418 302; do
  new_case
  respond last "$status" '{"message":"Resource not accessible by integration"}'
  run_wait
  failed "HTTP $status"
  eq "HTTP $status: not retried" "$(requests)" 1
done
check "a plain 403 says why" grep -q 'Resource not accessible by integration' "$d/out"

# --- rate limits are retried within the budget -------------------------------
new_case
respond 1 403 '{"message":"API rate limit exceeded"}' 'Retry-After: 7'
respond 2 200 "$(jobs completed success)"
run_wait
eq "403 with a short Retry-After: exit" "$rc" 0
eq "403 with a short Retry-After: the normal gap still applies" "$(sleeps)" 45

new_case
respond 1 429 '{}' 'Retry-After: 200'
respond 2 200 "$(jobs completed success)"
run_wait
eq "429 with a long Retry-After: exit" "$rc" 0
eq "429 with a long Retry-After: waits what the server asked" "$(sleeps)" 200

new_case
respond 1 403 '{"message":"API rate limit exceeded"}' 'X-RateLimit-Remaining: 0' "X-RateLimit-Reset: $((epoch + 90))"
respond 2 200 "$(jobs completed success)"
run_wait
eq "exhausted limit: exit" "$rc" 0
eq "exhausted limit: waits for the reset" "$(sleeps)" 91

new_case
respond 1 429 '{}'
respond 2 429 '{}'
respond 3 429 '{}'
respond 4 429 '{}'
respond 5 200 "$(jobs completed success)"
run_wait
eq "429 without headers: exit" "$rc" 0
eq "429 without headers backs off 60 s, doubling, up to 300 s" "$(sleeps)" '60 120 240 300'

new_case
respond 1 403 '{"message":"You have exceeded a secondary rate limit."}'
respond 2 200 "$(jobs completed success)"
run_wait
eq "secondary limit message: exit" "$rc" 0
eq "secondary limit message: retried with the backoff" "$(sleeps)" 60

new_case
respond 1 403 '{"message":"API rate limit exceeded"}' 'X-RateLimit-Remaining: 0' "X-RateLimit-Reset: $((epoch + 4000))"
run_wait
failed "reset after the deadline"
eq "reset after the deadline: gives up at once" "$(requests)" 1
check "reset after the deadline: says so" grep -q 'after the 1800 s deadline' "$d/out"

new_case
respond last 429 '{}' 'Retry-After: 800'
run_wait
failed "repeated long rate limits"
eq "repeated long rate limits: stop when the third wait would pass the deadline" "$(requests)" 3

new_case
respond 1 429 '{}' 'Retry-After: 1000'
respond 2 429 '{}' 'Retry-After: 700'
respond last 200 "$(jobs queued null)"
run_wait
failed "deadline reached while queued"
eq "the last wait is cut at the deadline" "$(sleeps)" '1000 700 45 45 10'
eq "a final request is made at the deadline" "$(requests)" 5

# --- transient failures ------------------------------------------------------
new_case
transport_failure 1 28
respond 2 502 '{}'
respond 3 200 "$(jobs completed success)"
run_wait
eq "transport failure and 5xx: exit" "$rc" 0
eq "transport failure and 5xx are retried on the normal gap" "$(sleeps)" '45 45'
eq "transport failure and 5xx count as requests" "$(requests)" 4

new_case
transport_failure last 28
run_wait
failed "permanent transport failure"
eq "permanent transport failure: budget" "$(requests)" 24

# --- request budget and schedule ---------------------------------------------
new_case
respond last 200 "$(jobs queued null)"
run_wait
failed "a producer that never finishes"
eq "at most 24 requests" "$(requests)" 24
eq "requests 2-14 follow after 45 s, request 15 on after 120 s" \
  "$(sleeps)" "$(repeat 13 45) $(repeat 10 120)"
check "running out of requests is reported" grep -q '24 requests' "$d/out"

# --- the artifact check after the producer's success -------------------------
new_case
respond 1 200 "$(jobs completed success)"
respond_art 1 200 "$(art_list "$(art_entry product-core false)" "$(art_entry web-ui-screenshots false)")"
run_wait
eq "an unrelated artifact next to the one product-core: exit" "$rc" 0
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list)"
run_wait
failed "no product-core artifact"
eq "no product-core artifact: not retried" "$(requests)" 2
check "no product-core artifact: says how many" grep -q 'holds 0 artifacts named' "$d/out"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list "$(art_entry product-core false)" "$(art_entry product-core false)")"
run_wait
failed "two product-core artifacts (a failed attempt's leftover)"
eq "two product-core artifacts: not retried" "$(requests)" 2
check "two product-core artifacts: says how many" grep -q 'holds 2 artifacts named' "$d/out"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list "$(art_entry product-core true)")"
run_wait
failed "an expired product-core artifact"
check "an expired product-core artifact: says so" grep -q 'has expired' "$d/out"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list "$(art_entry product-core false)" "$(art_entry product-core true)")"
run_wait
failed "a live and an expired product-core artifact"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list "$(art_entry product-core false 9999)")"
run_wait
failed "another run's artifact"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 'not json'
run_wait
failed "artifacts response is not JSON"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 '{"total_count":1,"artifacts":[{"id":1,"name":"product-core","workflow_run":{"id":4242}}]}'
run_wait
failed "an artifact without expired"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list "$(art_entry product-core false)" | jq -c '.total_count = 150')"
run_wait
failed "more artifacts than one page"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list "$(art_entry product-core false)")" 'Link: <https://api.github.com/x?page=2>; rel="next"'
run_wait
failed "a next page of artifacts"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list "$(art_entry product-core false)" | jq -c '.total_count = 2')" 'Link: <https://api.github.com/x?page=2>; rel="next"'
run_wait
failed "a next page of artifacts next to a count mismatch"
eq "a next page of artifacts next to a count mismatch: not retried" "$(requests)" 2
# The artifacts listing has the same two reads: a mismatch is polled again, and
# neither "missing" nor "found" is concluded from it.
new_case
respond 1 200 "$(jobs completed success)"
respond_art 1 200 "$(art_list | jq -c '.total_count = 1')"
respond_art 2 200 "$(art_list "$(art_entry product-core false)")"
run_wait
eq "artifacts: a transient count mismatch: exit" "$rc" 0
eq "artifacts: a transient count mismatch: asked again after the normal gap" "$(sleeps)" 45
eq "artifacts: a transient count mismatch: requests" "$(requests)" 3
check "artifacts: an empty listing that promises one is no 'missing' verdict" absent 'holds 0 artifacts' "$d/out"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 200 "$(art_list "$(art_entry product-core false)" | jq -c '.total_count = 2')"
run_wait
failed "artifacts: a count mismatch that never settles"
eq "artifacts: a count mismatch that never settles: shares the 24-request budget" "$(requests)" 24
check "artifacts: a count mismatch that never settles: says so" grep -q 'count mismatch, never a consistent listing: 1 of 2 artifacts listed' "$d/out"
check "artifacts: a mismatching listing is not taken for the artifact" absent 'finished\|succeeded' "$d/out"
new_case
respond 1 200 "$(jobs completed success)"
respond_art last 403 '{"message":"Resource not accessible by integration"}'
run_wait
failed "artifacts: permission denied"
eq "artifacts: permission denied is not retried" "$(requests)" 2
new_case
respond 1 200 "$(jobs completed success)"
respond_art 1 429 '{}' 'Retry-After: 30'
respond_art 2 502 '{}'
run_wait
eq "artifacts: rate limit then 5xx then the default answer: exit" "$rc" 0
eq "artifacts: both retries wait the normal gap or the server's time" "$(sleeps)" '45 45'
eq "artifacts: retries count as requests" "$(requests)" 4
new_case
respond 1 200 "$(jobs completed success)"
respond_art 1 403 '{"message":"API rate limit exceeded"}' 'X-RateLimit-Remaining: 0' "X-RateLimit-Reset: $((epoch + 4000))"
run_wait
failed "artifacts: reset after the deadline"
eq "artifacts: reset after the deadline gives up at once" "$(requests)" 2
# The check shares the 24-request budget: a producer seen done on the last
# request leaves nothing to check its artifact with.
new_case
respond 24 200 "$(jobs completed success)"
respond last 200 "$(jobs queued null)"
run_wait
failed "producer done on the last request"
eq "producer done on the last request: no 25th request" "$(requests)" 24
check "producer done on the last request: says so" grep -q 'no request left' "$d/out"
new_case
respond 23 200 "$(jobs completed success)"
respond last 200 "$(jobs queued null)"
respond_art last 502 '{}'
run_wait
failed "artifact check retries run out of the shared budget"
eq "artifact check retries run out of the shared budget: 24 requests" "$(requests)" 24

# --- cancellation ------------------------------------------------------------
new_case
mkfifo "$d/fifo"
# env execs the script, so $! is the script itself.
env GITHUB_REPOSITORY=owner/repo GITHUB_RUN_ID=4242 GITHUB_TOKEN="$token" WAIT_PRODUCT_JITTER_MAX=0 \
  PATH="$work/bin:$PATH" MOCK_DIR="$d" REAL_DATE="$real_date" REAL_SLEEP="$real_sleep" \
  "$script" > "$d/out" 2>&1 &
runner=$!
exec 4< "$d/fifo"
read -r _ <&4
kill -TERM "$runner"
rc=0
wait "$runner" || rc=$?
eq "TERM during a request: exit status" "$rc" 143
# EOF arrives once every holder of the FIFO's write end, i.e. the mock curl, is gone.
cat <&4 > /dev/null
exec 4<&-
check "TERM during a request: no token left in the log" absent "$token" "$d/out"

# --- environment -------------------------------------------------------------
new_case
run_wait GITHUB_TOKEN=
failed "missing token"
eq "missing token: no request" "$(requests)" 0
new_case
run_wait GITHUB_TOKEN='abc"def'
failed "a token that could break the curl config"
eq "unusable token: no request" "$(requests)" 0
new_case
run_wait GITHUB_REPOSITORY='owner/repo;rm'
failed "odd repository name"
eq "odd repository name: no request" "$(requests)" 0
new_case
run_wait GITHUB_RUN_ID=12x
failed "odd run id"

# --- check-source ------------------------------------------------------------
src=$work/downloaded
mkdir "$src"
echo '{"source_sha":"deadbeef"}' > "$src/product-manifest.json"
new_case
rc=0
GITHUB_SHA=deadbeef "$script" check-source "$src" > "$d/out" 2>&1 || rc=$?
eq "check-source: matching sha" "$rc" 0
rc=0
GITHUB_SHA=cafef00d "$script" check-source "$src" > "$d/out" 2>&1 || rc=$?
failed "check-source: another commit"
echo '{"file":"x"}' > "$src/product-manifest.json"
rc=0
GITHUB_SHA=deadbeef "$script" check-source "$src" > "$d/out" 2>&1 || rc=$?
failed "check-source: no source_sha"
rm "$src/product-manifest.json"
rc=0
GITHUB_SHA=deadbeef "$script" check-source "$src" > "$d/out" 2>&1 || rc=$?
failed "check-source: no manifest"

if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "wait-product-artifact: $cases cases passed"
