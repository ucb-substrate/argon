//! The library's cells, which the GUI offers to open before a cell is open.

use argonc::{WorkspaceConfig, ast::Decl, parse::WorkspaceParseAst};

use crate::rpc::CellEntry;

/// The cells of the library's own modules, root module first and each module
/// in declaration order. `std` and dependencies are left out.
///
/// A module that does not parse keeps its entries from `previous`, so a
/// half-written edit does not hide its cells. `None` before the workspace has
/// a root module.
pub(crate) fn library_cells(
    ast: &WorkspaceParseAst,
    config: &WorkspaceConfig,
    previous: Option<&[CellEntry]>,
) -> Option<Vec<CellEntry>> {
    ast.get(&Vec::new())?;
    let mut cells = Vec::new();
    for (module, module_ast) in ast {
        if module.first().is_some_and(|first| {
            first == "std" || config.dependencies.iter().any(|(name, _)| name == first)
        }) {
            continue;
        }
        let module_path = module.join("::");
        if !module_ast.parsed {
            cells.extend(
                previous
                    .into_iter()
                    .flatten()
                    .filter(|cell| entry_module(&cell.path) == module_path)
                    .cloned(),
            );
            continue;
        }
        for decl in &module_ast.ast.decls {
            let Decl::Cell(cell) = decl else {
                continue;
            };
            let name = &cell.name.name;
            cells.push(CellEntry {
                path: if module_path.is_empty() {
                    name.to_string()
                } else {
                    format!("{module_path}::{name}")
                },
                parameters: cell
                    .args
                    .iter()
                    .map(|arg| arg.name.name.to_string())
                    .collect(),
                requires_arguments: cell.args.iter().any(|arg| arg.default.is_none()),
            });
        }
    }
    Some(cells)
}

/// The module part of a cell path, empty for the root module.
fn entry_module(path: &str) -> &str {
    path.rsplit_once("::").map_or("", |(module, _)| module)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use argonc::{WorkspaceConfig, parse};

    use super::library_cells;
    use crate::rpc::CellEntry;

    fn entry(path: &str, parameters: &[&str], requires_arguments: bool) -> CellEntry {
        CellEntry {
            path: path.to_owned(),
            parameters: parameters.iter().map(|name| (*name).to_owned()).collect(),
            requires_arguments,
        }
    }

    fn write(directory: &Path, name: &str, source: &str) {
        std::fs::write(directory.join(name), source).unwrap();
    }

    #[test]
    fn lists_library_cells_but_not_std_or_dependencies() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("library");
        let dependency = directory.path().join("dependency");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&dependency).unwrap();
        write(
            &library,
            "lib.ar",
            "mod utils;\ncell top(w: Float = 1.) {}\ncell inv(nw: Float, pw: Float = nw) {}\nfn helper() -> Float { 1. }\n",
        );
        write(&library, "utils.ar", "cell pad() {}\n");
        write(&dependency, "lib.ar", "cell nmos() {}\n");
        let config = WorkspaceConfig::new(library.join("lib.ar"))
            .with_dependencies([("devices".to_owned(), dependency)]);
        let ast = parse::parse_workspace_with_config(&config).ast();

        assert_eq!(
            library_cells(&ast, &config, None),
            Some(vec![
                entry("top", &["w"], false),
                entry("inv", &["nw", "pw"], true),
                entry("utils::pad", &[], false),
            ])
        );
    }

    #[test]
    fn a_module_that_does_not_parse_keeps_its_previous_cells() {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "lib.ar", "mod utils;\ncell top() {}\n");
        write(directory.path(), "utils.ar", "cell pad() {\n");
        let config = WorkspaceConfig::new(directory.path().join("lib.ar"));
        let ast = parse::parse_workspace_with_config(&config).ast();
        let previous = [
            entry("top", &[], false),
            entry("old", &[], false),
            entry("utils::pad", &[], false),
        ];

        assert_eq!(
            library_cells(&ast, &config, Some(&previous)),
            Some(vec![
                entry("top", &[], false),
                entry("utils::pad", &[], false)
            ])
        );
    }

    #[test]
    fn nothing_is_known_before_the_root_module_is_parsed() {
        let config = WorkspaceConfig::new("lib.ar");
        assert_eq!(library_cells(&Default::default(), &config, None), None);
    }
}
