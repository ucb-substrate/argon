//! Source edits for creating and semantically renaming cells.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use arcstr::Substr;
use argonc::{
    ast::{AstMetadata, Decl, Ident, ModPath, UseDecl},
    compile::{self, BUILTINS},
    parse::{self, ParseMetadata, WorkspaceParseAst},
};
use tower_lsp_server::ls_types::{Range, TextEdit, Uri};

use crate::document::Document;

const KEYWORDS: &[&str] = &[
    "as", "cell", "const", "else", "enum", "false", "fn", "for", "if", "in", "let", "match", "mod",
    "struct", "true", "use",
];

pub(crate) struct RenameCellEdit {
    pub(crate) changes: HashMap<Uri, Vec<TextEdit>>,
    pub(crate) invocation: String,
}

pub(crate) fn validate_cell_name(name: &str) -> Result<(), String> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    {
        return Err(format!("`{name}` is not a valid Argon identifier"));
    }
    if KEYWORDS.contains(&name) {
        return Err(format!("`{name}` is a reserved Argon keyword"));
    }
    if BUILTINS.contains(&name) {
        return Err(format!(
            "`{name}` is an Argon built-in and cannot name a cell"
        ));
    }
    Ok(())
}

fn declaration_name(decl: &Decl<Substr, ParseMetadata>) -> Option<&str> {
    match decl {
        Decl::Enum(decl) => Some(decl.name.name.as_str()),
        Decl::Struct(decl) => Some(decl.name.name.as_str()),
        Decl::Constant(decl) => Some(decl.name.name.as_str()),
        Decl::Cell(decl) => Some(decl.name.name.as_str()),
        Decl::Mod(decl) => Some(decl.ident.name.as_str()),
        Decl::Use(decl) => decl
            .alias
            .as_ref()
            .or_else(|| decl.path.last())
            .map(|ident| ident.name.as_str()),
        Decl::Fn(decl) => Some(decl.name.name.as_str()),
    }
}

fn ensure_name_is_available(
    workspace: &WorkspaceParseAst,
    module_path: &ModPath,
    name: &str,
) -> Result<(), String> {
    let module = workspace
        .get(module_path)
        .ok_or_else(|| "The cell's source module is no longer available".to_owned())?;
    if module
        .ast
        .decls
        .iter()
        .filter_map(declaration_name)
        .any(|existing| existing == name)
    {
        return Err(format!("`{name}` is already declared in this module"));
    }
    Ok(())
}

pub(crate) fn new_cell_edit(
    workspace: &WorkspaceParseAst,
    source_path: &Path,
    name: &str,
) -> Result<TextEdit, String> {
    validate_cell_name(name)?;
    let (module_path, module) = workspace
        .iter()
        .find(|(_, module)| module.path == source_path)
        .ok_or_else(|| "The active buffer is not part of this Argon workspace".to_owned())?;
    ensure_name_is_available(workspace, module_path, name)?;

    let source = module.source_text.as_str();
    let separator = if source.is_empty() || source.ends_with("\n\n") {
        ""
    } else if source.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let document = Document::new(&module.source_text, 0);
    let end = document.offset_to_pos(source.len());
    Ok(TextEdit {
        range: Range::new(end, end),
        new_text: format!("{separator}cell {name}() {{\n}}\n"),
    })
}

fn use_module_path(current_path: &ModPath, use_decl: &UseDecl<Substr, ParseMetadata>) -> ModPath {
    let module_parts = &use_decl.path[..use_decl.path.len().saturating_sub(1)];
    match use_decl.path.first().map(|ident| ident.name.as_str()) {
        Some("std") => vec!["std".to_owned()],
        Some("lib") => module_parts
            .iter()
            .skip(1)
            .map(|ident| ident.name.to_string())
            .collect(),
        Some(_) => current_path
            .iter()
            .cloned()
            .chain(module_parts.iter().map(|ident| ident.name.to_string()))
            .collect(),
        None => current_path.clone(),
    }
}

fn reference_module_path<S, M>(current_path: &ModPath, path: &[Ident<S, M>]) -> ModPath
where
    S: AsRef<str>,
    M: AstMetadata,
{
    if path.len() <= 1 {
        return current_path.clone();
    }
    match path[0].name.as_ref() {
        "std" => vec!["std".to_owned()],
        "lib" => path
            .iter()
            .skip(1)
            .take(path.len() - 2)
            .map(|ident| ident.name.as_ref().to_owned())
            .collect(),
        _ => current_path
            .iter()
            .cloned()
            .chain(
                path.iter()
                    .take(path.len() - 1)
                    .map(|ident| ident.name.as_ref().to_owned()),
            )
            .collect(),
    }
}

/// Names in each module that resolve to the renamed cell, mapped to the name
/// they will expose after the edit. Explicit aliases therefore map to
/// themselves, while ordinary imports propagate the new declaration name.
fn target_names(
    workspace: &WorkspaceParseAst,
    target_module_path: &ModPath,
    old_name: &str,
    new_name: &str,
) -> HashMap<ModPath, HashMap<String, String>> {
    let mut names = HashMap::from([(
        target_module_path.clone(),
        HashMap::from([(old_name.to_owned(), new_name.to_owned())]),
    )]);
    loop {
        let mut additions = Vec::new();
        for (module_path, module) in workspace {
            for use_decl in module.ast.decls.iter().filter_map(|decl| match decl {
                Decl::Use(use_decl) => Some(use_decl),
                _ => None,
            }) {
                let Some(imported_name) = use_decl.path.last() else {
                    continue;
                };
                let imported_module = use_module_path(module_path, use_decl);
                let Some(post_rename_name) = names
                    .get(&imported_module)
                    .and_then(|module_names| module_names.get(imported_name.name.as_str()))
                else {
                    continue;
                };
                let local_name = use_decl.alias.as_ref().unwrap_or(imported_name);
                let post_rename_local_name = use_decl
                    .alias
                    .as_ref()
                    .map_or_else(|| post_rename_name.clone(), |alias| alias.name.to_string());
                let already_known = names.get(module_path).is_some_and(|module_names| {
                    module_names.contains_key(local_name.name.as_str())
                });
                if !already_known {
                    additions.push((
                        module_path.clone(),
                        local_name.name.to_string(),
                        post_rename_local_name,
                    ));
                }
            }
        }
        if additions.is_empty() {
            return names;
        }
        for (module_path, old, new) in additions {
            names.entry(module_path).or_default().insert(old, new);
        }
    }
}

fn add_edit(
    workspace: &WorkspaceParseAst,
    seen: &mut HashSet<(PathBuf, usize, usize)>,
    changes: &mut HashMap<Uri, Vec<TextEdit>>,
    path: &Path,
    span: cfgrammar::Span,
    new_name: &str,
) -> Result<(), String> {
    if !seen.insert((path.to_owned(), span.start(), span.end())) {
        return Ok(());
    }
    let module = workspace
        .values()
        .find(|module| module.path == path)
        .ok_or_else(|| "A cell reference is outside the current workspace".to_owned())?;
    if span.end() > module.source_text.len() {
        return Err("Imported GDS cells and generated declarations are read-only".to_owned());
    }
    let uri = Uri::from_file_path(path)
        .ok_or_else(|| format!("Could not convert `{}` to an editor URI", path.display()))?;
    let document = Document::new(&module.source_text, 0);
    changes.entry(uri).or_default().push(TextEdit {
        range: Range::new(
            document.offset_to_pos(span.start()),
            document.offset_to_pos(span.end()),
        ),
        new_text: new_name.to_owned(),
    });
    Ok(())
}

pub(crate) fn rename_cell_edits(
    workspace: &WorkspaceParseAst,
    current_invocation: &str,
    new_name: &str,
) -> Result<RenameCellEdit, String> {
    validate_cell_name(new_name)?;

    let parsed_invocation = parse::parse_cell(current_invocation)
        .map_err(|error| format!("The open cell invocation is invalid: {error}"))?;
    let invocation_name = parsed_invocation
        .func
        .path
        .last()
        .ok_or_else(|| "The open cell invocation has no cell name".to_owned())?;

    let mut analysis_ast = workspace.clone();
    let invocation = parse::splice_cell_invocation(&mut analysis_ast, current_invocation)
        .map_err(|error| format!("The open cell invocation is invalid: {error}"))?;
    let (typed, _) = compile::static_compile(&analysis_ast)
        .ok_or_else(|| "The workspace has no root module".to_owned())?;
    let invocation_span = invocation.span();
    let target_id = typed
        .values()
        .find(|module| module.path == invocation_span.path)
        .and_then(|module| module.span2call.get(&invocation_span))
        .and_then(|call| call.metadata.0)
        .ok_or_else(|| format!("Could not resolve the open cell `{}`", invocation_name.name))?;

    let (target_module_path, target_source_path, old_name, declaration_span) = typed
        .iter()
        .find_map(|(module_path, module)| {
            module.ast.decls.iter().find_map(|decl| match decl {
                Decl::Cell(cell) if cell.metadata.1 == target_id => Some((
                    module_path.clone(),
                    cell.metadata.0.clone(),
                    cell.name.name.to_string(),
                    cell.name.span,
                )),
                _ => None,
            })
        })
        .ok_or_else(|| "The open cell is not a source-defined Argon cell".to_owned())?;

    if old_name == new_name {
        return Err(format!("The open cell is already named `{new_name}`"));
    }
    ensure_name_is_available(workspace, &target_module_path, new_name)?;
    let target_names = target_names(workspace, &target_module_path, &old_name, new_name);

    let mut changes = HashMap::new();
    let mut seen = HashSet::new();
    add_edit(
        workspace,
        &mut seen,
        &mut changes,
        &target_source_path,
        declaration_span,
        new_name,
    )?;

    // Calls carry the resolved declaration ID. Module name propagation tells
    // us whether their final segment changes or is a stable explicit alias.
    for (module_path, module) in &typed {
        let Some(source_module) = workspace.get(module_path) else {
            continue;
        };
        for call in module.span2call.values() {
            let Some(name) = call.func.path.last() else {
                continue;
            };
            let referenced_module = reference_module_path(module_path, &call.func.path);
            let renamed = target_names
                .get(&referenced_module)
                .and_then(|module_names| module_names.get(name.name.as_str()));
            if call.metadata.0 == Some(target_id)
                && name.span.end() <= source_module.source_text.len()
                && let Some(renamed) = renamed
                && renamed != name.name.as_str()
            {
                add_edit(
                    workspace,
                    &mut seen,
                    &mut changes,
                    &source_module.path,
                    name.span,
                    renamed,
                )?;
            }
        }
    }

    // Import declarations do not carry resolved IDs. The propagated name map
    // follows re-exports transitively using the type checker's path rules.
    for (module_path, module) in workspace {
        for use_decl in module.ast.decls.iter().filter_map(|decl| match decl {
            Decl::Use(use_decl) => Some(use_decl),
            _ => None,
        }) {
            let Some(name) = use_decl.path.last() else {
                continue;
            };
            let imported_module = use_module_path(module_path, use_decl);
            let renamed = target_names
                .get(&imported_module)
                .and_then(|module_names| module_names.get(name.name.as_str()));
            if let Some(renamed) = renamed
                && renamed != name.name.as_str()
            {
                add_edit(
                    workspace,
                    &mut seen,
                    &mut changes,
                    &module.path,
                    name.span,
                    renamed,
                )?;
            }
        }
    }

    let mut renamed_invocation = current_invocation.to_owned();
    let invocation_module = reference_module_path(&ModPath::new(), &parsed_invocation.func.path);
    let renamed_invocation_name = target_names
        .get(&invocation_module)
        .and_then(|module_names| module_names.get(invocation_name.name));
    if let Some(renamed_name) = renamed_invocation_name
        && renamed_name != invocation_name.name
    {
        renamed_invocation.replace_range(
            invocation_name.span.start()..invocation_name.span.end(),
            renamed_name,
        );
    }
    Ok(RenameCellEdit {
        changes,
        invocation: renamed_invocation,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use argonc::parse;

    use super::{new_cell_edit, rename_cell_edits, validate_cell_name};

    #[test]
    fn cell_names_must_be_plain_non_reserved_identifiers() {
        assert!(validate_cell_name("guard_ring_2").is_ok());
        assert!(validate_cell_name("2guard").is_err());
        assert!(validate_cell_name("guard-ring").is_err());
        assert!(validate_cell_name("cell").is_err());
        assert!(validate_cell_name("rect").is_err());
    }

    #[test]
    fn new_cells_are_separated_from_existing_source() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        fs::write(&source_path, "cell top() {}\n").unwrap();
        let workspace = parse::parse_workspace_with_std(&source_path).ast();
        let edit = new_cell_edit(&workspace, &source_path, "child").unwrap();
        assert_eq!(edit.new_text, "\ncell child() {\n}\n");
        assert!(new_cell_edit(&workspace, &source_path, "top").is_err());
    }

    #[test]
    fn rename_tracks_declarations_imports_and_resolved_calls() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        let module_path = directory.path().join("blocks.ar");
        fs::write(
            &source_path,
            "mod blocks;\nuse blocks::child;\ncell top() { child(); }\n",
        )
        .unwrap();
        fs::write(
            &module_path,
            "// child in a comment\ncell child() { let label = \"child\"; }\n",
        )
        .unwrap();
        let workspace = parse::parse_workspace_with_std(&source_path).ast();
        let rename = rename_cell_edits(&workspace, "lib::blocks::child()", "unit").unwrap();

        assert_eq!(rename.invocation, "lib::blocks::unit()");
        assert_eq!(rename.changes.values().map(Vec::len).sum::<usize>(), 3);
        assert_eq!(
            rename
                .changes
                .values()
                .flat_map(|edits| edits.iter())
                .filter(|edit| edit.new_text == "unit")
                .count(),
            3
        );
    }

    #[test]
    fn rename_preserves_explicit_local_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        let module_path = directory.path().join("blocks.ar");
        fs::write(
            &source_path,
            "mod blocks;\nuse blocks::child as placed;\ncell top() { placed(); }\n",
        )
        .unwrap();
        fs::write(&module_path, "cell child() {}\n").unwrap();
        let workspace = parse::parse_workspace_with_std(&source_path).ast();
        let rename = rename_cell_edits(&workspace, "placed()", "unit").unwrap();

        assert_eq!(rename.invocation, "placed()");
        assert_eq!(rename.changes.values().map(Vec::len).sum::<usize>(), 2);
    }

    #[test]
    fn rename_propagates_through_unaliased_reexports() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        let public_path = directory.path().join("public.ar");
        let blocks_path = directory.path().join("blocks.ar");
        fs::write(
            &source_path,
            "mod public;\nmod blocks;\nuse public::child;\ncell top() { child(); }\n",
        )
        .unwrap();
        fs::write(&public_path, "use lib::blocks::child;\n").unwrap();
        fs::write(&blocks_path, "cell child() {}\n").unwrap();
        let workspace = parse::parse_workspace_with_std(&source_path).ast();
        let rename = rename_cell_edits(&workspace, "lib::blocks::child()", "unit").unwrap();

        assert_eq!(rename.changes.values().map(Vec::len).sum::<usize>(), 4);
    }
}
