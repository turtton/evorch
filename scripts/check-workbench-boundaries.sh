#!/usr/bin/env bash
# Architecture boundary check for the workbench protocol crates (ADR 0045 §11).
#
# Reads the `cargo metadata` dependency graph and fails when
#   1. workbench-protocol reaches any other workspace crate (normal, build or
#      dev; optional and target-specific declarations included),
#   2. workbench-client reaches a workspace crate other than workbench-protocol,
#   3. workbench-protocol or workbench-client declares a third-party
#      dependency outside its small allowlist, or
#   4. a core implementation crate (runtime, storage, sandbox, ...) depends on,
#      directly or through any other crate, a workbench-* crate.
#   5. workbench-host reaches a GUI or client crate (gui, workspace-ui,
#      workbench-client, browser-cef, evorch) or a rendering stack
#      (egui, eframe, wgpu, winit, CEF) through any dependency path.
#   6. workbench-web (the HTTP + SSE adapter) reaches any workspace crate other
#      than workbench-protocol and workbench-authority, reaches a GUI or
#      rendering crate, or declares a third-party dependency outside its small
#      allowlist. It is a protocol adapter: the core binary connects it to
#      workbench-host. The authority is the only addition (5a-I): it carries the
#      request authority the adapter reads from its request context, and it is
#      itself host-free (rule 9). There is no allowlist exception for the web
#      crate to reach workbench-host, the client or any core crate.
#   7. gui or evorch (the CLI) declares a direct dependency on a workspace
#      crate that is not in scripts/workbench-boundary-allowlist.txt. That list
#      is a ratchet: it freezes the dependencies the two crates have today
#      (normal, dev, build, optional and target-specific alike), may only
#      shrink, and records the migration PR that removes each entry. The
#      client-side crates workbench-client, workbench-protocol, core-instance
#      and core-launcher are the intended way to reach the core and need no
#      entry. An entry whose dependency is gone is reported as stale
#      (a warning; a failure when WORKBENCH_BOUNDARY_STALE_ALLOWLIST=fail).
#      Because those four are exempt, core-instance must reach no other workspace
#      crate and core-launcher only core-instance, workbench-client and
#      workbench-protocol (checked once the crates exist).
#   8. test-socket-dir (short temporary directories for Unix sockets in tests)
#      is a leaf: it reaches no workspace crate and only dev edges point at it.
#      Under that rule workbench-client and core-launcher may list it as a
#      dev-dependency, which rule 2 and the core-launcher check below would
#      otherwise reject for any workspace crate.
#   9. workbench-authority (the non-wire transport authority, 5a-I) reaches no
#      workspace crate other than workbench-protocol (so never the host, the
#      web adapter, the runtime or the storage), declares only the third-party
#      dependencies and features of its explicit list (tokio sync/net/time,
#      rustix process, uuid v4), and only workbench-host, workbench-web and
#      evorch-core depend on it. The dependency direction is host, web and the
#      core binary -> authority -> protocol.
#  10. The authority's fake verifier is the feature `test-support`. It may be
#      enabled only by a dev-dependency declaration: not by a normal or build
#      dependency, not in [workspace.dependencies], and not through any
#      [features] entry that forwards to it (`authority/test-support`,
#      `authority?/test-support`, also through a renamed dependency key),
#      and the authority's default features must not reach it. When the
#      product dependency tree is available (a real run), the normal and build
#      graph of workbench-host, workbench-web and evorch-core must not enable it.
#  11. verify_unix_peer, the one function that mints a local transport proof from
#      a kernel peer check, is called in production code only in
#      crates/evorch-core/src/unix.rs, once. The authority defines it; tests
#      (tests/ directories, tests.rs, *_tests.rs) and comments are not counted.
# Each violation prints the dependency path that causes it.
#
# Usage: scripts/check-workbench-boundaries.sh [metadata.json]
#   Without an argument it runs `cargo metadata --format-version 1 --locked`.
#   The optional file is used by scripts/tests/test_check_workbench_boundaries.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

PROTOCOL=workbench-protocol
CLIENT=workbench-client
HOST=workbench-host
WEB=workbench-web
PROTOCOL_EXTERNAL_ALLOW="serde serde_json thiserror"
# tracing: the only trace of the socket error that ends a core connection (it is
# folded into TransportError::Disconnected); a facade crate, silent without a subscriber.
CLIENT_EXTERNAL_ALLOW="serde thiserror tokio tracing"
# TaskTracker (rt) proves detached work has exited even after handle/waiter drop.
WEB_EXTERNAL_ALLOW="axum futures-core http http-body-util hyper hyper-util serde serde_json thiserror tokio tokio-util"
# The transport authority (rule 9, 10 and 11). It may use the protocol only.
AUTH=workbench-authority
AUTH_EXTERNAL_ALLOW="tokio rustix uuid"
# Features of those third-party dependencies, as a JSON object (explicit, no wildcard).
AUTH_EXTERNAL_FEATURES='{"tokio":["sync","net","time"],"rustix":["process"],"uuid":["v4"]}'
AUTH_DEPENDENTS="workbench-host workbench-web evorch-core"
AUTH_FAKE_FEATURE=test-support
AUTH_PEER_FN=verify_unix_peer
AUTH_PEER_CALLSITE=crates/evorch-core/src/unix.rs
# Where the sources are scanned for rule 11 and the root manifest for rule 10. The
# environment overrides exist only for the fixture tests.
SOURCE_ROOT="${WORKBENCH_BOUNDARY_SOURCE_ROOT:-.}"
ROOT_MANIFEST="${WORKBENCH_BOUNDARY_ROOT_MANIFEST:-${WORKBENCH_BOUNDARY_MANIFEST_PATH:-Cargo.toml}}"
# workbench-host is the core side of the protocol. It may use core crates and
# the protocol, but nothing that draws or that speaks as a client.
HOST_FORBIDDEN_WORKSPACE="gui workspace-ui workbench-client browser-cef evorch"
# workbench-web may only use the protocol (closure_violations); this list names
# the front-end crates for the rendering-stack check, which also covers
# third-party crates.
WEB_FORBIDDEN_WORKSPACE="gui workspace-ui workbench-client workbench-host browser-cef evorch"
HOST_FORBIDDEN_EXTERNAL='^(eframe|egui|egui[-_][a-z_-]+|epaint|emath|ecolor|wgpu|wgpu[-_][a-z_-]+|winit|glow|cef|cef[-_][a-z_-]+)$'
# Core implementation crates. They own runtime state and must never import the
# client protocol; the core-side workbench-host crate is where DTO
# conversion belongs. gui, evorch and workspace-ui are the clients and are
# deliberately not listed.
CORE_CRATES="${WORKBENCH_BOUNDARY_CORE_CRATES:-agents apply-patch browser browser-cef catalog config event-bus js-engine model providers recorder routing runtime sandbox secret-guard storage tools}"
# WORKBENCH_BOUNDARY_CORE_CRATES exists only so the fixture tests can use small
# graphs; CI runs the script with the built-in list.

# Dependency ratchet for the front-end crates (rule 7).
RATCHET_CRATES="gui evorch"
# core-instance (a leaf) and core-launcher (client launcher over core-instance,
# workbench-client and workbench-protocol) are client-side crates added by PR 04;
# listing them here keeps the ratchet independent of which PR merges first.
RATCHET_FREE_TARGETS="workbench-client workbench-protocol core-instance core-launcher test-socket-dir"
# The one workspace crate the client crates may use as a dev-dependency (rule 8).
TEST_LEAF=test-socket-dir
ALLOWLIST="${WORKBENCH_BOUNDARY_ALLOWLIST:-scripts/workbench-boundary-allowlist.txt}"
# WORKBENCH_BOUNDARY_ALLOWLIST lets the fixture tests supply their own list.
STALE_ALLOWLIST="${WORKBENCH_BOUNDARY_STALE_ALLOWLIST:-warn}"

command -v jq >/dev/null || { echo "check-workbench-boundaries: jq is required" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

metadata_arg=$#
if [[ $# -ge 1 ]]; then
  cp "$1" "$work/metadata.json"
else
  # --all-features: an optional dependency that no default feature enables has
  # no resolved edge, so only the all-features graph shows where a [patch]
  # section sends it. The manifest override exists for the fixture tests.
  cargo metadata --format-version 1 --locked --all-features \
    --manifest-path "${WORKBENCH_BOUNDARY_MANIFEST_PATH:-Cargo.toml}" > "$work/metadata.json"
fi

# Edge list. Lines:  W <workspace crate>
#                    E <from> <to> <kind> <note>     (workspace-crate labels have no version)
#                    D <from> <crate> <kind> <note>  (declared third-party deps of workspace crates)
# "resolved" edges come from the resolve graph (package ids); "declared" edges
# come from each manifest and so also cover optional and target-specific
# dependencies that the resolved graph may leave out.
cat > "$work/edges.jq" <<'JQ'
. as $m
| if $m.resolve == null then error("metadata has no resolve graph; run cargo metadata without --no-deps") else . end
| ($m.packages | map({key: .id, value: .}) | from_entries) as $byid
| ($m.workspace_members | map({key: ., value: true}) | from_entries) as $wsid
| ($m.workspace_members | map($byid[.].name)) as $ws
| def pkg_label($p): if $wsid[$p.id] then $p.name else "\($p.name)@\($p.version)" end;
  def note($optional; $target):
    ["declared", (if $optional then "optional" else empty end), (if $target then "target \($target)" else empty end)] | join(", ");
  ($ws[] | "W\t\(.)"),
  ($m.resolve.nodes[] | . as $n | $byid[$n.id] as $from
    | $n.deps[] | . as $d | $byid[$d.pkg] as $to
    | $d.dep_kinds[]
    | "E\t\(pkg_label($from))\t\(pkg_label($to))\t\(.kind // "normal")\tresolved\(if .target then ", target \(.target)" else "" end)"),
  ($m.packages[] | select($wsid[.id]) | . as $p | .dependencies[]
    | . as $d
    | ($d.kind // "normal") as $kind
    | if ($d.source == null and ($ws | index($d.name))) then
        "E\t\($p.name)\t\($d.name)\t\($kind)\t\(note($d.optional; $d.target))"
      else
        "D\t\($p.name)\t\($d.name)\t\($kind)\t\(note($d.optional; $d.target))"
      end)
JQ
jq -r -f "$work/edges.jq" "$work/metadata.json" > "$work/edges.tsv"

violations="$work/violations.txt"
: > "$violations"

for crate in "$PROTOCOL" "$CLIENT" "$HOST" "$WEB" "$AUTH" evorch-core $RATCHET_CRATES; do
  if ! awk -F'\t' -v c="$crate" '$1 == "W" && $2 == c { found = 1 } END { exit !found }' "$work/edges.tsv"; then
    echo "boundary check: workspace member '$crate' not found; the check would pass vacuously" >> "$violations"
  fi
done

# Every core crate named above must still exist. A renamed or removed crate
# would otherwise drop out of the reverse-dependency check without a sound.
for crate in $CORE_CRATES; do
  if ! awk -F'\t' -v c="$crate" '$1 == "W" && $2 == c { found = 1 } END { exit !found }' "$work/edges.tsv"; then
    echo "boundary check: core crate '$crate' is not a workspace member; it was renamed or removed, so update CORE_CRATES in $0" >> "$violations"
  fi
done

# Walk everything reachable from a root and report each workspace crate that is
# not allowed, with the path that reaches it. Dev edges count only at the root:
# a crate's own tests may not pull in workspace crates either, but the dev
# dependencies of its dependencies are not part of its build.
closure_violations() {
  local root=$1 allowed=$2
  awk -F'\t' -v root="$root" -v allowed=" $allowed " '
    $1 == "W" { ws[$2] = 1 }
    $1 == "E" { n++; from[n] = $2; to[n] = $3; kind[n] = $4; note[n] = $5; out[$2] = out[$2] " " n }
    END {
      queue[1] = root; head = 1; tail = 1; seen[root] = 1
      while (head <= tail) {
        node = queue[head++]
        m = split(out[node], idx, " ")
        for (k = 1; k <= m; k++) {
          i = idx[k]
          if (kind[i] == "dev" && node != root) continue
          t = to[i]
          if (t in seen) continue
          seen[t] = 1; via[t] = i; queue[++tail] = t
        }
      }
      for (node in seen) {
        if (node == root || !(node in ws) || index(allowed, " " node " ")) continue
        path = ""; cur = node
        while (cur != root) {
          i = via[cur]
          path = " -> " cur " [" kind[i] "; " note[i] "]" path
          cur = from[i]
        }
        print "VIOLATION: " root " must not depend on workspace crate " node ": " root path
      }
    }' "$work/edges.tsv"
}

# Everything reachable from a root (dev edges only at the root) must be free of
# GUI and client crates, workspace or third-party. Used for workbench-host and
# workbench-web.
gui_free_violations() {
  local root=$1 forbidden=$2
  awk -F'\t' -v root="$root" -v forbidden=" $forbidden " -v external_re="$HOST_FORBIDDEN_EXTERNAL" '
    $1 == "W" { ws[$2] = 1 }
    # Third-party dependencies a manifest declares, including optional and
    # target-specific ones that the resolved graph leaves out.
    $1 == "D" && $3 ~ external_re { d++; dfrom[d] = $2; dto[d] = $3; dkind[d] = $4; dnote[d] = $5 }
    $1 == "E" { n++; from[n] = $2; to[n] = $3; kind[n] = $4; note[n] = $5; out[$2] = out[$2] " " n }
    END {
      queue[1] = root; head = 1; tail = 1; seen[root] = 1
      while (head <= tail) {
        node = queue[head++]
        m = split(out[node], idx, " ")
        for (k = 1; k <= m; k++) {
          i = idx[k]
          if (kind[i] == "dev" && node != root) continue
          t = to[i]
          if (t in seen) continue
          seen[t] = 1; via[t] = i; queue[++tail] = t
        }
      }
      for (node in seen) {
        if (node == root) continue
        split(node, parts, "@"); name = parts[1]
        bad = (node in ws) ? index(forbidden, " " name " ") : (name ~ external_re)
        if (!bad) continue
        path = ""; cur = node
        while (cur != root) {
          i = via[cur]
          path = " -> " cur " [" kind[i] "; " note[i] "]" path
          cur = from[i]
        }
        print "VIOLATION: " root " must not depend on GUI or client crate " node ": " root path
      }
      # A declaration counts at the root whatever its kind, and at any crate the
      # root reaches unless it is a dev dependency of that crate.
      for (j = 1; j <= d; j++) {
        if (!(dfrom[j] in seen)) continue
        if (dfrom[j] == root) {
          print "VIOLATION: " root " declares GUI or client dependency " dto[j] " [" dkind[j] "; " dnote[j] "]"
          continue
        }
        if (dkind[j] == "dev") continue
        path = ""; cur = dfrom[j]
        while (cur != root) {
          i = via[cur]
          path = " -> " cur " [" kind[i] "; " note[i] "]" path
          cur = from[i]
        }
        print "VIOLATION: " root " must not reach GUI or client dependency " dto[j] " declared by " dfrom[j] " [" dkind[j] "; " dnote[j] "]: " root path
      }
    }' "$work/edges.tsv"
}

external_violations() {
  local root=$1 allowed=$2
  awk -F'\t' -v root="$root" -v allowed=" $allowed " '
    $1 == "D" && $2 == root && $4 != "dev" && !index(allowed, " " $3 " ") {
      print "VIOLATION: " root " declares third-party dependency " $3 " [" $4 "; " $5 "] outside its allowlist (" allowed ")"
    }' "$work/edges.tsv"
}

# From every core crate, walk the whole dependency graph (through other
# workspace crates and third-party crates alike) and report each workbench-*
# crate that can be reached, with the path. Dev edges count only at the core
# crate itself, as for the protocol crates above.
core_reach_violations() {
  awk -F'\t' -v core=" $CORE_CRATES " '
    $1 == "W" { ws[$2] = 1 }
    $1 == "E" { n++; from[n] = $2; to[n] = $3; kind[n] = $4; note[n] = $5; out[$2] = out[$2] " " n }
    END {
      for (root in ws) {
        if (!index(core, " " root " ")) continue
        delete seen; delete via
        queue[1] = root; head = 1; tail = 1; seen[root] = 1
        while (head <= tail) {
          node = queue[head++]
          m = split(out[node], idx, " ")
          for (k = 1; k <= m; k++) {
            i = idx[k]
            if (kind[i] == "dev" && node != root) continue
            t = to[i]
            if (t in seen) continue
            seen[t] = 1; via[t] = i; queue[++tail] = t
          }
        }
        for (node in seen) {
          if (node !~ /^workbench-/) continue
          path = ""; cur = node
          while (cur != root) {
            i = via[cur]
            path = " -> " cur " [" kind[i] "; " note[i] "]" path
            cur = from[i]
          }
          print "VIOLATION: core crate " root " must not depend on " node " (directly or transitively): " root path
        }
      }
    }' "$work/edges.tsv"
}

# Rule 7. Reads the allowlist into `A <crate> <dependency> <kind> <pr>` lines
# (allow.tsv), reporting malformed, duplicate and out-of-scope entries, then
# compares it with the workspace dependencies of the two crates. Declared edges
# come from the manifests, so optional and target-specific dependencies are
# covered; resolved edges cover a registry declaration that a [patch] section or
# source replacement turned into a workspace crate.
: > "$work/allow.tsv"
: > "$work/stale.txt"
ratchet_violations() {
  if [[ ! -f $ALLOWLIST ]]; then
    echo "VIOLATION: dependency allowlist $ALLOWLIST not found; the ratchet cannot run without it"
    return
  fi
  awk -v crates=" $RATCHET_CRATES " -v file="$ALLOWLIST" '
    { sub(/#.*/, "") }
    NF == 0 { next }
    NF != 4 { print "VIOLATION: " file ":" NR ": expected `<crate> <dependency> <kind> <removal-pr>`, got " NF " field(s)"; next }
    !index(crates, " " $1 " ") { print "VIOLATION: " file ":" NR ": crate " $1 " is not covered by the ratchet (" crates ")"; next }
    $3 !~ /^(normal|dev|build)$/ { print "VIOLATION: " file ":" NR ": kind " $3 " must be normal, dev or build"; next }
    $4 !~ /^(unassigned|[0-9]+[a-z]?)$/ { print "VIOLATION: " file ":" NR ": removal PR " $4 " must be a PR step such as 19b, or unassigned"; next }
    (($1 SUBSEP $2 SUBSEP $3) in seen) { print "VIOLATION: " file ":" NR ": duplicate entry " $1 " " $2 " " $3; next }
    { seen[$1, $2, $3] = 1; print "A\t" $1 "\t" $2 "\t" $3 "\t" $4 }' "$ALLOWLIST" > "$work/allow.raw"
  grep -v '^A' "$work/allow.raw" || true
  grep '^A' "$work/allow.raw" > "$work/allow.tsv" || true
  awk -F'\t' -v crates=" $RATCHET_CRATES " -v free=" $RATCHET_FREE_TARGETS " -v file="$ALLOWLIST" -v stale_file="$work/stale.txt" '
    FILENAME ~ /allow\.tsv$/ { allow[$2, $3, $4] = $5; next }
    $1 == "W" { ws[$2] = 1; next }
    $1 == "E" && ($3 in ws) && index(crates, " " $2 " ") && !index(free, " " $3 " ") {
      key = $2 SUBSEP $3 SUBSEP $4
      present[key] = 1
      if ($5 ~ /^declared/) notes[key] = notes[key] ((key in noted) ? " | " : "") $5
      else if (!(key in noted)) fallback[key] = $5
      if ($5 ~ /^declared/) noted[key] = 1
      if (!(($2, $3, $4) in allow) && !(key in reported)) {
        reported[key] = 1; order[++n] = key
      }
    }
    END {
      for (i = 1; i <= n; i++) {
        key = order[i]; split(key, p, SUBSEP)
        note = (key in noted) ? notes[key] : fallback[key] " (not declared as a workspace path dependency; a [patch] or source replacement points a registry dependency at a workspace crate)"
        print "VIOLATION: " p[1] " declares workspace dependency " p[2] " [" p[3] "; " note "] that is not in " file ". Reach the core through workbench-client instead of adding entries (ADR 0045 section 11)"
      }
      for (k in allow) {
        split(k, p, SUBSEP)
        if (!((p[1] SUBSEP p[2] SUBSEP p[3]) in present))
          print "stale allowlist entry: " p[1] " " p[2] " " p[3] " (removal PR " allow[k] ") is no longer declared; delete it from " file > stale_file
      }
    }' "$work/allow.tsv" "$work/edges.tsv"
}
ratchet_violations >> "$violations"

closure_violations "$PROTOCOL" "" >> "$violations"
closure_violations "$CLIENT" "$PROTOCOL $TEST_LEAF" >> "$violations"
gui_free_violations "$HOST" "$HOST_FORBIDDEN_WORKSPACE" >> "$violations"
# evorch-core is a composition root, not a runtime implementation crate. Its
# integration tests may use workbench-client, but no rendering/GUI dependency.
gui_free_violations "evorch-core" "gui workspace-ui browser-cef evorch" >> "$violations"
gui_free_violations "$WEB" "$WEB_FORBIDDEN_WORKSPACE" >> "$violations"
external_violations "$PROTOCOL" "$PROTOCOL_EXTERNAL_ALLOW" >> "$violations"
external_violations "$CLIENT" "$CLIENT_EXTERNAL_ALLOW" >> "$violations"
closure_violations "$WEB" "$PROTOCOL $AUTH" >> "$violations"
external_violations "$WEB" "$WEB_EXTERNAL_ALLOW" >> "$violations"
closure_violations "$AUTH" "$PROTOCOL" >> "$violations"
external_violations "$AUTH" "$AUTH_EXTERNAL_ALLOW" >> "$violations"
core_reach_violations >> "$violations"

# Rule 9: only the host, the web adapter and the core binary depend on the
# authority, with any kind of edge (their tests use the fake verifier).
authority_dependent_violations() {
  awk -F'\t' -v auth="$AUTH" -v allowed=" $AUTH_DEPENDENTS " '
    $1 == "E" && $3 == auth && !index(allowed, " " $2 " ") {
      print "VIOLATION: " $2 " must not depend on " auth " (only" allowed "may): " $2 " -> " auth " [" $4 "; " $5 "]"
    }' "$work/edges.tsv"
}
authority_dependent_violations >> "$violations"

# Rule 9 (third-party features) and rule 10 (the fake verifier feature), from the
# metadata of every workspace member: its declared dependencies (with the
# features, kind and rename each declaration carries) and its [features] table.
cat > "$work/authority.jq" <<'JQ'
. as $m
| ($m.packages | map({key: .id, value: .}) | from_entries) as $byid
| ($m.workspace_members | map($byid[.])) as $ws
| def closure($f; $start):
    {seen: [], queue: $start}
    | until(.queue | length == 0;
        .queue[0] as $n
        | .queue |= .[1:]
        | if (.seen | index($n)) then .
          else .seen += [$n]
               | .queue += (($f[$n] // []) | map(select(contains("/") | not) | sub("^dep:"; "")))
          end)
    | .seen;
  $ws[] | . as $p
  | (
      ( .dependencies[] | select(.name == $auth and ((.kind // "normal") != "dev") and ((.features // []) | index($fake)))
        | "VIOLATION: \($p.name) enables \($auth)/\($fake) through a \(.kind // "normal") dependency; only a dev-dependency may"),
      ( ([.dependencies[] | select(.name == $auth) | (.rename // .name)] + [$auth] | unique) as $aliases
        | (.features // {}) | to_entries[] | . as $e | $e.value[]
        | select(. as $v | $aliases | any(. as $a | $v == "\($a)/\($fake)" or $v == "\($a)?/\($fake)"))
        | "VIOLATION: feature \($e.key) of \($p.name) forwards to \(.); the fake verifier is enabled by a dev-dependency declaration only"),
      ( select(.name == $auth)
        | (.features // {}) as $f
        | select(closure($f; ["default"]) | index($fake))
        | "VIOLATION: the default features of \($auth) reach \($fake)"),
      ( select(.name == $auth)
        | .dependencies[] | select((.kind // "normal") != "dev") | . as $d
        | ($allowed[$d.name] // null) as $ok | select($ok != null)
        | (($d.features // []) - $ok) | select(length > 0)
        | "VIOLATION: \($auth) enables feature(s) \(join(", ")) of \($d.name) outside its list (\($ok | join(", ")))")
    )
JQ
jq -r --arg auth "$AUTH" --arg fake "$AUTH_FAKE_FEATURE" --argjson allowed "$AUTH_EXTERNAL_FEATURES" \
  -f "$work/authority.jq" "$work/metadata.json" >> "$violations"

# Rule 10, root manifest: [workspace.dependencies] must not enable the fake verifier
# (an inherited declaration would carry it into a normal dependency of a member).
workspace_dependency_violations() {
  [[ -f $ROOT_MANIFEST ]] || return 0
  awk -v auth="$AUTH" '
    function evaluate(text, name) {
      if (text !~ /"test-support"/) return
      if (name == auth || text ~ ("package[[:space:]]*=[[:space:]]*\"" auth "\"") || text ~ ("^[[:space:]]*" auth "[[:space:]]*[.=]"))
        print "VIOLATION: [workspace.dependencies] of " FILENAME " enables the fake verifier of " auth " (" name "); only dev-dependency declarations may"
    }
    function close_entry() { if (stmt != "") evaluate(stmt, entry); stmt = ""; bal = 0 }
    { line = $0; sub(/^[[:space:]]+/, "", line) }
    line ~ /^\[/ {
      close_entry(); in_table = 0; in_deps = 0
      if (line ~ /^\[workspace\.dependencies\][[:space:]]*(#.*)?$/) { in_deps = 1; next }
      if (match(line, /^\[workspace\.dependencies\.[^]]+\]/)) {
        in_table = 1; entry = substr(line, 1, RLENGTH); sub(/^\[workspace\.dependencies\./, "", entry); sub(/\]$/, "", entry)
        gsub(/"/, "", entry); stmt = ""; next
      }
      next
    }
    in_table { sub(/#.*/, "", line); stmt = stmt "\n" line; next }
    in_deps {
      sub(/#.*/, "", line)
      if (line == "" && stmt == "") next
      if (stmt == "") { entry = line; sub(/[[:space:]]*[.=].*/, "", entry); gsub(/"/, "", entry) }
      stmt = stmt "\n" line
      t = line; opens = gsub(/[[{]/, "", t); closes = gsub(/[]}]/, "", t)
      bal += opens - closes
      if (bal <= 0) { evaluate(stmt, entry); stmt = ""; bal = 0 }
    }
    END { close_entry() }' "$ROOT_MANIFEST"
}
workspace_dependency_violations >> "$violations"

# core-instance and core-launcher are exempt from the ratchet (rule 7), so what
# they reach is checked here. They are new crates: the checks apply once a crate
# exists in the workspace and are skipped before that.
has_member() { awk -F'\t' -v c="$1" '$1 == "W" && $2 == c { found = 1 } END { exit !found }' "$work/edges.tsv"; }
if has_member core-instance; then
  closure_violations core-instance "" >> "$violations"
fi
if has_member core-launcher; then
  closure_violations core-launcher "core-instance $CLIENT $PROTOCOL $TEST_LEAF" >> "$violations"
fi

# Rule 8. The leaf is checked once the crate exists in the workspace.
test_leaf_violations() {
  awk -F'\t' -v leaf="$TEST_LEAF" '
    $1 == "E" && $3 == leaf && $4 != "dev" {
      print "VIOLATION: " $2 " must depend on " leaf " only as a dev-dependency: " $2 " -> " leaf " [" $4 "; " $5 "]"
    }' "$work/edges.tsv"
}
if has_member "$TEST_LEAF"; then
  closure_violations "$TEST_LEAF" "" >> "$violations"
  test_leaf_violations >> "$violations"
fi

# Rule 11. The only production call of the local-proof minting function is the
# one in the core's UDS accept path. Imports count as uses outside that file, so a
# use statement elsewhere fails too.
peer_callsite_violations() {
  [[ -d $SOURCE_ROOT/crates ]] || return 0
  (cd "$SOURCE_ROOT" && grep -rn --include='*.rs' "$AUTH_PEER_FN" crates 2>/dev/null || true) \
    | awk -v auth_dir="crates/$AUTH/" -v allowed="$AUTH_PEER_CALLSITE" -v fn="$AUTH_PEER_FN" '
      {
        file = $0; sub(/:.*/, "", file)
        text = $0; sub(/^[^:]*:[0-9]+:/, "", text)
        sub(/^[[:space:]]+/, "", text)
        if (index(file, auth_dir) == 1) next
        if (file ~ /(^|\/)tests\// || file ~ /(^|\/)tests\.rs$/ || file ~ /_tests\.rs$/) next
        if (text ~ /^\/\//) next
        if (file != allowed) {
          print "VIOLATION: " file " uses " fn "; its only production call site is " allowed ": " file ": " text
          next
        }
        if (text ~ /^(pub(\([a-z]+\))? )?use /) next
        calls++
      }
      END {
        if (calls != 1) print "VIOLATION: " allowed " must call " fn " exactly once in production code, found " calls + 0
      }'
}
if has_member "$AUTH"; then
  peer_callsite_violations >> "$violations"
fi

# Rule 10, product build: in the normal and build graph (no dev-dependency, as a
# product binary is built) of the crates that depend on the authority, the fake
# verifier feature is not enabled. A real run asks cargo tree; the fixture tests
# supply the listing in WORKBENCH_BOUNDARY_PRODUCT_TREE.
product_feature_violations() {
  local crate listing="$work/product-tree.txt"
  : > "$listing"
  if [[ -n ${WORKBENCH_BOUNDARY_PRODUCT_TREE:-} ]]; then
    cp "$WORKBENCH_BOUNDARY_PRODUCT_TREE" "$listing"
  elif [[ $metadata_arg -eq 0 && -z ${WORKBENCH_BOUNDARY_SKIP_PRODUCT_TREE:-} ]]; then
    for crate in $AUTH_DEPENDENTS; do
      if ! cargo tree --locked --manifest-path "${WORKBENCH_BOUNDARY_MANIFEST_PATH:-Cargo.toml}" \
          -p "$crate" -e normal,build --prefix none -f '{p} {f}' >> "$listing" 2> "$work/product-tree.err"; then
        echo "VIOLATION: cargo tree of $crate failed, so the product feature set of $AUTH cannot be checked: $(head -c 300 "$work/product-tree.err")"
      fi
    done
  fi
  awk -v auth="$AUTH" -v fake="$AUTH_FAKE_FEATURE" '
    { sub(/[[:space:]]+\(\*\)$/, "") }
    $1 == auth {
      n = split($NF, feats, ",")
      for (i = 1; i <= n; i++) if (feats[i] == fake) {
        print "VIOLATION: the product (normal and build) dependency graph enables " auth "/" fake ": " $0; break
      }
    }' "$listing"
}
if has_member "$AUTH"; then
  product_feature_violations >> "$violations"
fi

if [[ -s "$work/stale.txt" ]]; then
  if [[ $STALE_ALLOWLIST == fail ]]; then
    sed 's/^/VIOLATION: /' "$work/stale.txt" >> "$violations"
  else
    sort -u "$work/stale.txt" | sed 's/^/warning: /' >&2
  fi
fi

if [[ -s "$violations" ]]; then
  sort -u "$violations" >&2
  echo "workbench boundary check failed: $(sort -u "$violations" | wc -l) violation(s)" >&2
  exit 1
fi
edges=$(grep -c '^E' "$work/edges.tsv" || true)
allowed=$(grep -c '^A' "$work/allow.tsv" || true)
echo "workbench boundary check passed ($edges dependency edges inspected, $allowed allowlisted gui/evorch dependencies)"
