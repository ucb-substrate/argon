//! Checking a library and compiling a cell invocation in it, as `argonc` does.

use std::path::PathBuf;

use arcstr::ArcStr;
use indexmap::IndexMap;

use crate::{
    WorkspaceConfig,
    compile::{self, CompileOutput, StaticErrorCompileOutput},
    diagnostics::{self, Diagnostic},
    parse::{self, CellInvocation},
};

/// Source text that replaces the file at each path; other files are read from disk.
pub type Sources = IndexMap<PathBuf, ArcStr>;

/// Parses and type-checks `workspace`, returning its errors.
pub fn check(workspace: &WorkspaceConfig, sources: &Sources) -> Vec<Diagnostic> {
    let analysis = compile::analyze_workspace(parse::parse_workspace_with_config_and_sources(
        workspace, sources,
    ));
    if analysis.typed_ast.is_none() {
        return vec![unparsed_root(workspace)];
    }
    output_diagnostics(
        &CompileOutput::StaticErrors(StaticErrorCompileOutput {
            errors: analysis.errors,
        }),
        None,
    )
}

/// Compiles the cell invocation `cell`, such as `top(10., 20.)`, returning a
/// valid output or the errors that prevent one. Without `debug_info`, the
/// output keeps only what export reads.
pub fn compile_cell(
    workspace: &WorkspaceConfig,
    sources: &Sources,
    cell: &str,
    debug_info: bool,
) -> Result<CompileOutput, Vec<Diagnostic>> {
    let fail = |message: String| vec![Diagnostic::error(message)];
    let Some(tech) = &workspace.tech else {
        return Err(fail("the library has no technology file".to_owned()));
    };
    crate::tech::read_tech(tech).map_err(|error| fail(error.to_string()))?;

    // Compiled while the workspace is parsed and analyzed.
    crate::wasm::precompile(
        workspace
            .wasm_modules
            .iter()
            .map(|(_, path)| path.clone())
            .collect(),
    );
    // The invocation is spliced into the workspace before analysis, so its
    // arguments are resolved, type-checked, and evaluated like source.
    let mut parse_output = if sources.is_empty() {
        parse::parse_workspace_without_span_indexes(workspace)
    } else {
        parse::parse_workspace_with_config_and_sources(workspace, sources)
    };
    let invocation = parse::add_cell_invocation(&mut parse_output, cell)
        .map_err(|error| fail(format!("invalid cell invocation: {error}")))?;
    // Neither analysis nor evaluation reads the span indexes, so neither the
    // parsed nor the typed AST keeps them, including the modules the
    // invocation re-parsed.
    for (ast, _) in parse_output.asts.values_mut() {
        ast.drop_span_indexes();
    }
    let analysis = compile::analyze_workspace(parse_output);
    let Some(typed_ast) = analysis.typed_ast else {
        return Err(vec![unparsed_root(workspace)]);
    };
    if !analysis.errors.is_empty() {
        let output = CompileOutput::StaticErrors(StaticErrorCompileOutput {
            errors: analysis.errors,
        });
        return Err(output_diagnostics(&output, Some(&invocation)));
    }
    // Evaluation reads only the typed AST.
    drop(analysis.ast);
    let output = if debug_info {
        compile::execute_cell_invocation(&typed_ast, &invocation, workspace)
    } else {
        compile::execute_cell_invocation_without_debug_info(&typed_ast, &invocation, workspace)
    };
    match output {
        CompileOutput::Valid(_) => Ok(output),
        _ => Err(output_diagnostics(&output, Some(&invocation))),
    }
}

/// The diagnostics for `output`, pointing into `invocation` where they land in it.
pub fn output_diagnostics(
    output: &CompileOutput,
    invocation: Option<&CellInvocation>,
) -> Vec<Diagnostic> {
    let mut diagnostics = diagnostics::from_compile_output(output);
    if let Some(invocation) = invocation {
        diagnostics::remap_invocation(&mut diagnostics, invocation);
    }
    // After remapping, so that diagnostics the invocation splice moves onto a
    // common span are deduplicated too.
    diagnostics::condense(&mut diagnostics);
    diagnostics
}

fn unparsed_root(workspace: &WorkspaceConfig) -> Diagnostic {
    Diagnostic::error(format!(
        "could not parse library root `{}`",
        workspace.root_lib.display()
    ))
}
