//! References to internal workspace crates in a Rust source file.
//!
//! The core-client desktop reaches the core only through the protocol (ADR 0045).
//! This reads a file with `syn` and counts how often it names an internal crate
//! (`storage`, `runtime`, ...), so the form of the reference does not matter:
//!
//! * `use` trees are expanded and each root is resolved: `use storage::Db;`,
//!   `use ::storage::Db;`, `use r#storage::Db;`, `use runtime as rt;`,
//!   `use {serde, sandbox as sb};`, `extern crate providers;`.
//! * Paths in expressions, types, patterns and attributes are resolved the same
//!   way, and a path through an alias counts (`use runtime as rt; rt::X`).
//! * A name the module (or block) defines or imports from elsewhere shadows a
//!   crate of that name inside that scope only: `mod storage {} use storage::X;`
//!   is local, and a child module's `storage` does not hide the crate outside it.
//!   A leading `::` always means the crate.
//! * A generic parameter hides a crate of the same name in its item (`fn f<storage: Tr>`).
//! * The tokens of a macro invocation or definition are scanned for paths that
//!   start at a crate, without expanding the macro. The scan is conservative: a
//!   token sequence that looks like a path from a crate counts even when the macro
//!   uses it as something else. A macro metavariable (`$storage::x`), `$crate::` and
//!   the arguments of text macros (`stringify!`, `concat!`, `include_str!`, ...) are
//!   not crate names, and what a macro expands to is out of scope.
//! * Only the files that are given are read. A module in a file that is not a `.rs`
//!   file (`#[path = "x.txt"] mod m;`, `include!`) is not followed. The command line
//!   reads every `.rs` file under the root, so a module file of the tree is covered.
//! * The check finds references written by mistake. It does not try to catch a
//!   reference that is hidden on purpose.
//!
//! One reference is counted for each root crate a `use` declaration names, and one
//! for each other path (two or more segments) that starts at an internal crate.

use std::collections::{BTreeSet, HashMap};

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::ext::IdentExt;
use syn::visit::{self, Visit};
use syn::{
    Block, File, GenericParam, Generics, ImplItemFn, ImplItemType, Item, ItemEnum, ItemExternCrate,
    ItemFn, ItemImpl, ItemMod, ItemStruct, ItemTrait, ItemType, ItemUnion, ItemUse, MetaList, Path,
    Stmt, TraitItemFn, TraitItemType, UseTree,
};

/// The crates the core-client desktop must not name. `browser` is left out: it is
/// also the name of a feature module there.
pub const INTERNAL_CRATES: &[&str] = &[
    "storage",
    "runtime",
    "sandbox",
    "tools",
    "providers",
    "routing",
    "config",
    "eval",
    "event_bus",
    "recorder",
    "agents",
    "catalog",
    "browser_cef",
    "model",
    "secret_guard",
    "workbench_host",
];

/// One reference, described by the path as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub description: String,
}

/// The references `source` makes to the crates in `internal`.
///
/// # Errors
/// The source is not valid Rust: a file that cannot be read is never counted as
/// free of references.
pub fn references(source: &str, internal: &BTreeSet<String>) -> Result<Vec<Reference>, syn::Error> {
    let file = syn::parse_file(source)?;
    let mut checker = Checker {
        internal,
        stack: Vec::new(),
        found: Vec::new(),
    };
    checker.visit_file(&file);
    Ok(checker.found)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    /// Something defined or imported that is not an internal crate: it shadows a
    /// crate of the same name in this scope.
    Local,
    /// A name that stands for an internal crate (`use runtime as rt;`).
    Crate(String),
}

/// The names one module, block or generic parameter list defines.
#[derive(Default)]
struct Scope {
    /// Items, extern crates and generic parameters.
    defs: HashMap<String, Binding>,
    /// Names a `use` declaration imports, with the address of that declaration:
    /// the roots of a `use` are not resolved through the names it introduces
    /// itself (`use tools::tools;`, `use storage::Database as storage;`).
    imports: Vec<(usize, String, Binding)>,
}

impl Scope {
    fn new() -> Self {
        Self::default()
    }

    fn define(&mut self, name: String, binding: Binding) {
        self.defs.insert(name, binding);
    }

    fn import(&mut self, owner: usize, name: String, binding: Binding) {
        self.imports.push((owner, name, binding));
    }

    fn get(&self, name: &str, exclude: Option<usize>) -> Option<&Binding> {
        self.defs.get(name).or_else(|| {
            self.imports
                .iter()
                .rev()
                .find(|(owner, bound, _)| bound == name && Some(*owner) != exclude)
                .map(|(_, _, binding)| binding)
        })
    }
}

fn address(item: &ItemUse) -> usize {
    std::ptr::from_ref(item).addr()
}

/// Keywords that start a path in the token stream of a macro: what follows them
/// is not the continuation of a path. `crate`, `self`, `super` and `Self` are
/// path segments and are not listed.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "trait", "type", "unsafe", "use", "where", "while", "yield",
];

/// Macros whose arguments are text, not paths.
const TEXT_MACROS: &[&str] = &[
    "stringify",
    "concat",
    "include_str",
    "include_bytes",
    "include",
    "env",
    "option_env",
    "compile_error",
];

struct Checker<'a> {
    internal: &'a BTreeSet<String>,
    /// Innermost last. A module starts a new stack, a block adds to it.
    stack: Vec<Scope>,
    found: Vec<Reference>,
}

/// One imported name of a `use` declaration.
struct Leaf {
    /// The path from the declaration's root, without the last `as`.
    path: Vec<String>,
    /// The name it is imported as: the `as` name, or the last segment.
    bound: Option<String>,
    glob: bool,
}

fn name_of(ident: &syn::Ident) -> String {
    ident.unraw().to_string()
}

fn flatten(tree: &UseTree, prefix: &[String], out: &mut Vec<Leaf>) {
    match tree {
        UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(name_of(&path.ident));
            flatten(&path.tree, &next, out);
        }
        UseTree::Name(name) => {
            let mut path = prefix.to_vec();
            let ident = name_of(&name.ident);
            // `use a::{self}` imports `a`.
            let bound = if ident == "self" && !path.is_empty() {
                path.last().cloned()
            } else {
                path.push(ident.clone());
                Some(ident)
            };
            out.push(Leaf {
                path,
                bound,
                glob: false,
            });
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.to_vec();
            let ident = name_of(&rename.ident);
            if !(ident == "self" && !path.is_empty()) {
                path.push(ident);
            }
            let bound = name_of(&rename.rename);
            out.push(Leaf {
                path,
                bound: (bound != "_").then_some(bound),
                glob: false,
            });
        }
        UseTree::Glob(_) => out.push(Leaf {
            path: prefix.to_vec(),
            bound: None,
            glob: true,
        }),
        UseTree::Group(group) => {
            for item in &group.items {
                flatten(item, prefix, out);
            }
        }
    }
}

fn leaves(item: &ItemUse) -> Vec<Leaf> {
    let mut out = Vec::new();
    flatten(&item.tree, &[], &mut out);
    out
}

impl Checker<'_> {
    /// The internal crate a path starting with `name` names, if any.
    fn resolve(&self, name: &str, absolute: bool) -> Option<String> {
        self.resolve_in(None, name, absolute, None)
    }

    /// `inner` is a scope that is still being built and is consulted before the
    /// stack. The names the `use` declaration at `exclude` introduces are not
    /// consulted: its own roots are resolved without them.
    fn resolve_in(
        &self,
        inner: Option<&Scope>,
        name: &str,
        absolute: bool,
        exclude: Option<usize>,
    ) -> Option<String> {
        if absolute {
            return self.internal.contains(name).then(|| name.to_owned());
        }
        if matches!(name, "crate" | "self" | "super" | "Self") {
            return None;
        }
        for scope in inner.into_iter().chain(self.stack.iter().rev()) {
            match scope.get(name, exclude) {
                Some(Binding::Local) => return None,
                Some(Binding::Crate(root)) => return Some(root.clone()),
                None => {}
            }
        }
        self.internal.contains(name).then(|| name.to_owned())
    }

    fn hit(&mut self, description: String) {
        self.found.push(Reference { description });
    }

    /// The names the items of one module or block define or import. Items are
    /// visible throughout their scope, so this runs before the scope is read.
    fn collect<'i>(&self, items: impl IntoIterator<Item = &'i Item>) -> Scope {
        let items: Vec<&Item> = items.into_iter().collect();
        let mut scope = Scope::new();
        for item in &items {
            match item {
                Item::Mod(m) => scope.define(name_of(&m.ident), Binding::Local),
                Item::Struct(i) => scope.define(name_of(&i.ident), Binding::Local),
                Item::Enum(i) => scope.define(name_of(&i.ident), Binding::Local),
                Item::Union(i) => scope.define(name_of(&i.ident), Binding::Local),
                Item::Trait(i) => scope.define(name_of(&i.ident), Binding::Local),
                Item::TraitAlias(i) => scope.define(name_of(&i.ident), Binding::Local),
                Item::Type(i) => scope.define(name_of(&i.ident), Binding::Local),
                Item::ExternCrate(c) => {
                    let name = name_of(&c.ident);
                    let bound = c.rename.as_ref().map_or(name.clone(), |(_, r)| name_of(r));
                    let binding = if self.internal.contains(&name) {
                        Binding::Crate(name)
                    } else {
                        Binding::Local
                    };
                    scope.define(bound, binding);
                }
                _ => {}
            }
        }
        // Imports last: whether one names an internal crate depends on the names
        // the scope defines itself.
        for item in &items {
            let Item::Use(item_use) = item else { continue };
            for leaf in leaves(item_use) {
                let Some(bound) = &leaf.bound else { continue };
                if leaf.glob {
                    continue;
                }
                let absolute = item_use.leading_colon.is_some();
                let owner = address(item_use);
                let root = leaf
                    .path
                    .first()
                    .and_then(|first| self.resolve_in(Some(&scope), first, absolute, Some(owner)));
                let binding = match root {
                    // `use runtime as rt;` imports the crate itself.
                    Some(root) if leaf.path.len() == 1 => Binding::Crate(root),
                    _ => Binding::Local,
                };
                scope.import(owner, bound.clone(), binding);
            }
        }
        scope
    }

    /// Visits with the type and const parameters of `generics` in scope: a
    /// parameter named like a crate hides it.
    fn with_generics(&mut self, generics: &Generics, visit: impl FnOnce(&mut Self)) {
        let mut scope = Scope::new();
        for param in &generics.params {
            match param {
                GenericParam::Type(p) => scope.define(name_of(&p.ident), Binding::Local),
                GenericParam::Const(p) => scope.define(name_of(&p.ident), Binding::Local),
                GenericParam::Lifetime(_) => {}
            }
        }
        self.stack.push(scope);
        visit(self);
        self.stack.pop();
    }

    /// Paths written in the tokens of a macro invocation or definition.
    fn scan(&mut self, tokens: TokenStream) {
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        self.scan_trees(&trees);
    }

    fn scan_trees(&mut self, trees: &[TokenTree]) {
        let punct = |i: usize, c: char| -> bool {
            matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == c)
        };
        let double_colon = |i: usize| -> bool {
            matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == ':' && p.spacing() == Spacing::Joint)
                && punct(i + 1, ':')
        };
        let mut skip: Option<usize> = None;
        for (i, tree) in trees.iter().enumerate() {
            match tree {
                TokenTree::Group(_) if skip == Some(i) => {}
                TokenTree::Group(group) => {
                    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                    self.scan_trees(&inner);
                }
                TokenTree::Ident(ident)
                    if TEXT_MACROS.contains(&ident.to_string().as_str())
                        && punct(i + 1, '!')
                        && matches!(trees.get(i + 2), Some(TokenTree::Group(_))) =>
                {
                    skip = Some(i + 2);
                }
                TokenTree::Ident(ident) if double_colon(i + 1) => {
                    let name = ident.unraw().to_string();
                    let mut absolute = false;
                    if i >= 2 && double_colon(i - 2) {
                        // `a::name` continues a path; `x = ::name::y` and
                        // `-> ::name::Y` start one.
                        let before = i.checked_sub(3).and_then(|j| trees.get(j));
                        let arrow =
                            i >= 4 && punct(i - 3, '>') && (punct(i - 4, '-') || punct(i - 4, '='));
                        let continues = !arrow
                            && match before {
                                Some(TokenTree::Ident(id)) => {
                                    !KEYWORDS.contains(&id.to_string().as_str())
                                }
                                Some(TokenTree::Punct(p)) => p.as_char() == '>',
                                Some(TokenTree::Group(g)) => {
                                    // `pub(crate) ::name` starts a path.
                                    let after_pub = i.checked_sub(4).is_some_and(|j| {
                                        matches!(trees.get(j), Some(TokenTree::Ident(id)) if id == "pub")
                                    });
                                    !after_pub
                                        && matches!(
                                            g.delimiter(),
                                            Delimiter::Parenthesis | Delimiter::Bracket
                                        )
                                }
                                _ => false,
                            };
                        if continues {
                            continue;
                        }
                        absolute = true;
                    } else if i >= 1 && (punct(i - 1, '$') || punct(i - 1, '.')) {
                        // A metavariable (`$name::x`), `$crate::`, a method turbofish.
                        continue;
                    }
                    if let Some(root) = self.resolve(&name, absolute) {
                        self.hit(format!("path from {root} in a macro"));
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Checker<'_> {
    fn visit_file(&mut self, file: &'ast File) {
        let scope = self.collect(&file.items);
        self.stack.push(scope);
        visit::visit_file(self, file);
        self.stack.pop();
    }

    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        // A module sees its own names, not those of the module around it.
        if let Some((_, items)) = &item.content {
            let scope = self.collect(items);
            let outer = std::mem::replace(&mut self.stack, vec![scope]);
            visit::visit_item_mod(self, item);
            self.stack = outer;
        } else {
            visit::visit_item_mod(self, item);
        }
    }

    fn visit_block(&mut self, block: &'ast Block) {
        let items = block.stmts.iter().filter_map(|stmt| match stmt {
            Stmt::Item(item) => Some(item),
            _ => None,
        });
        let scope = self.collect(items);
        self.stack.push(scope);
        visit::visit_block(self, block);
        self.stack.pop();
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        let absolute = item.leading_colon.is_some();
        let mut roots = BTreeSet::new();
        for leaf in leaves(item) {
            if let Some(first) = leaf.path.first()
                && let Some(root) = self.resolve_in(None, first, absolute, Some(address(item)))
            {
                roots.insert(root);
            }
        }
        for root in roots {
            self.hit(format!("use of {root}"));
        }
        visit::visit_item_use(self, item);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast ItemExternCrate) {
        let name = name_of(&item.ident);
        if self.internal.contains(&name) {
            self.hit(format!("extern crate {name}"));
        }
        visit::visit_item_extern_crate(self, item);
    }

    fn visit_path(&mut self, path: &'ast Path) {
        if path.segments.len() >= 2
            && let Some(first) = path.segments.first()
            && let Some(root) = self.resolve(&name_of(&first.ident), path.leading_colon.is_some())
        {
            self.hit(format!("path from {root}"));
        }
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.visit_path(&mac.path);
        let text = mac
            .path
            .segments
            .last()
            .is_some_and(|segment| TEXT_MACROS.contains(&name_of(&segment.ident).as_str()));
        if !text {
            self.scan(mac.tokens.clone());
        }
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.with_generics(&item.sig.generics, |this| visit::visit_item_fn(this, item));
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        self.with_generics(&item.sig.generics, |this| {
            visit::visit_impl_item_fn(this, item)
        });
    }

    fn visit_trait_item_fn(&mut self, item: &'ast TraitItemFn) {
        self.with_generics(&item.sig.generics, |this| {
            visit::visit_trait_item_fn(this, item)
        });
    }

    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        self.with_generics(&item.generics, |this| visit::visit_item_struct(this, item));
    }

    fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
        self.with_generics(&item.generics, |this| visit::visit_item_enum(this, item));
    }

    fn visit_item_union(&mut self, item: &'ast ItemUnion) {
        self.with_generics(&item.generics, |this| visit::visit_item_union(this, item));
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        self.with_generics(&item.generics, |this| visit::visit_item_trait(this, item));
    }

    fn visit_item_type(&mut self, item: &'ast ItemType) {
        self.with_generics(&item.generics, |this| visit::visit_item_type(this, item));
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        self.with_generics(&item.generics, |this| visit::visit_item_impl(this, item));
    }

    fn visit_impl_item_type(&mut self, item: &'ast ImplItemType) {
        self.with_generics(&item.generics, |this| {
            visit::visit_impl_item_type(this, item)
        });
    }

    fn visit_trait_item_type(&mut self, item: &'ast TraitItemType) {
        self.with_generics(&item.generics, |this| {
            visit::visit_trait_item_type(this, item)
        });
    }

    fn visit_meta_list(&mut self, list: &'ast MetaList) {
        self.visit_path(&list.path);
        self.scan(list.tokens.clone());
    }
}

#[cfg(test)]
mod tests;
