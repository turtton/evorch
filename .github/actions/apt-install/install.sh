#!/usr/bin/env bash
# The install step of the apt-install action (see action.yml): verified cached .debs
# through dpkg, otherwise apt-get, and which .debs may be saved afterwards.
# Environment: APT_PACKAGES, APT_TIMEOUT, APT_PRIMARY_TIMEOUT, CACHE_HIT,
# GITHUB_OUTPUT (ImageVersion is the runner's).
set -u
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The defaults are the runner's; scripts/tests/test_apt_install.sh overrides them.
archives="${APT_CACHE_DIR:-$HOME/apt-archives}"
apt_archives="${APT_ARCHIVES_DIR:-/var/cache/apt/archives}"
apt_etc="${APT_ETC_DIR:-/etc/apt}"
verify="${APT_VERIFY_SCRIPT:-$here/verify-cache.sh}"

installed_ok() {
  local p
  for p in $APT_PACKAGES; do
    dpkg-query -W -f='${Status}\n' "$p" 2> /dev/null | grep -qx 'install ok installed' || return 1
  done
}

# Exactly this package set on exactly this image version: the cached
# .debs are the whole set apt had to download, so dpkg installs them
# without apt's package index or any network access. Their maintainer
# scripts run as root, so nothing is installed before verify-cache.sh has
# shown that every .deb is an Ubuntu archive package: the cached
# InRelease must verify against the runner image's own archive keyring
# (never against anything in the cache), vouch for the cached Packages
# indexes, and those must list the sha256 of each .deb. Anything else goes
# through apt, which checks cached downloads against its own signed index.
verified=false
if compgen -G "$archives/*.deb" > /dev/null; then
  if "$verify" verify "$archives"; then
    verified=true
  else
    echo "::warning::the cached .debs could not be verified against the Ubuntu archive keyring; not using them"
  fi
fi
if [ "$verified" = true ] && [ "$CACHE_HIT" = true ] && [ -n "${ImageVersion:-}" ]; then
  echo "apt-install: installing $(ls "$archives"/*.deb | wc -l) verified cached .debs with dpkg (no mirror access)"
  # The dry run checks dependencies and conflicts before any script runs.
  if sudo dpkg --dry-run -i "$archives"/*.deb && sudo dpkg -i "$archives"/*.deb && installed_ok; then
    echo "apt-install: installed from the cache"
    echo "collected=false" >> "$GITHUB_OUTPUT"
    exit 0
  fi
  echo "::warning::the verified cached .debs did not install cleanly; falling back to apt-get"
  # A failed unpack leaves dpkg half-configured.
  sudo dpkg --configure -a || true
fi

# Only verified .debs seed apt's download cache.
if [ "$verified" = true ]; then
  sudo cp "$archives"/*.deb "$apt_archives"/
fi
apt_install() {
  local limit=$1 status=0
  timeout --kill-after=30s "$limit" sudo apt-get update || status=$?
  if [ "$status" -eq 0 ]; then
    # shellcheck disable=SC2086 # word splitting is the package list
    timeout --kill-after=30s "$limit" \
      sudo apt-get install -y --no-install-recommends $APT_PACKAGES || status=$?
  fi
  return "$status"
}
status=0
apt_install "$APT_PRIMARY_TIMEOUT" || status=$?
if [ "$status" -ne 0 ]; then
  # The Azure mirror can trickle at tens of kB/s for hours while
  # archive.ubuntu.com stays fast; apt's mirror list only falls back on
  # errors, so drop the Azure entry and retry. Completed .debs stay in
  # /var/cache/apt/archives and are not downloaded again.
  echo "::warning::apt-get on the default mirror failed or timed out (status $status); retrying on archive.ubuntu.com"
  if [ -f "$apt_etc/apt-mirrors.txt" ]; then
    cat "$apt_etc/apt-mirrors.txt"
    sudo sed -i '/azure\.archive\.ubuntu\.com/d' "$apt_etc/apt-mirrors.txt"
    if ! grep -q . "$apt_etc/apt-mirrors.txt"; then
      echo 'https://archive.ubuntu.com/ubuntu/' | sudo tee "$apt_etc/apt-mirrors.txt" > /dev/null
    fi
  fi
  for sources in "$apt_etc/sources.list" "$apt_etc"/sources.list.d/*.list "$apt_etc"/sources.list.d/*.sources; do
    [ -f "$sources" ] || continue
    sudo sed -i 's#http://azure\.archive\.ubuntu\.com#https://archive.ubuntu.com#g' "$sources"
  done
  # A timeout during unpacking leaves dpkg half-configured.
  sudo dpkg --configure -a || true
  status=0
  apt_install "$APT_TIMEOUT" || status=$?
fi
if [ "$status" -ne 0 ]; then
  echo "::error::apt-get failed or timed out (status $status)"
  exit "$status"
fi
# Keep the .debs of the versions that are installed now (status exactly
# "install ok installed", not a removed package with config files left) and
# nothing else: an older leftover version of a package would make the dpkg
# install of the whole set above fail.
rm -rf "$archives"
mkdir -p "$archives"
for deb in "$apt_archives"/*.deb; do
  [ -f "$deb" ] || continue
  pkg=$(dpkg-deb -f "$deb" Package)
  ver=$(dpkg-deb -f "$deb" Version)
  if dpkg-query -W -f='${Status} ${Version}\n' "$pkg" 2> /dev/null | grep -qxF "install ok installed $ver"; then
    cp "$deb" "$archives/"
  fi
done
du -sh "$archives"
# Only a complete install whose cache passes the same check a later run
# makes is saved: a cache key cannot be rewritten, so an entry that could
# not be used would make every later run fall back to the network.
if compgen -G "$archives/*.deb" > /dev/null \
  && "$verify" collect "$archives" \
  && "$verify" verify "$archives"; then
  echo "collected=true" >> "$GITHUB_OUTPUT"
else
  echo "::warning::the installed .debs could not be matched to the signed archive indexes; not caching them"
fi
