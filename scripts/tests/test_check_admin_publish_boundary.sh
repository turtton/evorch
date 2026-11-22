#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$PWD
gate="$repo/scripts/check-admin-publish-boundary.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/admin-publish-boundary.XXXXXX")
trap 'rm -rf "$work"' EXIT
core="$work/crates/evorch-core"
cap="$core/src/admin_composition/capability.rs"
service="$work/crates/workbench-host/src/admin/service.rs"

reset() {
  rm -rf "$work/crates"
  mkdir -p "$core/src/admin_composition" "$core/tests" "$(dirname "$service")"
  # The real switch file, so its pinned hash matches.
  cp "$repo/crates/evorch-core/src/admin_composition/capability.rs" "$cap"
  printf '    pub fn enable_management(&self) {\n' > "$service"
  printf 'pub mod admin_composition;\n' > "$core/src/lib.rs"
  printf 'fn main() { evorch_core::admin_composition::install(&evorch_core::admin_composition::Inputs::new(&h, &b)); }\n' > "$core/src/main.rs"
  printf 'pub fn install() {}\n' > "$core/src/admin_composition.rs"
}
accepts() {
  if ! bash "$gate" "$work" >/dev/null; then echo "$1 refused"; bash "$gate" "$work" || true; exit 1; fi
}
rejects() {
  if bash "$gate" "$work" >/dev/null; then echo "$1 accepted"; exit 1; fi
}
extra() { printf '%s\n' "$1" > "$core/src/extra.rs"; }

reset
accepts 'the baseline'

# A whole-line // comment is the only exemption.
reset; extra '// admin.enable_management(); MANAGEMENT_PUBLISHED admin_composition'
accepts 'a whole-line comment'
reset; extra '/* admin.enable_management(); */'
rejects 'a block comment'
reset; extra '// closed */ admin.enable_management();'
rejects 'code after a comment terminator'
reset; extra '// "quoted" enable_management'
rejects 'a comment holding a quote'

# enable_management: the word, wherever it is written.
reset; extra 'fn go(a: &AdminService) { a.enable_management(); }'
rejects 'a product call'
reset; printf 'fn t(a: &AdminService) { a.enable_management(); }\n' > "$core/tests/extra.rs"
rejects 'a call from tests/'
reset; extra 'let _ = Some("fn enable_management").map(|_| admin.enable_management());'
rejects 'a call hidden behind the definition text'
reset; extra 'let name = "enable_management";'
rejects 'the word in a string'
reset; extra 'fn t(a: &AdminService) { a.publish_for_test(); }'
accepts 'the test-only entrance'
reset; printf '    pub fn enable_management(&self) {\n    pub fn enable_management(&self) {\n' > "$service"
rejects 'two definitions'
reset; : > "$service"
rejects 'no definition'

# The switch file is pinned by content.
reset; sed -i 's/= true;/= false;/' "$cap"
rejects 'the switch flipped without the pin'
reset; sed -i 's|^const MANAGEMENT_PUBLISHED.*|const MANAGEMENT_PUBLISHED: bool = false; // const MANAGEMENT_PUBLISHED: bool = true;|' "$cap"
rejects 'a false declaration behind a commented true one'
reset; printf '#[cfg(test)]\nmod tests;\nfn later(a: \&AdminService) { a.enable_management(); }\n' >> "$cap"
rejects 'production code after the test module'
reset
printf 'const /* gate\n*/ MANAGEMENT_PUBLISHED: bool = true;\n#[cfg(any())]\nconst MANAGEMENT_PUBLISHED: bool = false;\n' >> "$cap"
rejects 'a declaration split by a comment plus a cfg(any()) twin'
reset
printf 'const RAW: &str = r#"const MANAGEMENT_PUBLISHED: bool = false;"#;\n' >> "$cap"
rejects 'a declaration text inside a raw string of the switch file'
reset; rm "$cap"
rejects 'a missing switch file'

# MANAGEMENT_PUBLISHED belongs to the switch file.
reset; extra 'fn read() -> bool { MANAGEMENT_PUBLISHED }'
rejects 'a use outside the switch file'
reset; extra 'const RAW: &str = r#"const MANAGEMENT_PUBLISHED: bool = false;"#;'
rejects 'a declaration text inside a raw string elsewhere'
reset; extra 'const MANAGEMENT_PUBLISHED: bool = true;'
rejects 'a second declaration elsewhere'

# admin_composition and install.
reset; printf 'use crate::admin_composition::{Inputs, install};\nfn main() { install(inputs); }\n' > "$core/src/main.rs"
rejects 'an imported install'
reset; printf 'use crate::admin_composition::{\n    Inputs,\n    install,\n};\nfn main() { install(a); }\n' > "$core/src/planned_entry.rs"
rejects 'a multi-line import'
reset; printf 'use crate::admin_composition::install as publish_management;\n' > "$core/src/main.rs"
rejects 'an aliased install'
reset; printf 'use crate::admin_composition as ac;\nfn main() { ac::install(a); }\n' > "$core/src/main.rs"
rejects 'an aliased module'
reset; printf 'use crate::admin_composition::{self as ac};\n' > "$core/src/main.rs"
rejects 'an aliased self'
reset; extra 'use crate::admin_composition::install;'
rejects 'an import in another file'
reset; extra 'fn go() { crate::admin_composition::install(&x); }'
rejects 'a qualified call in another file'
reset; printf 'pub mod admin_composition;\npub use admin_composition::install;\n' > "$core/src/lib.rs"
rejects 'a re-export from lib.rs'
reset; printf 'fn a() { evorch_core::admin_composition::install(&x); }\n' > "$core/src/planned_entry.rs"
accepts 'one qualified install in each entrypoint'
reset; printf 'fn a() { evorch_core::admin_composition::install(&x); }\nfn b() { evorch_core::admin_composition::install(&y); }\n' > "$core/src/main.rs"
rejects 'two calls in one entrypoint'
reset; printf 'fn a() { evorch_core::admin_composition::install(&x); evorch_core::admin_composition::install(&y); }\n' > "$core/src/main.rs"
rejects 'two calls on one line'
reset; printf 'fn m(r: &Registry) { r.install(x); self::install_leaf(y); }\n' > "$core/src/main.rs"
accepts 'unrelated install methods and functions'

# No environment key.
reset; extra 'fn read() { let _ = std::env::var("EVORCH_ENABLE_MANAGEMENT"); }'
rejects 'an environment key for publication'
reset; extra 'fn read() { let _ = std::env::var("HOME"); }'
accepts 'an unrelated environment variable'
echo 'Admin publication boundary fixtures passed'
