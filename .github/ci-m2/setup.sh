#!/usr/bin/env bash
# CI benchmark (DO NOT MERGE): per-job setup. Reads the mode file, installs the
# link-time shim (a `clang` in front of PATH that logs how long each link takes,
# RUSTFLAGS and the dependency fingerprints are untouched) and records the
# cargo-related environment.
# usage: setup.sh <label> <sccache-enabled: true|false>
set -euo pipefail
label=$1
enable=$2
mode=$(sed -n 's/^mode=//p' .github/ci-m2/mode)
gen=$(sed -n 's/^gen=//p' .github/ci-m2/mode)
out="$RUNNER_TEMP/m2-out"
mkdir -p "$out" "$RUNNER_TEMP/m2-shim"
sccache=false
if [[ "$enable" == true && "$mode" != off ]]; then sccache=true; fi
{
  echo "mode=$mode"
  echo "gen=$gen"
  echo "sccache=$sccache"
} >> "$GITHUB_OUTPUT"
real=$(command -v clang || true)
if [[ -n "$real" ]]; then
cat > "$RUNNER_TEMP/m2-shim/clang" <<SHIM
#!/usr/bin/env bash
s=\$EPOCHREALTIME
$real "\$@"
rc=\$?
e=\$EPOCHREALTIME
out=""; prev=""; kind=link
for a in "\$@"; do
  [[ \$prev == -o ]] && out=\$a
  [[ \$a == -c ]] && kind=compile
  prev=\$a
done
printf '%s\t%s\t%s\t%s\n' "\$(( (\${e/./} - \${s/./}) / 1000 ))" "\$kind" "\$rc" "\$out" >> "$out/link.log"
exit \$rc
SHIM
chmod +x "$RUNNER_TEMP/m2-shim/clang"
echo "$RUNNER_TEMP/m2-shim" >> "$GITHUB_PATH"
fi
{
  echo "M2_LABEL=$label"
  echo "M2_MODE=$mode"
  echo "M2_OUT=$out"
} >> "$GITHUB_ENV"
{
  echo "label=$label mode=$mode gen=$gen sccache=$sccache"
  echo "nproc=$(nproc) mem=$(free -m | awk '/Mem:/{print $2}')MB"
  echo "real clang: $real"
  env | grep -E '^(CARGO|RUSTC|RUSTFLAGS|SCCACHE|RUST_)' | sort || true
  echo "CARGO_INCREMENTAL=${CARGO_INCREMENTAL-<unset at setup time>}"
  date -u +%FT%TZ
} | tee "$out/env.txt"
