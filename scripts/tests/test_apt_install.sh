#!/usr/bin/env bash
# .github/actions/apt-install/install.sh with fake sudo, dpkg, apt-get, dpkg-query,
# dpkg-deb and verify-cache.sh: what it installs through which route, and what it
# lets the action save afterwards. The failure paths are the point of this test:
# a failed dpkg install, a failed apt-get install, a cache that does not verify,
# and installed-state filtering of what is saved.
set -euo pipefail
cd "$(dirname "$0")/../.."
install_sh="$PWD/.github/actions/apt-install/install.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
bin="$work/bin"
mkdir "$bin"

# sudo runs only what the scenarios fake or copy; it never edits the host's apt
# configuration (sed -i and tee are logged and skipped, or fail on request).
cat > "$bin/sudo" <<'EOF'
#!/usr/bin/env bash
echo "sudo $*" >> "$FAKE_LOG"
case "$1" in sed | tee) [[ -z "${FAKE_SED_FAIL:-}" ]]; exit ;; esac
exec "$@"
EOF
# rm, mkdir and cp touch the download cache only when asked to fail there.
export REAL_RM="$(command -v rm)" REAL_MKDIR="$(command -v mkdir)" REAL_CP="$(command -v cp)"
cat > "$bin/rm" <<'EOF'
#!/usr/bin/env bash
[[ -n "${FAKE_RM_FAIL:-}" && "$*" == *"$APT_CACHE_DIR"* ]] && exit 1
exec "$REAL_RM" "$@"
EOF
cat > "$bin/mkdir" <<'EOF'
#!/usr/bin/env bash
[[ -n "${FAKE_MKDIR_FAIL:-}" && "$*" == *"$APT_CACHE_DIR"* ]] && exit 1
exec "$REAL_MKDIR" "$@"
EOF
cat > "$bin/cp" <<'EOF'
#!/usr/bin/env bash
last="${@: -1}"
[[ -n "${FAKE_CP_FAIL:-}" && "$last" == "$APT_CACHE_DIR/" && "$*" == *"$FAKE_CP_FAIL"* ]] && exit 1
[[ -n "${FAKE_SEED_CP_FAIL:-}" && "$last" == "$APT_ARCHIVES_DIR/" ]] && exit 1
exec "$REAL_CP" "$@"
EOF
cat > "$bin/dpkg" <<'EOF'
#!/usr/bin/env bash
echo "dpkg $*" >> "$FAKE_LOG"
case "$1" in
  --dry-run) [[ -z "${FAKE_DRYRUN_FAIL:-}" ]] ;;
  -i) [[ -z "${FAKE_DPKG_FAIL:-}" ]] ;;
  *) exit 0 ;;
esac
EOF
cat > "$bin/apt-get" <<'EOF'
#!/usr/bin/env bash
echo "apt-get $*" >> "$FAKE_LOG"
case "$1" in
  update)
    if [[ -n "${FAKE_APT_UPDATE_FAIL_ONCE:-}" && ! -e "$FAKE_STATE/update-failed" ]]; then
      : > "$FAKE_STATE/update-failed"
      exit 100
    fi
    exit 0
    ;;
  install)
    [[ -z "${FAKE_APT_INSTALL_FAIL:-}" ]] || exit 100
    for deb in ${FAKE_DOWNLOADS:-}; do printf 'deb' > "$APT_ARCHIVES_DIR/$deb"; done
    ;;
esac
EOF
# dpkg-query reads "<package> <status> <version>" rows from $FAKE_STATE/status.
cat > "$bin/dpkg-query" <<'EOF'
#!/usr/bin/env bash
pkg="${@: -1}"
row=$(awk -v p="$pkg" '$1 == p { print; exit }' "$FAKE_STATE/status") || exit 1
[[ -n "$row" ]] || exit 1
read -r _ status version <<< "$row"
status=${status//,/ }
if [[ "$*" == *'${Version}'* ]]; then echo "$status $version"; else echo "$status"; fi
EOF
# dpkg-deb -f <file> Package|Version, from <package>_<version>_<arch>.deb.
cat > "$bin/dpkg-deb" <<'EOF'
#!/usr/bin/env bash
name=$(basename "$2" .deb)
case "$3" in
  Package) echo "${name%%_*}" ;;
  Version) rest=${name#*_}; echo "${rest%%_*}" ;;
esac
EOF
chmod +x "$bin"/*

cat > "$work/verify-stub" <<'EOF'
#!/usr/bin/env bash
echo "verify $1 $2" >> "$FAKE_LOG"
case "$1" in
  verify) [[ -z "${FAKE_VERIFY_FAIL:-}" ]] ;;
  collect) [[ -z "${FAKE_COLLECT_FAIL:-}" ]] ;;
esac
EOF
chmod +x "$work/verify-stub"

# new_case <name>: a fresh state, apt cache, download cache and log; sets the env
# every case shares.
new_case() {
  case_dir="$work/$1"
  rm -rf "$case_dir"
  mkdir -p "$case_dir"/{state,apt-archives,cache,etc}
  export FAKE_STATE="$case_dir/state" FAKE_LOG="$case_dir/log" GITHUB_OUTPUT="$case_dir/output"
  export APT_CACHE_DIR="$case_dir/cache" APT_ARCHIVES_DIR="$case_dir/apt-archives" APT_ETC_DIR="$case_dir/etc"
  export APT_VERIFY_SCRIPT="$work/verify-stub" APT_PACKAGES="a" APT_TIMEOUT=60 APT_PRIMARY_TIMEOUT=60
  export ImageVersion=1 CACHE_HIT=false PATH="$bin:$PATH"
  unset FAKE_DRYRUN_FAIL FAKE_DPKG_FAIL FAKE_APT_UPDATE_FAIL_ONCE FAKE_APT_INSTALL_FAIL \
    FAKE_DOWNLOADS FAKE_VERIFY_FAIL FAKE_COLLECT_FAIL FAKE_SED_FAIL FAKE_RM_FAIL FAKE_MKDIR_FAIL \
    FAKE_CP_FAIL FAKE_SEED_CP_FAIL
  : > "$FAKE_LOG"
  : > "$GITHUB_OUTPUT"
  printf 'a install,ok,installed 1\n' > "$FAKE_STATE/status"
}
run_install() { # run_install; sets status and the combined output in $case_dir/stdout
  status=0
  "$install_sh" > "$case_dir/stdout" 2>&1 || status=$?
}
logged() { grep -q -- "$1" "$FAKE_LOG"; }
fail() { echo "FAIL ($1)" >&2; echo "--- log" >&2; cat "$FAKE_LOG" >&2; echo "--- stdout" >&2; cat "$case_dir/stdout" >&2; echo "--- output" >&2; cat "$GITHUB_OUTPUT" >&2; exit 1; }
expect_status() { [[ "$status" -eq "$1" ]] || fail "$2: exit status $status, expected $1"; }
expect_logged() { logged "$1" || fail "$2: not run: $1"; }
expect_not_logged() { ! logged "$1" || fail "$2: run although it must not be: $1"; }
expect_output() { grep -qx "$1" "$GITHUB_OUTPUT" || fail "$2: GITHUB_OUTPUT lacks $1"; }
expect_no_collected() { ! grep -q '^collected=' "$GITHUB_OUTPUT" || fail "$1: the install must not be marked for saving"; }

# 1. A verified exact cache installs through dpkg, without apt-get or a save.
new_case offline
printf 'deb' > "$case_dir/cache/a_1_amd64.deb"
CACHE_HIT=true run_install
expect_status 0 offline
expect_logged 'verify verify' offline
expect_logged 'dpkg --dry-run -i' offline
expect_logged 'dpkg -i ' offline
expect_not_logged 'apt-get' offline
expect_output 'collected=false' offline

# 2. dpkg fails: dpkg is repaired and the install goes on through apt-get.
new_case dpkg-fails
printf 'deb' > "$case_dir/cache/a_1_amd64.deb"
FAKE_DPKG_FAIL=1 FAKE_DOWNLOADS="a_1_amd64.deb" CACHE_HIT=true run_install
expect_status 0 dpkg-fails
expect_logged 'dpkg --configure -a' dpkg-fails
expect_logged 'apt-get update' dpkg-fails
expect_logged 'apt-get install' dpkg-fails
expect_output 'collected=true' dpkg-fails

# 2b. The dry run fails (conflicts or missing dependencies): no real dpkg -i at all.
new_case dry-run-fails
printf 'deb' > "$case_dir/cache/a_1_amd64.deb"
FAKE_DRYRUN_FAIL=1 FAKE_DOWNLOADS="a_1_amd64.deb" CACHE_HIT=true run_install
expect_status 0 dry-run-fails
expect_not_logged 'dpkg -i ' dry-run-fails
expect_logged 'apt-get install' dry-run-fails

# 3. The cache does not verify: nothing from it is installed or handed to apt.
new_case unverified
printf 'deb' > "$case_dir/cache/a_1_amd64.deb"
FAKE_VERIFY_FAIL=1 FAKE_DOWNLOADS="a_5_amd64.deb" CACHE_HIT=true run_install
expect_status 0 unverified
expect_not_logged 'dpkg -i ' unverified
expect_not_logged 'dpkg --dry-run' unverified
[[ ! -e "$case_dir/apt-archives/a_1_amd64.deb" ]] || fail "unverified: an unverified .deb was copied into apt's archives"
expect_logged 'apt-get install' unverified

# 4. apt-get fails on both mirrors: the step fails and nothing is marked for saving.
new_case install-fails
FAKE_APT_INSTALL_FAIL=1 run_install
expect_status 100 install-fails
expect_logged 'apt-get install' install-fails
expect_no_collected install-fails
printf 'deb' > "$case_dir/cache/a_1_amd64.deb"
FAKE_APT_INSTALL_FAIL=1 run_install
expect_status 100 install-fails-with-a-cache
expect_no_collected install-fails-with-a-cache
[[ -e "$case_dir/cache/a_1_amd64.deb" ]] || fail "install-fails: the download cache was rewritten after a failed install"

# 5. The first mirror fails: the Azure mirror is dropped and apt-get is retried.
new_case mirror-retry
printf 'http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\n' > "$case_dir/etc/apt-mirrors.txt"
FAKE_APT_UPDATE_FAIL_ONCE=1 FAKE_DOWNLOADS="a_1_amd64.deb" run_install
expect_status 0 mirror-retry
grep -q 'retrying on archive.ubuntu.com' "$case_dir/stdout" || fail "mirror-retry: no warning about the retry"
expect_logged 'sed -i /azure' mirror-retry
expect_output 'collected=true' mirror-retry

# 6. Only the .debs of installed packages are saved, at their installed version.
new_case saved-set
printf 'a install,ok,installed 1\nb deinstall,ok,config-files 2\nc install,ok,installed 3\n' > "$FAKE_STATE/status"
FAKE_DOWNLOADS="a_1_amd64.deb b_2_amd64.deb c_9_amd64.deb c_3_amd64.deb d_1_amd64.deb" run_install
expect_status 0 saved-set
expect_output 'collected=true' saved-set
saved=$(cd "$case_dir/cache" && ls *.deb | tr '\n' ' ')
[[ "$saved" == "a_1_amd64.deb c_3_amd64.deb " ]] \
  || fail "saved-set: saved '$saved'; expected only the installed versions of installed packages (not removed b, not the older c_9, not d which is not installed)"

# 7. The saved set must verify the way a later run verifies it.
new_case collect-fails
FAKE_DOWNLOADS="a_1_amd64.deb" FAKE_COLLECT_FAIL=1 run_install
expect_status 0 collect-fails
expect_no_collected collect-fails
new_case verify-after-install-fails
FAKE_DOWNLOADS="a_1_amd64.deb" FAKE_VERIFY_FAIL=1 run_install
expect_status 0 verify-after-install-fails
expect_no_collected verify-after-install-fails

# 8. Only removed packages were downloaded: there is nothing to save.
new_case nothing-installed
printf 'a deinstall,ok,config-files 1\n' > "$FAKE_STATE/status"
FAKE_DOWNLOADS="a_1_amd64.deb" run_install
expect_no_collected nothing-installed

# 9. Preparing the download cache fails after a good install: the install still
# succeeds, but nothing is marked for saving, so a stale .deb that survives a
# failed clean-up (it would pass the signature check) can never be saved.
new_case rm-fails
printf 'deb' > "$case_dir/cache/old_1_amd64.deb"
FAKE_RM_FAIL=1 FAKE_DOWNLOADS="a_1_amd64.deb" run_install
expect_status 0 rm-fails
expect_no_collected rm-fails
[[ -e "$case_dir/cache/old_1_amd64.deb" ]] || fail "rm-fails: the failed clean-up unexpectedly removed the stale .deb"
grep -q 'not caching them' "$case_dir/stdout" || fail "rm-fails: no warning that nothing is cached"
new_case mkdir-fails
FAKE_MKDIR_FAIL=1 FAKE_DOWNLOADS="a_1_amd64.deb" run_install
expect_status 0 mkdir-fails
expect_no_collected mkdir-fails
new_case cp-fails
printf 'a install,ok,installed 1\nc install,ok,installed 3\n' > "$FAKE_STATE/status"
FAKE_CP_FAIL=c_3 FAKE_DOWNLOADS="a_1_amd64.deb c_3_amd64.deb" run_install
expect_status 0 cp-fails
expect_no_collected cp-fails

# 10. Best-effort steps do not fail the install: seeding apt's download cache, and
# editing the mirror list before the retry.
new_case seed-fails
printf 'deb' > "$case_dir/cache/a_1_amd64.deb"
FAKE_SEED_CP_FAIL=1 FAKE_DOWNLOADS="a_1_amd64.deb" run_install
expect_status 0 seed-fails
expect_logged 'apt-get install' seed-fails
new_case sed-fails
printf 'http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\n' > "$case_dir/etc/apt-mirrors.txt"
FAKE_SED_FAIL=1 FAKE_APT_UPDATE_FAIL_ONCE=1 FAKE_DOWNLOADS="a_1_amd64.deb" run_install
expect_status 0 sed-fails
expect_output 'collected=true' sed-fails

# 11. The script keeps what an Actions bash step provides: -e and pipefail.
head -n 1 "$install_sh" | grep -q bash || fail "install.sh has no bash shebang"
grep -q '^set -euo pipefail$' "$install_sh" || fail "install.sh must run with set -euo pipefail"

echo "apt-install tests passed"
