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
#      older than APT_VERIFY_MAX_AGE_DAYS (an old signed snapshot cannot be replayed
#      indefinitely);
#   2. each Packages index (decompressed) has the sha256 that a verified InRelease
#      lists for a Packages file; any other index is ignored;
#   3. the sha256 of every *.deb is among the SHA256 fields of those indexes.
# A .deb whose bytes are not an archive package, or an index that is not vouched
# for, makes verify fail, and the caller then installs through apt as usual.
#
# Environment (all optional; the defaults are the runner's):
#   APT_LISTS_DIR              /var/lib/apt/lists (collect only)
#   APT_VERIFY_KEYRINGS        space-separated keyrings, default the Ubuntu archive ones
#   APT_HELPER                 /usr/lib/apt/apt-helper (reads compressed indexes)
#   APT_VERIFY_MAX_AGE_DAYS    60
#   APT_VERIFY_NOW             epoch seconds to treat as now
set -euo pipefail

mode="${1:?usage: verify-cache.sh collect|verify <cache-dir>}"
cache="${2:?usage: verify-cache.sh collect|verify <cache-dir>}"
lists_dir="${APT_LISTS_DIR:-/var/lib/apt/lists}"
helper="${APT_HELPER:-/usr/lib/apt/apt-helper}"
max_age_days="${APT_VERIFY_MAX_AGE_DAYS:-60}"
now="${APT_VERIFY_NOW:-$(date +%s)}"

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
    : > "$tmp/trusted-index-hashes"
    for release in "$cache"/lists/*InRelease; do
      [[ -f "$release" ]] || continue
      gpgv "${keyring_args[@]}" --output "$tmp/release" "$release" 2> /dev/null || continue
      released="$(sed -n 's/^Date: //p' "$tmp/release" | head -n 1)"
      released_at="$(date -d "$released" +%s 2> /dev/null)" || continue
      (( released_at <= now + 86400 && now - released_at <= max_age_days * 86400 )) || continue
      awk '/^SHA256:/ { in_hashes = 1; next } /^[^ ]/ { in_hashes = 0 } in_hashes && $3 ~ /(^|\/)Packages$/ { print $1 }' \
        "$tmp/release" >> "$tmp/trusted-index-hashes"
    done
    [[ -s "$tmp/trusted-index-hashes" ]] || fail "no InRelease verified against the Ubuntu archive keyring"

    : > "$tmp/deb-hashes"
    for index in "$cache"/lists/*Packages*; do
      [[ -f "$index" ]] || continue
      index_content "$index" > "$tmp/index"
      index_hash="$(sha256sum "$tmp/index" | cut -d' ' -f1)"
      grep -qxF "$index_hash" "$tmp/trusted-index-hashes" || continue
      awk '/^SHA256: / { print $2 }' "$tmp/index" >> "$tmp/deb-hashes"
    done
    [[ -s "$tmp/deb-hashes" ]] || fail "no Packages index is vouched for by a verified InRelease"

    for deb in "${debs[@]}"; do
      deb_hash="$(sha256sum "$deb" | cut -d' ' -f1)"
      grep -qxF "$deb_hash" "$tmp/deb-hashes" || fail "${deb##*/} is not an archive package listed by a verified index"
    done
    echo "apt-install verify: ${#debs[@]} .debs match the signed Ubuntu archive indexes"
    ;;
  *) fail "unknown mode: $mode" ;;
esac
