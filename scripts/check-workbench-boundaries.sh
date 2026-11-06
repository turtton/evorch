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
#      than workbench-protocol, reaches a GUI or rendering crate, or declares a
#      third-party dependency outside its small allowlist. It is a protocol
#      adapter: the core binary connects it to workbench-host.
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
CLIENT_EXTERNAL_ALLOW="serde thiserror tokio"
WEB_EXTERNAL_ALLOW="axum futures-core http serde serde_json thiserror tokio"
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

command -v jq >/dev/null || { echo "check-workbench-boundaries: jq is required" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

if [[ $# -ge 1 ]]; then
  cp "$1" "$work/metadata.json"
else
  cargo metadata --format-version 1 --locked > "$work/metadata.json"
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

for crate in "$PROTOCOL" "$CLIENT" "$HOST" "$WEB"; do
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

closure_violations "$PROTOCOL" "" >> "$violations"
closure_violations "$CLIENT" "$PROTOCOL" >> "$violations"
gui_free_violations "$HOST" "$HOST_FORBIDDEN_WORKSPACE" >> "$violations"
gui_free_violations "$WEB" "$WEB_FORBIDDEN_WORKSPACE" >> "$violations"
external_violations "$PROTOCOL" "$PROTOCOL_EXTERNAL_ALLOW" >> "$violations"
external_violations "$CLIENT" "$CLIENT_EXTERNAL_ALLOW" >> "$violations"
closure_violations "$WEB" "$PROTOCOL" >> "$violations"
external_violations "$WEB" "$WEB_EXTERNAL_ALLOW" >> "$violations"
core_reach_violations >> "$violations"

if [[ -s "$violations" ]]; then
  sort -u "$violations" >&2
  echo "workbench boundary check failed: $(sort -u "$violations" | wc -l) violation(s)" >&2
  exit 1
fi
edges=$(grep -c '^E' "$work/edges.tsv" || true)
echo "workbench boundary check passed ($edges dependency edges inspected)"
