//! Production settings editors must declare their real layered save scope.

use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

fn rust_sources(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name == "tests"
            || name.ends_with("_tests.rs")
            || name.ends_with("_test.rs")
            || name.starts_with("test_")
        {
            continue;
        }
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<syn::Path>()
                .is_ok_and(|condition| condition.is_ident("test"))
    })
}

fn item_attributes(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

#[derive(Default)]
struct SaveScopeGuard {
    violations: Vec<String>,
}

impl SaveScopeGuard {
    fn check_use_tree(&mut self, prefix: &str, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(path) => {
                self.check_use_tree(&format!("{prefix}{}::", path.ident), &path.tree);
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.check_use_tree(prefix, item);
                }
            }
            syn::UseTree::Name(name) => self.check_path(format!("{prefix}{}", name.ident)),
            syn::UseTree::Rename(rename) => self.check_path(format!("{prefix}{}", rename.ident)),
            syn::UseTree::Glob(_) => {}
        }
    }

    fn check_path(&mut self, path: String) {
        if path.contains("StandaloneSaveContext")
            || path.contains("SaveScope::Standalone")
            || path.contains("config::save_")
            || path.contains("config::save::save_")
            || path.contains("config::delete_provider")
            || path.contains("config::delete_role_profile")
            || path.contains("config::preview_role_profiles_bindings")
        {
            self.violations.push(path);
        }
    }
}

impl<'ast> Visit<'ast> for SaveScopeGuard {
    fn visit_file(&mut self, file: &'ast syn::File) {
        if !test_only(&file.attrs) {
            visit::visit_file(self, file);
        }
    }

    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !test_only(item_attributes(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attributes = match item {
            syn::ImplItem::Const(item) => &item.attrs[..],
            syn::ImplItem::Fn(item) => &item.attrs[..],
            syn::ImplItem::Type(item) => &item.attrs[..],
            syn::ImplItem::Macro(item) => &item.attrs[..],
            _ => &[],
        };
        if !test_only(attributes) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attributes = match item {
            syn::TraitItem::Const(item) => &item.attrs[..],
            syn::TraitItem::Fn(item) => &item.attrs[..],
            syn::TraitItem::Type(item) => &item.attrs[..],
            syn::TraitItem::Macro(item) => &item.attrs[..],
            _ => &[],
        };
        if !test_only(attributes) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
        let attributes = match item {
            syn::ForeignItem::Fn(item) => &item.attrs[..],
            syn::ForeignItem::Static(item) => &item.attrs[..],
            syn::ForeignItem::Type(item) => &item.attrs[..],
            syn::ForeignItem::Macro(item) => &item.attrs[..],
            _ => &[],
        };
        if !test_only(attributes) {
            visit::visit_foreign_item(self, item);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.check_use_tree("", &item.tree);
        visit::visit_item_use(self, item);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.check_path(
            path.segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        );
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // syn leaves macro bodies opaque; retain the guard for calls inside them.
        self.check_path(mac.tokens.to_string().replace(char::is_whitespace, ""));
        visit::visit_macro(self, mac);
    }
}

fn violations(source: &str) -> syn::Result<Vec<String>> {
    let file = syn::parse_file(source)?;
    let mut guard = SaveScopeGuard::default();
    guard.visit_file(&file);
    Ok(guard.violations)
}

#[test]
fn production_after_leading_test_import_or_module_is_still_checked() {
    for test_item in [
        "#[cfg(test)] use config::StandaloneSaveContext;",
        "#[cfg(test)] #[path = \"fixture_tests.rs\"] mod tests;",
        "#[cfg(test)] mod tests { fn fixture() { config::save_fixture(); } }",
    ] {
        let source = format!("{test_item}\nfn production() {{ config::save_provider(); }}");
        assert_eq!(violations(&source).unwrap(), ["config::save_provider"]);
    }
}

#[test]
fn test_items_are_skipped_without_hiding_surrounding_production_items() {
    let source = r#"
        fn before() { config::delete_provider(); }
        #[cfg(test)] fn fixture() { config::save_fixture(); }
        mod nested {
            #[cfg(test)] use config::StandaloneSaveContext;
            fn production() { config::delete_role_profile(); }
        }
        impl Editor {
            #[cfg(test)] fn fixture() { config::save_fixture(); }
            fn production() { config::preview_role_profiles_bindings(); }
        }
        trait Defaults {
            #[cfg(test)] fn fixture() { config::save_fixture(); }
            fn production() { config::save_provider(); }
        }
        unsafe extern "C" {
            #[cfg(test)] fn fixture(value: config::StandaloneSaveContext);
            fn production(value: config::StandaloneSaveContext);
        }
    "#;
    assert_eq!(
        violations(source).unwrap(),
        [
            "config::delete_provider",
            "config::delete_role_profile",
            "config::preview_role_profiles_bindings",
            "config::save_provider",
            "config::StandaloneSaveContext",
        ]
    );
}

#[test]
fn only_definitely_test_only_items_are_excluded() {
    let source = r#"
        #[cfg(not(test))] fn production() { config::save_provider(); }
        #[cfg(any(test, feature = "gui"))]
        fn possibly_production() { config::save_routing(); }
    "#;
    assert_eq!(
        violations(source).unwrap(),
        ["config::save_provider", "config::save_routing"]
    );
    assert!(
        violations("#![cfg(test)]\nfn fixture() { config::save_fixture(); }")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn production_imports_cannot_alias_standalone_save_apis() {
    let source = r#"
        #[cfg(test)] use config::StandaloneSaveContext;
        use config::{StandaloneSaveContext as Context, save_provider as save};
        use config::save::save_routing;
    "#;
    assert_eq!(
        violations(source).unwrap(),
        [
            "config::StandaloneSaveContext",
            "config::save_provider",
            "config::save::save_routing",
        ]
    );
}

#[test]
fn macro_bodies_and_spaced_paths_remain_guarded() {
    let source = r#"
        #[cfg(test)] mod tests {}
        fn production() {
            wrapper!(config :: save_provider());
            config :: StandaloneSaveContext :: new();
        }
    "#;
    let found = violations(source).unwrap();
    assert!(found.iter().any(|path| path == "config::save_provider()"));
    assert!(
        found
            .iter()
            .any(|path| path.contains("StandaloneSaveContext"))
    );
}

#[test]
fn standalone_save_entry_points_are_confined_to_explicit_config_api_adapters() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    // Only the config API itself may implement the explicit standalone contract.
    // Runtime/GUI/evaluator code edits layered user settings, not isolated files.
    let adapters = [
        "config/src/lib.rs",
        "config/src/save_context.rs",
        "config/src/save.rs",
        "config/src/save_agent_features.rs",
        "config/src/save_agents.rs",
        "config/src/save_codex.rs",
        "config/src/save_role_profiles.rs",
        "config/src/save_routing.rs",
        "config/src/save_sandbox.rs",
        "config/src/save_self_improvement.rs",
        "config/src/save_subscription.rs",
    ];
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            rust_sources(&src, &mut files);
        }
    }
    for path in files {
        let relative = path.strip_prefix(crates).unwrap().to_string_lossy();
        if adapters.contains(&relative.as_ref()) {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let found = violations(&source).unwrap_or_else(|error| {
            panic!("cannot parse {relative} for save scope guard: {error}")
        });
        assert!(
            found.is_empty(),
            "{relative} bypasses the layered save boundary via {found:?}"
        );
    }
}
