#!/usr/bin/env bash
# Report what a `cargo nextest archive` holds: the uncompressed size of the
# archive, split by package, and its largest files. Reading it costs one pass
# over the archive (list) plus one (metadata), so run it where the archive is
# already downloaded and nothing waits on the job.
#
# Usage: scripts/report-archive-composition.sh <workspace.tar.zst>
#   EVORCH_ARCHIVE_TAR_FLAGS  decompression flag for tar (default --zstd).
set -euo pipefail

archive="${1:?usage: report-archive-composition.sh <archive>}"
tar_flags="${EVORCH_ARCHIVE_TAR_FLAGS:---zstd}"
command -v jq >/dev/null || { echo "report-archive-composition: jq is required" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# shellcheck disable=SC2086 # the flag is a single optional word
tar $tar_flags -tvf "$archive" | awk '$1 !~ /^l/ { print $3 "\t" $6 }' > "$work/files.tsv"
# shellcheck disable=SC2086
tar $tar_flags -xOf "$archive" target/nextest/binaries-metadata.json > "$work/metadata.json"
jq -r '.["rust-binaries"][] | [(.["binary-path"] | split("/") | last), (.["binary-id"] | split("::")[0])] | @tsv' \
  "$work/metadata.json" > "$work/packages.tsv"

report() {
  awk -F'\t' -v compressed="$(stat -c %s "$archive")" '
    NR == FNR { package[$1] = $2; next }
    {
      n = split($2, parts, "/")
      name = parts[n]
      if (name in package) { key = package[name]; binaries[key]++ }
      else if ($1 > 0) { key = "(not a test binary: " parts[1] "/" parts[2] "/" parts[3] ")" }
      else { next }
      size[key] += $1
      total += $1
      files++
    }
    END {
      printf "### Nextest archive composition\n\n"
      printf "compressed %.2f GB, uncompressed %.2f GB, %d files\n\n", compressed / 1e9, total / 1e9, files
      printf "| group | binaries | uncompressed MB | share |\n| --- | ---: | ---: | ---: |\n"
      for (key in size) printf "%s\t%d\t%.0f\t%.1f%%\n", key, binaries[key], size[key] / 1e6, 100 * size[key] / total | "sort -t\"\t\" -k3,3nr"
    }
  ' "$work/packages.tsv" "$work/files.tsv" | awk -F'\t' '
    NF == 4 { printf "| %s | %s | %s | %s |\n", $1, $2, $3, $4; next }
    { print }'
  printf '\nLargest files (MB):\n\n'
  sort -t "$(printf '\t')" -k1,1nr "$work/files.tsv" | head -15 \
    | awk -F'\t' '{ printf "- %.0f %s\n", $1 / 1e6, $2 }'
}

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  report | tee -a "$GITHUB_STEP_SUMMARY"
else
  report
fi
