//! The items each `std` module declares, for naming where an unresolved name
//! lives.

use std::sync::LazyLock;

use indexmap::IndexMap;
use itertools::Itertools;

use crate::{
    ast::Decl,
    parse::{STD_MODULES, parse_std},
};

use super::Native;

/// Each item name to the full paths of the `std` items of that name, in
/// module order. Imports a module makes are not its items.
static ITEMS: LazyLock<IndexMap<String, Vec<String>>> = LazyLock::new(|| items(false));

/// As [`ITEMS`], but only the enums and native types, which a two-segment
/// path such as `DeviceKind::Nmos` can start with.
static ENUMS: LazyLock<IndexMap<String, Vec<String>>> = LazyLock::new(|| items(true));

fn items(enums_only: bool) -> IndexMap<String, Vec<String>> {
    let ast = parse_std();
    let mut items = IndexMap::<String, Vec<String>>::new();
    for module in &STD_MODULES {
        let mod_path = module.mod_path();
        let declared = ast[&mod_path].ast.decls.iter().filter_map(|decl| {
            Some(match decl {
                Decl::Enum(e) => e.name.name.to_string(),
                _ if enums_only => return None,
                Decl::Struct(s) => s.name.name.to_string(),
                Decl::Constant(c) => c.name.name.to_string(),
                Decl::Cell(c) => c.name.name.to_string(),
                Decl::Fn(f) => f.name.name.to_string(),
                Decl::Mod(_) | Decl::Use(_) => return None,
            })
        });
        // A native with no builtin is a type, never called.
        let natives = Native::in_module(&mod_path)
            .filter(|native| !enums_only || super::Builtin::from_native(*native).is_none())
            .map(|native| native.name().to_owned());
        for name in declared.chain(natives) {
            let path = module
                .module
                .iter()
                .copied()
                .chain([name.as_str()])
                .join("::");
            let paths = items.entry(name).or_default();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    items
}

/// The full paths of the `std` enums and native types named `name`.
pub(super) fn enum_paths(name: &str) -> Vec<String> {
    ENUMS.get(name).cloned().unwrap_or_default()
}

/// The full paths of the `std` items named `name`, in module order, leaving
/// out those in `except`.
pub(super) fn paths(name: &str, except: Option<&[String]>) -> Vec<String> {
    let Some(paths) = ITEMS.get(name) else {
        return Vec::new();
    };
    paths
        .iter()
        .filter(|path| {
            except.is_none_or(|module| {
                path.rsplit_once("::").is_none_or(|(prefix, _)| {
                    !module.iter().map(String::as_str).eq(prefix.split("::"))
                })
            })
        })
        .cloned()
        .collect()
}
