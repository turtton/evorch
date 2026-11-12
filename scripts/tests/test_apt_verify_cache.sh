#!/usr/bin/env bash
# .github/actions/apt-install/verify-cache.sh must accept a cache entry only when
# every .deb is listed by a Packages index that an InRelease signed by a trusted
# key vouches for, and that InRelease is not older than the runner image's own one
# of the same signed Origin and Suite. The test builds a signed fake archive with
# throwaway keys.
set -euo pipefail
cd "$(dirname "$0")/../.."
verify=.github/actions/apt-install/verify-cache.sh

for tool in gpg gpgv gzip zcat; do
  command -v "$tool" > /dev/null || { echo "skipping: $tool is not installed"; exit 0; }
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export GNUPGHOME="$work/gnupg"
mkdir -m 700 "$GNUPGHOME"

new_key() { # new_key <name>: writes $work/<name>.gpg (binary keyring), keeps the secret key
  gpg --batch --quiet --passphrase '' --quick-generate-key "$1 <$1@example.invalid>" default default never 2> /dev/null
  gpg --batch --quiet --export "$1@example.invalid" > "$work/$1.gpg"
}
new_key trusted
new_key stranger

cat > "$work/helper" <<'EOF'
#!/usr/bin/env bash
[[ "$1" == cat-file ]] && exec zcat -f "$2"
exit 2
EOF
chmod +x "$work/helper"
export APT_HELPER="$work/helper"
export APT_VERIFY_KEYRINGS="$work/trusted.gpg"

release_text() { # release_text <suite> <date> [<packages-hash> <packages-size>]
  printf 'Origin: Ubuntu\nSuite: %s\nDate: %s\nSHA256:\n' "$1" "$2"
  if [[ $# -ge 4 ]]; then
    printf ' %s %s main/binary-amd64/Packages\n 0000 1 main/binary-amd64/Packages.xz\n' "$3" "$4"
  fi
}

# The runner image's own lists: a signed InRelease per suite, the baseline of what
# is current. Created under a name that differs from the cached copies on purpose.
image="$work/image-lists"
make_image() { # make_image <suite> <date> [<signer>]
  mkdir -p "$image"
  release_text "$1" "$2" > "$work/image.txt"
  gpg --batch --quiet --yes --local-user "${3:-trusted}@example.invalid" --clearsign \
    --output "$image/image.invalid_dists_$1_InRelease" "$work/image.txt"
}
export APT_LISTS_DIR="$image"

# A signed archive: one InRelease vouching for one Packages index listing the .debs.
make_repo() { # make_repo <dir> <signer> <date> [<suite>]
  local dir=$1 signer=$2 date=$3 suite=${4:-resolute}
  rm -rf "$dir"
  mkdir -p "$dir/lists"
  printf 'the bytes of foo' > "$dir/foo_1_amd64.deb"
  printf 'the bytes of bar' > "$dir/bar_2_all.deb"
  {
    for deb in foo_1_amd64 bar_2_all; do
      printf 'Package: %s\nVersion: 1\nArchitecture: amd64\nSHA256: %s\n\n' "${deb%%_*}" \
        "$(sha256sum "$dir/$deb.deb" | cut -d' ' -f1)"
    done
  } > "$dir.Packages"
  release_text "$suite" "$date" "$(sha256sum "$dir.Packages" | cut -d' ' -f1)" "$(wc -c < "$dir.Packages")" > "$dir.InRelease.txt"
  gpg --batch --quiet --yes --local-user "$signer@example.invalid" --clearsign \
    --output "$dir/lists/archive.invalid_dists_${suite}_InRelease" "$dir.InRelease.txt"
  # Named .lz4 so that it goes through the helper.
  gzip -c "$dir.Packages" > "$dir/lists/archive.invalid_dists_${suite}_main_binary-amd64_Packages.lz4"
}
fresh=$(date -u -R)
old=$(date -u -R -d '200 days ago')

expect_ok() { # expect_ok <what> <cache-dir>
  "$verify" verify "$2" > "$work/log" 2>&1 || { cat "$work/log" >&2; echo "rejected a good cache: $1" >&2; exit 1; }
}
expect_rejected() { # expect_rejected <what> <cache-dir>
  if "$verify" verify "$2" > "$work/log" 2>&1; then
    echo "accepted: $1" >&2
    exit 1
  fi
}

repo="$work/repo"
make_image resolute "$fresh"
make_repo "$repo" trusted "$fresh"
expect_ok "a signed archive as new as the image's" "$repo"

# collect, as the action runs it after an install, selects the same material.
collected="$work/collected"
rm -rf "$collected" "$work/live-lists"
mkdir -p "$collected" "$work/live-lists"
cp "$repo"/*.deb "$collected/"
cp "$repo"/lists/* "$work/live-lists/"
printf 'Package: unrelated\nSHA256: 1111\n\n' > "$work/live-lists/other_Packages"
APT_LISTS_DIR="$work/live-lists" "$verify" collect "$collected"
[[ ! -e "$collected/lists/other_Packages" ]] || { echo "collect kept an index that lists none of the .debs" >&2; exit 1; }
expect_ok "a collected cache" "$collected"

make_repo "$repo" trusted "$fresh"
printf 'tampered' >> "$repo/foo_1_amd64.deb"
expect_rejected "a .deb whose bytes changed" "$repo"

make_repo "$repo" trusted "$fresh"
printf 'an unlisted package' > "$repo/evil_9_amd64.deb"
expect_rejected "an extra .deb that no index lists" "$repo"

make_repo "$repo" trusted "$fresh"
printf 'SHA256: %s\n\n' "$(printf 'an unlisted package' | sha256sum | cut -d' ' -f1)" \
  | gzip -c >> "$repo/lists/archive.invalid_dists_resolute_main_binary-amd64_Packages.lz4"
printf 'an unlisted package' > "$repo/evil_9_amd64.deb"
expect_rejected "an index changed to list an extra .deb" "$repo"

make_repo "$repo" stranger "$fresh"
expect_rejected "an InRelease signed by an untrusted key" "$repo"

make_repo "$repo" trusted "$fresh"
cp "$repo.InRelease.txt" "$repo/lists/archive.invalid_dists_resolute_InRelease"
expect_rejected "an unsigned InRelease" "$repo"

make_repo "$repo" trusted "$fresh"
rm -rf "$repo/lists"
expect_rejected "a cache without any index" "$repo"

# Rollback: a signed snapshot older than the image's own InRelease. The pairing
# uses the signed Origin and Suite, so renaming the cached file changes nothing.
make_repo "$repo" trusted "$old"
expect_rejected "a signed snapshot older than the image's InRelease" "$repo"
mv "$repo/lists/archive.invalid_dists_resolute_InRelease" "$repo/lists/some_other_name_InRelease"
expect_rejected "the same old snapshot under another file name" "$repo"
mv "$repo/lists/some_other_name_InRelease" "$repo/lists/image.invalid_dists_resolute_InRelease"
expect_rejected "the same old snapshot under the image's own file name" "$repo"
make_repo "$repo" trusted "$fresh"
mv "$repo/lists/archive.invalid_dists_resolute_InRelease" "$repo/lists/some_other_name_InRelease"
expect_ok "a current snapshot under another file name" "$repo"

# Without a trusted counterpart in the image there is no baseline.
make_repo "$repo" trusted "$fresh" resolute-updates
expect_rejected "a suite the image has no InRelease for" "$repo"
make_image resolute-updates "$fresh" stranger
expect_rejected "a baseline that does not verify against the keyring" "$repo"
rm -rf "$image"
make_repo "$repo" trusted "$fresh"
expect_rejected "an image without any InRelease" "$repo"

# The newest image InRelease of a suite is the baseline, whatever their names.
make_image resolute "$old"
cp "$image/image.invalid_dists_resolute_InRelease" "$work/old-baseline"
make_image resolute "$(date -u -R -d '10 days ago')"
mv "$image/image.invalid_dists_resolute_InRelease" "$image/newer_InRelease"
cp "$work/old-baseline" "$image/older_InRelease"
make_repo "$repo" trusted "$(date -u -R -d '30 days ago')"
expect_rejected "a snapshot older than the newest of the image's InRelease files" "$repo"
make_repo "$repo" trusted "$(date -u -R -d '5 days ago')"
expect_ok "a snapshot newer than the newest of the image's InRelease files" "$repo"

make_repo "$repo" trusted "$fresh"
APT_VERIFY_KEYRINGS="$work/does-not-exist.gpg"
export APT_VERIFY_KEYRINGS
expect_rejected "a runner without the archive keyring" "$repo"
echo "apt cache verification tests passed"
