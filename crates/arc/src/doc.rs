//! Static, rustdoc-style documentation generation for Argon libraries.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use arcstr::Substr;
use argonc::{
    WorkspaceConfig,
    ast::{ArgDecl, Decl, ModPath, TySpec, TySpecKind},
    parse::{self, AnnotatedParseAst, ParseMetadata, WorkspaceParseAst},
};

use crate::Library;

const STYLE: &str = r#":root {
  color-scheme: light dark;
  --bg: #fbfaff;
  --panel: #ffffff;
  --text: #262231;
  --muted: #6e687b;
  --line: #e4dff0;
  --accent: #7147b8;
  --accent-soft: #f0e9fb;
  --code: #f5f1fa;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #17141d;
    --panel: #1e1a27;
    --text: #eee9f5;
    --muted: #aaa1b8;
    --line: #393143;
    --accent: #c09aef;
    --accent-soft: #302441;
    --code: #282131;
  }
}
* { box-sizing: border-box; }
html { scroll-behavior: smooth; }
body {
  margin: 0;
  color: var(--text);
  background: var(--bg);
  font: 15px/1.6 system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
.layout { display: grid; grid-template-columns: 250px minmax(0, 1fr); min-height: 100vh; }
.sidebar {
  position: sticky; top: 0; height: 100vh; overflow: auto;
  padding: 28px 22px; border-right: 1px solid var(--line); background: var(--panel);
}
.brand { display: block; color: var(--text); font-size: 18px; font-weight: 720; margin-bottom: 22px; }
.brand-mark { color: var(--accent); margin-right: 7px; }
.nav-label { color: var(--muted); font-size: 11px; font-weight: 700; letter-spacing: .09em; text-transform: uppercase; }
.module-nav { list-style: none; padding: 0; margin: 8px 0 24px; }
.module-nav a { display: block; padding: 5px 8px; border-radius: 6px; color: var(--muted); }
.module-nav a:hover, .module-nav a.current { color: var(--text); background: var(--accent-soft); text-decoration: none; }
main { width: min(980px, 100%); padding: 52px 56px 90px; }
.eyebrow, .source { color: var(--muted); font-size: 13px; }
h1 { margin: 5px 0 12px; font-size: clamp(30px, 5vw, 44px); line-height: 1.15; letter-spacing: -.025em; }
h2 { margin: 42px 0 14px; padding-bottom: 8px; border-bottom: 1px solid var(--line); font-size: 22px; }
h3 { margin: 0; font-size: 17px; }
.lead { max-width: 720px; color: var(--muted); font-size: 17px; }
.item { margin: 16px 0; padding: 18px 20px; border: 1px solid var(--line); border-radius: 10px; background: var(--panel); }
.item-head { display: flex; align-items: baseline; justify-content: space-between; gap: 16px; }
.signature { margin: 12px 0; padding: 13px 15px; overflow-x: auto; border-radius: 7px; background: var(--code); font-size: 14px; }
.kw { color: var(--accent); font-weight: 700; }
.name { color: var(--text); font-weight: 650; }
.type { color: var(--text); }
.doc { max-width: 760px; }
.doc code { padding: 1px 5px; border-radius: 4px; background: var(--code); }
.doc h4 { margin: 18px 0 5px; }
.doc p { margin: 8px 0; }
.doc ul { margin: 8px 0; padding-left: 22px; }
.empty { color: var(--muted); font-style: italic; }
.module-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(230px, 1fr)); gap: 12px; }
.module-card { display: block; padding: 16px 18px; border: 1px solid var(--line); border-radius: 9px; background: var(--panel); color: var(--text); }
.module-card:hover { border-color: var(--accent); text-decoration: none; }
.module-card span { display: block; color: var(--muted); font-size: 13px; }
table { width: 100%; border-collapse: collapse; margin: 14px 0 6px; }
th, td { padding: 7px 10px; border-bottom: 1px solid var(--line); text-align: left; }
th { color: var(--muted); font-size: 12px; text-transform: uppercase; letter-spacing: .05em; }
@media (max-width: 760px) {
  .layout { display: block; }
  .sidebar { position: static; width: 100%; height: auto; border-right: 0; border-bottom: 1px solid var(--line); }
  main { padding: 34px 22px 70px; }
  .module-nav { display: flex; flex-wrap: wrap; gap: 3px; }
}
"#;

pub struct DocReport {
    pub output: PathBuf,
    pub modules: usize,
}

struct Module<'a> {
    path: &'a ModPath,
    ast: &'a AnnotatedParseAst,
}

type TypeTargets = HashMap<String, Vec<(ModPath, String)>>;

pub fn generate(library: &Library, output: impl AsRef<Path>) -> Result<DocReport> {
    let output = output.as_ref();
    let config = WorkspaceConfig::new(&library.root)
        .with_dependencies(library.dependencies.clone())
        .with_gds_imports(library.gds.clone());
    let parsed = parse::parse_workspace_with_config(&config);
    let errors = parsed.static_errors();
    if !errors.is_empty() {
        let messages = errors
            .iter()
            .map(|error| format!("{}: {}", error.span.path.display(), error.kind))
            .collect::<Vec<_>>()
            .join("\n");
        bail!("cannot document a workspace with parse errors:\n{messages}");
    }
    let workspace = parsed.ast();
    let modules = documented_modules(library, &workspace);
    fs::create_dir_all(output).with_context(|| {
        format!(
            "could not create documentation directory '{}'",
            output.display()
        )
    })?;
    fs::write(output.join("style.css"), STYLE)
        .with_context(|| format!("could not write '{}'", output.join("style.css").display()))?;

    let type_targets = type_targets(&modules);
    let navigation = module_navigation(&modules);
    let index = render_index(library, &modules, &navigation);
    fs::write(output.join("index.html"), index)
        .with_context(|| format!("could not write '{}'", output.join("index.html").display()))?;
    for module in &modules {
        let file_name = module_file_name(module.path);
        let page = render_module(library, module, &modules, &navigation, &type_targets);
        fs::write(output.join(&file_name), page)
            .with_context(|| format!("could not write '{}'", output.join(file_name).display()))?;
    }

    Ok(DocReport {
        output: output.to_path_buf(),
        modules: modules.len(),
    })
}

fn documented_modules<'a>(library: &Library, workspace: &'a WorkspaceParseAst) -> Vec<Module<'a>> {
    workspace
        .iter()
        .filter(|(path, _)| {
            path.first()
                .is_none_or(|first| first != "std" && !library.dependencies.contains_key(first))
        })
        .map(|(path, ast)| Module { path, ast })
        .collect()
}

fn type_targets(modules: &[Module<'_>]) -> TypeTargets {
    let mut targets = HashMap::new();
    for module in modules {
        for declaration in &module.ast.ast.decls {
            if let Decl::Enum(enum_) = declaration
                && enum_.name.span.end() <= module.ast.source_text.len()
            {
                targets
                    .entry(enum_.name.name.to_string())
                    .or_insert_with(Vec::new)
                    .push((
                        module.path.clone(),
                        format!("{}#enum.{}", module_file_name(module.path), enum_.name.name),
                    ));
            }
        }
    }
    targets
}

fn module_name(path: &ModPath) -> String {
    if path.is_empty() {
        "crate".to_owned()
    } else {
        format!("crate::{}", path.join("::"))
    }
}

fn module_file_name(path: &ModPath) -> String {
    let slug = if path.is_empty() {
        "root".to_owned()
    } else {
        path.join("-")
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '-'
                }
            })
            .collect()
    };
    format!("module-{slug}.html")
}

fn module_navigation(modules: &[Module<'_>]) -> String {
    modules
        .iter()
        .map(|module| {
            format!(
                "<li><a data-module=\"{}\" href=\"{}\">{}</a></li>",
                escape(&module_name(module.path)),
                module_file_name(module.path),
                escape(&module_name(module.path))
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

fn page_shell(
    library: &Library,
    title: &str,
    current_module: Option<&str>,
    navigation: &str,
    body: &str,
) -> String {
    let navigation = current_module.map_or_else(
        || navigation.to_owned(),
        |current| {
            navigation.replace(
                &format!("data-module=\"{}\"", escape(current)),
                &format!("class=\"current\" data-module=\"{}\"", escape(current)),
            )
        },
    );
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"color-scheme\" content=\"light dark\"><title>{title} · {library}</title><link rel=\"stylesheet\" href=\"style.css\"></head><body><div class=\"layout\"><aside class=\"sidebar\"><a class=\"brand\" href=\"index.html\"><span class=\"brand-mark\">◇</span>{library}</a><div class=\"nav-label\">Modules</div><ul class=\"module-nav\">{navigation}</ul><div class=\"nav-label\">Generated by arc doc</div></aside><main>{body}</main></div></body></html>",
        title = escape(title),
        library = escape(&library.name),
    )
}

fn render_index(library: &Library, modules: &[Module<'_>], navigation: &str) -> String {
    let cards = modules
        .iter()
        .map(|module| {
            let documentation = module_doc(&module.ast.source_text);
            let summary = documentation
                .lines()
                .next()
                .unwrap_or("Module documentation");
            format!(
                "<a class=\"module-card\" href=\"{}\"><strong>{}</strong><span>{}</span></a>",
                module_file_name(module.path),
                escape(&module_name(module.path)),
                escape(summary)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let body = format!(
        "<div class=\"eyebrow\">Argon library</div><h1>{}</h1><p class=\"lead\">Generated API documentation for this library's source modules, cells, functions, and enum types.</p><h2>Modules</h2><div class=\"module-grid\">{cards}</div>",
        escape(&library.name)
    );
    page_shell(library, &library.name, None, navigation, &body)
}

fn render_module(
    library: &Library,
    module: &Module<'_>,
    modules: &[Module<'_>],
    navigation: &str,
    targets: &TypeTargets,
) -> String {
    let name = module_name(module.path);
    let relative_path = module
        .ast
        .path
        .strip_prefix(library.directory())
        .unwrap_or(&module.ast.path);
    let docs = module_doc(&module.ast.source_text);
    let child_modules = module.ast.ast.decls.iter().filter_map(|declaration| {
        let Decl::Mod(declaration) = declaration else {
            return None;
        };
        let mut child = module.path.clone();
        child.push(declaration.ident.name.to_string());
        modules
            .iter()
            .any(|candidate| candidate.path == &child)
            .then(|| {
                format!(
                    "<a class=\"module-card\" href=\"{}\"><strong>{}</strong><span>Module</span></a>",
                    module_file_name(&child),
                    escape(&module_name(&child))
                )
            })
    });
    let child_modules = child_modules.collect::<Vec<_>>().join("");

    let mut cells = Vec::new();
    let mut functions = Vec::new();
    let mut enums = Vec::new();
    for declaration in &module.ast.ast.decls {
        match declaration {
            Decl::Cell(cell) if cell.name.span.end() <= module.ast.source_text.len() => {
                cells.push(render_callable(
                    "cell",
                    cell.name.name.as_str(),
                    &cell.args,
                    None,
                    cell.span.start(),
                    cell.name.span.start(),
                    module,
                    targets,
                ));
            }
            Decl::Fn(function) if function.name.span.end() <= module.ast.source_text.len() => {
                functions.push(render_callable(
                    "fn",
                    function.name.name.as_str(),
                    &function.args,
                    function.return_ty.as_ref(),
                    function.span.start(),
                    function.name.span.start(),
                    module,
                    targets,
                ));
            }
            Decl::Enum(enum_) if enum_.name.span.end() <= module.ast.source_text.len() => {
                let variants = enum_
                    .variants
                    .iter()
                    .map(|variant| format!("<li><code>{}</code></li>", escape(&variant.name)))
                    .collect::<Vec<_>>()
                    .join("");
                enums.push(render_item(
                    "enum",
                    enum_.name.name.as_str(),
                    &format!(
                        "<span class=\"kw\">enum</span> <span class=\"name\">{}</span>",
                        escape(&enum_.name.name)
                    ),
                    enum_.name.span.start(),
                    enum_.name.span.start(),
                    module,
                    &format!("<ul>{variants}</ul>"),
                ));
            }
            _ => {}
        }
    }

    let mut body = format!(
        "<div class=\"eyebrow\">Module</div><h1>{}</h1><div class=\"source\">{}</div>{}",
        escape(&name),
        escape(&relative_path.display().to_string()),
        render_doc(&docs)
    );
    if !child_modules.is_empty() {
        body.push_str(&format!(
            "<h2>Modules</h2><div class=\"module-grid\">{child_modules}</div>"
        ));
    }
    push_section(&mut body, "Cells", cells);
    push_section(&mut body, "Functions", functions);
    push_section(&mut body, "Enums", enums);
    if child_modules.is_empty() && !body.contains("class=\"item\"") && docs.trim().is_empty() {
        body.push_str("<p class=\"empty\">This module has no documented declarations.</p>");
    }
    page_shell(library, &name, Some(&name), navigation, &body)
}

fn push_section(body: &mut String, title: &str, items: Vec<String>) {
    if !items.is_empty() {
        body.push_str(&format!("<h2>{}</h2>{}", escape(title), items.join("")));
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "all arguments describe one declaration"
)]
fn render_callable(
    kind: &str,
    name: &str,
    args: &[ArgDecl<Substr, ParseMetadata>],
    return_ty: Option<&TySpec<Substr, ParseMetadata>>,
    declaration_start: usize,
    name_start: usize,
    module: &Module<'_>,
    targets: &TypeTargets,
) -> String {
    let arguments = args
        .iter()
        .map(|argument| {
            format!(
                "{}: {}",
                escape(&argument.name.name),
                render_type(&argument.ty, module.path, targets)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let returns = return_ty.map_or_else(String::new, |ty| {
        format!(" -&gt; {}", render_type(ty, module.path, targets))
    });
    let signature = format!(
        "<span class=\"kw\">{}</span> <span class=\"name\">{}</span>({arguments}){returns}",
        escape(kind),
        escape(name)
    );
    let argument_table = (!args.is_empty()).then(|| {
        let rows = args
            .iter()
            .map(|argument| {
                format!(
                    "<tr><td><code>{}</code></td><td>{}</td></tr>",
                    escape(&argument.name.name),
                    render_type(&argument.ty, module.path, targets)
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!("<table><thead><tr><th>Argument</th><th>Type</th></tr></thead><tbody>{rows}</tbody></table>")
    });
    render_item(
        kind,
        name,
        &signature,
        declaration_start,
        name_start,
        module,
        argument_table.as_deref().unwrap_or(""),
    )
}

fn render_item(
    kind: &str,
    name: &str,
    signature: &str,
    declaration_start: usize,
    name_start: usize,
    module: &Module<'_>,
    details: &str,
) -> String {
    let docs = declaration_doc(&module.ast.source_text, declaration_start);
    let line = module.ast.source_text[..name_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    format!(
        "<article class=\"item\" id=\"{kind}.{anchor}\"><div class=\"item-head\"><h3>{name}</h3><span class=\"source\">line {line}</span></div><pre class=\"signature\"><code>{signature}</code></pre>{docs}{details}</article>",
        kind = escape(kind),
        anchor = escape(name),
        name = escape(name),
        docs = render_doc(&docs),
    )
}

fn render_type(
    ty: &TySpec<Substr, ParseMetadata>,
    current_module: &ModPath,
    targets: &TypeTargets,
) -> String {
    match &ty.kind {
        TySpecKind::Ident(ident) => {
            let name = ident.name.as_str();
            let target = targets.get(name).and_then(|candidates| {
                candidates
                    .iter()
                    .find(|(path, _)| path == current_module)
                    .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))
            });
            target.map_or_else(
                || format!("<span class=\"type\">{}</span>", escape(name)),
                |(_, href)| {
                    format!(
                        "<a class=\"type\" href=\"{}\">{}</a>",
                        escape(href),
                        escape(name)
                    )
                },
            )
        }
        TySpecKind::Seq(inner) => format!("[{}]", render_type(inner, current_module, targets)),
        TySpecKind::Tuple(items) if items.is_empty() => "()".to_owned(),
        TySpecKind::Tuple(items) => format!(
            "({},)",
            items
                .iter()
                .map(|item| render_type(item, current_module, targets))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn module_doc(source: &str) -> String {
    source
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .take_while(|line| line.trim_start().starts_with("//!"))
        .map(|line| line.trim_start().trim_start_matches("//!").trim_start())
        .collect::<Vec<_>>()
        .join("\n")
}

fn declaration_doc(source: &str, declaration_start: usize) -> String {
    let line_start = source[..declaration_start]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let mut lines = Vec::new();
    for line in source[..line_start].lines().rev() {
        let trimmed = line.trim_start();
        let Some(comment) = trimmed.strip_prefix("///") else {
            break;
        };
        lines.push(comment.trim_start());
    }
    lines.reverse();
    lines.join("\n")
}

fn render_doc(doc: &str) -> String {
    if doc.trim().is_empty() {
        return String::new();
    }
    let mut html = String::from("<div class=\"doc\">");
    let mut in_list = false;
    for line in doc.lines() {
        let line = line.trim();
        if let Some(item) = line.strip_prefix("- ") {
            if !in_list {
                html.push_str("<ul>");
                in_list = true;
            }
            html.push_str(&format!("<li>{}</li>", render_inline(item)));
            continue;
        }
        if in_list {
            html.push_str("</ul>");
            in_list = false;
        }
        if let Some(heading) = line.strip_prefix("### ") {
            html.push_str(&format!("<h4>{}</h4>", render_inline(heading)));
        } else if let Some(heading) = line.strip_prefix("## ") {
            html.push_str(&format!("<h4>{}</h4>", render_inline(heading)));
        } else if let Some(heading) = line.strip_prefix("# ") {
            html.push_str(&format!("<h4>{}</h4>", render_inline(heading)));
        } else if !line.is_empty() {
            html.push_str(&format!("<p>{}</p>", render_inline(line)));
        }
    }
    if in_list {
        html.push_str("</ul>");
    }
    html.push_str("</div>");
    html
}

fn render_inline(text: &str) -> String {
    let mut output = String::new();
    for (index, part) in text.split('`').enumerate() {
        if index % 2 == 1 {
            output.push_str(&format!("<code>{}</code>", escape(part)));
        } else {
            output.push_str(&escape(part));
        }
    }
    output
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{Library, doc};

    #[test]
    fn generates_linked_static_library_documentation() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("Argon.toml"), "name = \"demo\"\n").unwrap();
        fs::write(
            directory.path().join("lib.ar"),
            "//! Demo cells.\n/// Routing modes.\nenum Mode { Fast, Quiet, }\n/// Builds a route.\n/// # Arguments\n/// - `mode`: routing mode.\ncell route(mode: Mode) {}\n",
        )
        .unwrap();
        let library = Library::load(directory.path().join("Argon.toml")).unwrap();
        let output = directory.path().join("generated-docs");
        let report = doc::generate(&library, &output).unwrap();

        assert_eq!(report.modules, 1);
        let page = fs::read_to_string(output.join("module-root.html")).unwrap();
        assert!(page.contains("Demo cells."));
        assert!(page.contains("id=\"cell.route\""));
        assert!(page.contains("href=\"module-root.html#enum.Mode\""));
        assert!(page.contains("routing mode"));
        assert!(!page.contains("<script"));
    }
}
