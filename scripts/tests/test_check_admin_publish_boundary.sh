#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
gate="$PWD/scripts/check-admin-publish-boundary.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/admin-publish-boundary.XXXXXX")
trap 'rm -rf "$work"' EXIT
core="$work/crates/evorch-core"
cap="$core/src/admin_composition/capability.rs"
mkdir -p "$core/src/admin_composition" "$core/tests" "$work/crates/workbench-host/src/admin"

reset() {
  rm -rf "$core/src/extra.rs" "$core/tests/extra.rs" "$core/src/main.rs" "$core/src/planned_entry.rs"
  cat > "$cap" <<'RS'
const MANAGEMENT_PUBLISHED: bool = false;
fn publish(admin: &AdminService) {
    admin.enable_management();
}
#[cfg(test)]
mod tests {
    fn on(admin: &AdminService) {
        admin.enable_management();
        let _ = MANAGEMENT_PUBLISHED;
    }
}
RS
  printf 'pub fn enable_management(&self) {}\n' > "$work/crates/workbench-host/src/admin/service.rs"
  printf '// admin.enable_management(); never called here\nfn main() { evorch_core::admin_composition::install(&inputs); }\n' > "$core/src/main.rs"
}
rejects() {
  if bash "$gate" "$work" >/dev/null; then echo "$1 accepted"; exit 1; fi
}

reset
bash "$gate" "$work" >/dev/null

# Test code and comments may call the switch.
printf 'fn t(a: &AdminService) { a.enable_management(); }\n' > "$core/tests/extra.rs"
bash "$gate" "$work" >/dev/null

reset
printf 'fn go(a: &AdminService) { a.enable_management(); }\n' > "$core/src/extra.rs"
rejects 'a second product enable_management call'

reset
printf '#[cfg(test)]\nfn only_one_item(a: &AdminService) { a.enable_management(); }\n' > "$core/src/extra.rs"
rejects 'a cfg(test) item that is not a trailing test module'

reset
sed -i 's/= false;/= true;/' "$cap"
rejects 'MANAGEMENT_PUBLISHED flipped without the expected value'

reset
printf 'fn read() -> bool { MANAGEMENT_PUBLISHED }\n' > "$core/src/extra.rs"
rejects 'MANAGEMENT_PUBLISHED used outside capability.rs'

reset
printf 'fn read() { let _ = std::env::var("EVORCH_ENABLE_MANAGEMENT"); }\n' > "$core/src/extra.rs"
rejects 'an environment key for publication'

reset
sed -i '/admin.enable_management();/d' "$cap"
sed -i 's/^fn publish.*/fn publish() {/' "$cap"
rejects 'a missing enable_management call'

reset
printf 'fn go() { crate::admin_composition::install(&inputs); }\n' > "$core/src/extra.rs"
rejects 'install referenced from another file'

reset
printf 'fn a() { evorch_core::admin_composition::install(&x); }\n' > "$core/src/planned_entry.rs"
bash "$gate" "$work" >/dev/null
printf 'fn b() { evorch_core::admin_composition::install(&y); }\n' >> "$core/src/planned_entry.rs"
rejects 'install referenced twice in one entrypoint'
echo 'Admin publication boundary fixtures passed'
