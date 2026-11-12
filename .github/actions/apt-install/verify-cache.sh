#!/usr/bin/env bash
# Check that the .debs of an apt-install cache entry are signed Ubuntu archive
# packages, using only the runner image's own Ubuntu archive keyring as the trust
# root (nothing inside the cache is trusted).
#
# Usage: verify-cache.sh collect <cache-dir>   after a completed install: copy the
#                                              InRelease files and the Packages
#                                              indexes that list the .debs of
#                                              <cache-dir> into <cache-dir>/lists
#        verify-cache.sh verify <cache-dir>    exit 0 only when every .deb in
#                                              <cache-dir> is listed by a Packages
#                                              index that a signed InRelease vouches for
#
# verify checks, in this order:
#   1. each lists/*InRelease verifies against the Ubuntu archive keyring and is not
#      older than the runner image's own InRelease of the same signed Origin and
#      Suite (the image's must verify too; the file names are not signed and are
#      never used). A release with no such counterpart is not trusted. There is no
#      absolute age limit: the release pocket's InRelease is never refreshed;
#   2. each Packages index (decompressed) has the sha256 that a verified InRelease
#      lists for a Packages file; any other index is ignored;
#   3. the sha256 of every *.deb is among the SHA256 fields of those indexes.
# A .deb whose bytes are not an archive package, or an index that is not vouched
# for, makes verify fail, and the caller then installs through apt as usual.
#
# Environment (all optional; the defaults are the runner's):
#   APT_LISTS_DIR              /var/lib/apt/lists (collect: the source of the indexes;
#                              verify: the image's InRelease files, the baseline)
#   APT_VERIFY_KEYRINGS        space-separated keyrings, default the Ubuntu archive ones
#   APT_HELPER                 /usr/lib/apt/apt-helper (reads compressed indexes)
set -euo pipefail

mode="${1:?usage: verify-cache.sh collect|verify <cache-dir>}"
cache="${2:?usage: verify-cache.sh collect|verify <cache-dir>}"
lists_dir="${APT_LISTS_DIR:-/var/lib/apt/lists}"
helper="${APT_HELPER:-/usr/lib/apt/apt-helper}"

fail() { echo "apt-install verify: $*" >&2; exit 1; }

# The decompressed bytes of an index file, whatever apt stored it as.
index_content() {
  case "$1" in
    *Packages) cat "$1" ;;
    *.gz) zcat "$1" ;;
    *.xz) xzcat "$1" ;;
    *) "$helper" cat-file "$1" ;;
  esac
}

debs=()
for deb in "$cache"/*.deb; do
  [[ -f "$deb" ]] && debs+=("$deb")
done
[[ ${#debs[@]} -gt 0 ]] || fail "no .deb in $cache"

case "$mode" in
  collect)
    mkdir -p "$cache/lists"
    for release in "$lists_dir"/*InRelease; do
      [[ -f "$release" ]] && cp "$release" "$cache/lists/"
    done
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    sha256sum "${debs[@]}" | cut -d' ' -f1 > "$tmp/deb-hashes"
    for index in "$lists_dir"/*Packages*; do
      [[ -f "$index" ]] || continue
      index_content "$index" > "$tmp/index"
      # Keep an index that lists at least one of the .debs.
      if awk 'NR == FNR { want[$1] = 1; next } /^SHA256: / && ($2 in want) { found = 1; exit } END { exit !found }' \
        "$tmp/deb-hashes" "$tmp/index"; then
        cp "$index" "$cache/lists/"
      fi
    done
    ;;
  verify)
    keyring_args=()
    if [[ -n "${APT_VERIFY_KEYRINGS:-}" ]]; then
      # shellcheck disable=SC2206 # word splitting is the keyring list
      keyrings=(${APT_VERIFY_KEYRINGS})
    else
      keyrings=(/usr/share/keyrings/ubuntu-archive-keyring.gpg /etc/apt/trusted.gpg.d/ubuntu-keyring-*.gpg)
    fi
    for keyring in "${keyrings[@]}"; do
      [[ -f "$keyring" ]] && keyring_args+=(--keyring "$keyring")
    done
    [[ ${#keyring_args[@]} -gt 0 ]] || fail "no Ubuntu archive keyring on this runner"

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    # What a verified InRelease says about itself: "Origin|Suite|Date as epoch".
    # The file name is not signed, so it is never used to pair releases.
    release_identity() { # release_identity <file>; the file must have been verified
      local origin suite released
      origin="$(sed -n 's/^Origin: //p' "$1" | head -n 1)"
      suite="$(sed -n 's/^Suite: //p' "$1" | head -n 1)"
      released="$(date -d "$(sed -n 's/^Date: //p' "$1" | head -n 1)" +%s 2> /dev/null)" || return 1
      [[ -n "$origin" && -n "$suite" ]] || return 1
      printf '%s|%s|%s\n' "$origin" "$suite" "$released"
    }

    # The image's own InRelease files are the baseline for what is current: they
    # count only when they verify against the same keyring, and they are paired
    # with a cached one by the signed Origin and Suite.
    : > "$tmp/baseline"
    for release in "$lists_dir"/*InRelease; do
      [[ -f "$release" ]] || continue
      gpgv "${keyring_args[@]}" --output "$tmp/image-release" "$release" 2> /dev/null || continue
      release_identity "$tmp/image-release" >> "$tmp/baseline" || continue
    done

    : > "$tmp/trusted-index-hashes"
    releases_total=0
    releases_ok=0
    for release in "$cache"/lists/*InRelease; do
      [[ -f "$release" ]] || continue
      releases_total=$((releases_total + 1))
      gpgv "${keyring_args[@]}" --output "$tmp/release" "$release" 2> /dev/null || continue
      identity="$(release_identity "$tmp/release")" || continue
      # Without an image release of the same Origin and Suite there is nothing to
      # tell a current snapshot from a replayed old one, so it is not trusted.
      newest="$(awk -F'|' -v key="${identity%|*}" '$1 "|" $2 == key && $3 > best { best = $3 } END { if (best != "") print best }' "$tmp/baseline")"
      [[ -n "$newest" ]] || continue
      (( ${identity##*|} >= newest )) || continue
      releases_ok=$((releases_ok + 1))
      awk '/^SHA256:/ { in_hashes = 1; next } /^[^ ]/ { in_hashes = 0 } in_hashes && $3 ~ /(^|\/)Packages$/ { print $1 }' \
        "$tmp/release" >> "$tmp/trusted-index-hashes"
    done
    [[ -s "$tmp/trusted-index-hashes" ]] || fail "no InRelease verified against the Ubuntu archive keyring"

    : > "$tmp/deb-hashes"
    indexes_total=0
    indexes_ok=0
    for index in "$cache"/lists/*Packages*; do
      [[ -f "$index" ]] || continue
      indexes_total=$((indexes_total + 1))
      index_content "$index" > "$tmp/index"
      index_hash="$(sha256sum "$tmp/index" | cut -d' ' -f1)"
      grep -qxF "$index_hash" "$tmp/trusted-index-hashes" || continue
      indexes_ok=$((indexes_ok + 1))
      awk '/^SHA256: / { print $2 }' "$tmp/index" >> "$tmp/deb-hashes"
    done
    [[ -s "$tmp/deb-hashes" ]] || fail "no Packages index is vouched for by a verified InRelease"

    for deb in "${debs[@]}"; do
      deb_hash="$(sha256sum "$deb" | cut -d' ' -f1)"
      grep -qxF "$deb_hash" "$tmp/deb-hashes" || fail "${deb##*/} is not listed by a verified index (InRelease verified: $releases_ok/$releases_total, Packages vouched for: $indexes_ok/$indexes_total)"
    done
    echo "apt-install verify: ${#debs[@]} .debs match the signed Ubuntu archive indexes (InRelease verified: $releases_ok/$releases_total, Packages vouched for: $indexes_ok/$indexes_total)"
    ;;
  *) fail "unknown mode: $mode" ;;
esac
