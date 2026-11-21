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
  rm -f "$core/src/extra.rs" "$core/tests/extra.rs" "$core/src/main.rs" "$core/src/planned_entry.rs" "$core/src/lib.rs"
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
  printf 'pub mod admin_composition;\n' > "$core/src/lib.rs"
  printf '// admin.enable_management(); never called here\nfn main() { evorch_core::admin_composition::install(&inputs); }\n' > "$core/src/main.rs"
}
accepts() {
  if ! bash "$gate" "$work" >/dev/null; then echo "$1 refused"; bash "$gate" "$work" || true; exit 1; fi
}
rejects() {
  if bash "$gate" "$work" >/dev/null; then echo "$1 accepted"; exit 1; fi
}

reset
accepts 'the baseline'

# Test code and comments may call the switch.
printf 'fn t(a: &AdminService) { a.enable_management(); }\n' > "$core/tests/extra.rs"
accepts 'a call from tests/'

reset
printf 'fn go(a: &AdminService) { a.enable_management(); }\n' > "$core/src/extra.rs"
rejects 'a second product enable_management call'

reset
printf '#[cfg(test)]\nfn only_one_item(a: &AdminService) { a.enable_management(); }\n' > "$core/src/extra.rs"
rejects 'a cfg(test) item that is not a test module'

# M1: the test module ends where its braces end; code after it is production.
reset
cat >> "$cap" <<'RS'
fn later(admin: &AdminService) {
    admin.enable_management();
}
RS
rejects 'production code after the test module'

reset
cat > "$cap" <<'RS'
const MANAGEMENT_PUBLISHED: bool = false;
fn publish(admin: &AdminService) {
    admin.enable_management();
}
#[cfg(test)]
#[allow(clippy::all)]
mod tests {
    const OPEN: char = '{';
    const CLOSE: &str = "}";
    const RAW: &str = r#"} } }"#;
    /* } */
    fn on(admin: &AdminService) {
        admin.enable_management();
        if true { let _ = format!("{}", MANAGEMENT_PUBLISHED); }
    }
}
RS
accepts 'a test module whose braces sit in strings, chars and comments'
cat >> "$cap" <<'RS'
mod later {
    fn go(admin: &AdminService) {
        admin.enable_management();
    }
}
RS
rejects 'a module after a test module whose braces sit in strings'

reset
printf '#[cfg(test)]\nmod tests {\n    fn open() {\n' >> "$cap"
sed -i '/^#\[cfg(test)\]$/,$d' "$cap"
printf '#[cfg(test)]\nmod tests {\n    fn open() {\n' >> "$cap"
rejects 'a test module that never closes'

reset
printf '/* never closed\nfn x() {}\n' > "$core/src/extra.rs"
rejects 'a block comment that never closes'

# M2: only the real declaration counts, and it must be exact.
reset
sed -i 's|^const MANAGEMENT_PUBLISHED.*|const MANAGEMENT_PUBLISHED: bool = true;|' "$cap"
rejects 'MANAGEMENT_PUBLISHED flipped without the expected value'

reset
sed -i 's|^const MANAGEMENT_PUBLISHED.*|const MANAGEMENT_PUBLISHED: bool = true; // const MANAGEMENT_PUBLISHED: bool = false;|' "$cap"
rejects 'a true declaration hidden behind a commented false one'

reset
sed -i 's|^const MANAGEMENT_PUBLISHED.*|const MANAGEMENT_PUBLISHED: bool = /* false */ true;|' "$cap"
rejects 'a block comment around the value'

reset
sed -i 's|^const MANAGEMENT_PUBLISHED.*|const MANAGEMENT_PUBLISHED: bool =\n    true;|' "$cap"
rejects 'a declaration split over two lines'

reset
sed -i 's|^const MANAGEMENT_PUBLISHED.*|// const MANAGEMENT_PUBLISHED: bool = false;|' "$cap"
rejects 'a declaration that exists only in a comment'

reset
printf '#[cfg(not(any()))]\nconst MANAGEMENT_PUBLISHED: bool = false;\n' >> "$cap"
rejects 'a second declaration'

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

# M3: install is found however it is imported.
reset
printf 'use crate::admin_composition::{Inputs, install};\nfn main() { install(inputs); }\n' > "$core/src/main.rs"
accepts 'one imported install call'
printf 'fn also() { install(other); }\n' >> "$core/src/main.rs"
rejects 'two calls through an import'

reset
printf 'use crate::admin_composition::{\n    Inputs,\n    install,\n};\nfn main() { install(a); install(b); }\n' > "$core/src/planned_entry.rs"
rejects 'two calls through a multi-line import'

reset
printf 'use crate::admin_composition::install as publish_management;\nfn main() { publish_management(a); }\n' > "$core/src/main.rs"
rejects 'an aliased install'

reset
printf 'use crate::admin_composition::{Inputs, install as go};\nfn main() { go(a); }\n' > "$core/src/main.rs"
rejects 'an aliased install in a group'

reset
printf 'use crate::admin_composition as ac;\nfn main() { ac::install(a); }\n' > "$core/src/main.rs"
rejects 'an aliased module'

reset
printf 'use crate::admin_composition::{self as ac};\nfn main() { ac::install(a); }\n' > "$core/src/main.rs"
rejects 'an aliased self'

reset
printf 'use crate::admin_composition::install;\nfn go() { install(inputs); }\n' > "$core/src/extra.rs"
rejects 'an import in another file'

reset
printf 'pub mod admin_composition;\npub use admin_composition::install;\n' > "$core/src/lib.rs"
rejects 'a re-export from lib.rs'

reset
printf 'fn a() { evorch_core::admin_composition::install(&x); }\n' > "$core/src/planned_entry.rs"
accepts 'one install in each entrypoint'
printf 'fn b() { evorch_core::admin_composition::install(&y); }\n' >> "$core/src/planned_entry.rs"
rejects 'install called twice in one entrypoint'

reset
printf 'fn m(r: &Registry) { r.install(x); self::install_leaf(y); }\n' > "$core/src/main.rs"
accepts 'unrelated install methods and functions'
echo 'Admin publication boundary fixtures passed'
