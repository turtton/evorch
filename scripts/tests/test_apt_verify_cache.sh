#!/usr/bin/env bash
# .github/actions/apt-install/verify-cache.sh must accept a cache entry only when
# every .deb is listed by a Packages index that an InRelease signed by a trusted
# key vouches for. The test builds a signed fake archive with throwaway keys.
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

new_key() { # new_key <name>: writes $work/<name>.gpg (binary keyring) and keeps the secret key
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

# A signed archive (the index is named .lz4 so that it goes through the helper): one InRelease vouching for one Packages index listing foo.deb.
make_repo() { # make_repo <dir> <signer> <date>
  local dir=$1 signer=$2 date=$3
  rm -rf "$dir"
  mkdir -p "$dir/lists" "$dir/debs"
  printf 'the bytes of foo' > "$dir/debs/foo_1_amd64.deb"
  printf 'the bytes of bar' > "$dir/debs/bar_2_all.deb"
  {
    for deb in foo_1_amd64 bar_2_all; do
      printf 'Package: %s\nVersion: 1\nArchitecture: amd64\nSHA256: %s\n\n' "${deb%%_*}" \
        "$(sha256sum "$dir/debs/$deb.deb" | cut -d' ' -f1)"
    done
  } > "$dir/Packages"
  local hash size
  hash=$(sha256sum "$dir/Packages" | cut -d' ' -f1)
  size=$(wc -c < "$dir/Packages")
  printf 'Origin: Ubuntu\nSuite: resolute\nDate: %s\nSHA256:\n %s %s main/binary-amd64/Packages\n 0000 1 main/binary-amd64/Packages.xz\n' \
    "$date" "$hash" "$size" > "$dir/InRelease.txt"
  gpg --batch --quiet --local-user "$signer@example.invalid" --clearsign --output "$dir/lists/archive.invalid_dists_resolute_InRelease" "$dir/InRelease.txt"
  gzip -c "$dir/Packages" > "$dir/lists/archive.invalid_dists_resolute_main_binary-amd64_Packages.lz4"
  cp "$dir"/debs/*.deb "$dir/"
}
fresh=$(date -u -R)

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
make_repo "$repo" trusted "$fresh"
expect_ok "a signed, fresh archive" "$repo"

# collect, as the action runs it after an install, selects the same material.
collected="$work/collected"
rm -rf "$collected"
mkdir -p "$collected"
cp "$repo"/debs/*.deb "$collected/"
mkdir "$work/live-lists"
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
cp "$repo/InRelease.txt" "$repo/lists/archive.invalid_dists_resolute_InRelease"
expect_rejected "an unsigned InRelease" "$repo"

make_repo "$repo" trusted "$(date -u -R -d '200 days ago')"
expect_rejected "an old signed snapshot" "$repo"

make_repo "$repo" trusted "$fresh"
rm -rf "$repo/lists"
expect_rejected "a cache without any index" "$repo"

make_repo "$repo" trusted "$fresh"
APT_VERIFY_KEYRINGS="$work/does-not-exist.gpg"
export APT_VERIFY_KEYRINGS
expect_rejected "a runner without the archive keyring" "$repo"
echo "apt cache verification tests passed"
