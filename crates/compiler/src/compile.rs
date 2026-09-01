//! # Argon compiler
//
//! Pass 1: import resolution
//! Pass 2: assign variable IDs/type checking
//! Pass 3: solving
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::hash::Hasher;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use arcstr::Substr;
use enumify::enumify;
use fnv::FnvHasher;
use geometry::transform::{Rotation, TransformationMatrix};
use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools};
use serde::{Deserialize, Serialize};

mod result;

pub use result::{
    CompileOutput, ExecError, ExecErrorCompileOutput, ExecErrorKind, StaticError,
    StaticErrorCompileOutput, StaticErrorKind,
};

use crate::ast::annotated::AnnotatedAst;
use crate::ast::{
    BinOp, CastExpr, ComparisonOp, ConstantDecl, EnumDecl, FieldAccessExpr, FnDecl, ForLoop,
    IdentPath, IndexExpr, IndexFieldAccessExpr, IntLiteral, KwArgValue, MatchExpr, ModPath, Scope,
    Span, TySpec, TySpecKind, UnaryOp, UnaryOpExpr, UseDecl, WorkspaceAst,
};
use crate::cancellation::CancellationToken;
use crate::gds::{ImportedGdsElement, import_gds};
use crate::parse::{CellInvocation, ParseOutput, WorkspaceParseAst};
use crate::solver::{ConstraintId, Var};
use crate::tech::{Technology, read_tech};
use crate::workspace::WorkspaceConfig;
use crate::{
    ast::{
        ArgDecl, Ast, AstMetadata, AstTransformer, BinOpExpr, CallExpr, CellDecl, ComparisonExpr,
        Decl, Expr, Ident, IfExpr, LetBinding, Statement,
    },
    parse::ParseMetadata,
    solver::{LinearExpr, Solver},
};

pub const BUILTINS: [&str; 15] = [
    "list",
    "cons",
    "head",
    "tail",
    "range_full",
    "crect",
    "rect",
    "polygon",
    "path",
    "text",
    "float",
    "eq",
    "dimension",
    "inst",
    "bbox",
];

pub fn static_compile(
    ast: &WorkspaceParseAst,
) -> Option<(WorkspaceAst<VarIdTyMetadata>, StaticErrorCompileOutput)> {
    static_compile_cancellable(ast, None).expect("uncancellable static analysis cannot cancel")
}

pub(crate) fn static_compile_cancellable(
    ast: &WorkspaceParseAst,
    cancellation: Option<&CancellationToken>,
) -> Option<Option<(WorkspaceAst<VarIdTyMetadata>, StaticErrorCompileOutput)>> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return None;
    }
    if !ast.contains_key(&vec![]) {
        return Some(None);
    }
    let (dag, mut errors) = construct_dag(ast);
    // Type checking depends on a complete topological module order. Continuing
    // with a missing module or a dependency cycle only produces misleading
    // follow-on errors from half-populated module binding frames.
    if !errors.is_empty() {
        return Some(Some((IndexMap::new(), StaticErrorCompileOutput { errors })));
    }
    let (ast, new_errors) = execute_var_id_ty_pass(ast, &dag, cancellation)?;
    errors.extend(new_errors);
    Some(Some((ast, StaticErrorCompileOutput { errors })))
}

/// Runs static analysis for a parsed workspace and folds parser diagnostics
/// into the same error collection as import and type-checking errors.
pub fn analyze_workspace(parse_output: ParseOutput) -> StaticAnalysis {
    let parse_errors = parse_output.static_errors();
    let ast = parse_output.ast();
    analyze_workspace_ast(ast, parse_errors)
}

pub(crate) fn analyze_workspace_ast(
    ast: WorkspaceParseAst,
    parse_errors: Vec<StaticError>,
) -> StaticAnalysis {
    let (typed_ast, errors) = match static_compile(&ast) {
        Some((typed_ast, mut output)) => {
            output.errors.extend(parse_errors);
            (Some(typed_ast), output.errors)
        }
        None => (None, parse_errors),
    };
    StaticAnalysis {
        ast,
        typed_ast,
        errors,
    }
}

#[derive(Clone)]
pub struct StaticAnalysis {
    /// Parsed source AST used for source-aware tooling.
    pub ast: WorkspaceParseAst,
    /// Type-annotated AST, absent only when the workspace has no root module.
    pub typed_ast: Option<WorkspaceAst<VarIdTyMetadata>>,
    /// Parser, import-resolution, and type-checking diagnostics.
    pub errors: Vec<StaticError>,
}

/// Executes a cell using workspace-wide external inputs from `config`.
pub fn execute_cell(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    input: CompileInput<'_>,
    config: &WorkspaceConfig,
) -> CompileOutput {
    execute_cell_tracked(ast, input, config).output
}

/// Executes a cell while recording the declarations actually observed at
/// runtime. The incremental compiler uses this to retain results across source
/// revisions without making the public one-shot API stateful.
pub(crate) fn execute_cell_tracked(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    input: CompileInput<'_>,
    config: &WorkspaceConfig,
) -> TrackedExecution {
    execute_cell_tracked_with_artifacts(ast, input, config, Vec::new())
}

pub(crate) fn execute_cell_tracked_with_artifacts(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    input: CompileInput<'_>,
    config: &WorkspaceConfig,
    artifacts: Vec<CellArtifact>,
) -> TrackedExecution {
    execute_cell_tracked_with_artifacts_cancellable(ast, input, config, artifacts, Vec::new(), None)
        .expect("uncancellable cell execution cannot be cancelled")
}

pub(crate) fn execute_cell_tracked_with_artifacts_cancellable(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    input: CompileInput<'_>,
    config: &WorkspaceConfig,
    artifacts: Vec<CellArtifact>,
    continuations: Vec<CellContinuation>,
    cancellation: Option<&CancellationToken>,
) -> Option<TrackedExecution> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return None;
    }
    let Some(tech_file) = config.tech.as_deref() else {
        return Some(TrackedExecution::without_dependencies(missing_tech_output()));
    };
    let tech = match read_tech(tech_file) {
        Ok(tech) => tech,
        Err(error) => {
            return Some(TrackedExecution::without_dependencies(invalid_tech_output(
                ast,
                error.to_string(),
            )));
        }
    };
    let mut execution = ExecPass::with_artifacts_cancellable(
        ast,
        tech,
        &config.gds_imports,
        artifacts,
        continuations,
        cancellation,
    )
    .execute_tracked(input);
    if execution.cancelled {
        return None;
    }
    execution.output = check_output_layers(execution.output, tech_file);
    Some(execution)
}

/// Executes a cell invocation spliced into `ast` by
/// [`crate::parse::add_cell_invocation`]. Its arguments are evaluated by the
/// ordinary expression evaluator, so they may be any expression.
pub fn execute_cell_invocation(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    invocation: &CellInvocation,
    config: &WorkspaceConfig,
) -> CompileOutput {
    execute_cell_invocation_tracked(ast, invocation, config).output
}

pub(crate) fn execute_cell_invocation_tracked(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    invocation: &CellInvocation,
    config: &WorkspaceConfig,
) -> TrackedExecution {
    execute_cell_invocation_tracked_with_artifacts(ast, invocation, config, Vec::new())
}

pub(crate) fn execute_cell_invocation_tracked_with_artifacts(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    invocation: &CellInvocation,
    config: &WorkspaceConfig,
    artifacts: Vec<CellArtifact>,
) -> TrackedExecution {
    execute_cell_invocation_tracked_with_artifacts_cancellable(
        ast,
        invocation,
        config,
        artifacts,
        Vec::new(),
        None,
    )
    .expect("uncancellable invocation execution cannot be cancelled")
}

pub(crate) fn execute_cell_invocation_tracked_with_artifacts_cancellable(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    invocation: &CellInvocation,
    config: &WorkspaceConfig,
    artifacts: Vec<CellArtifact>,
    continuations: Vec<CellContinuation>,
    cancellation: Option<&CancellationToken>,
) -> Option<TrackedExecution> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return None;
    }
    let Some(tech_file) = config.tech.as_deref() else {
        return Some(TrackedExecution::without_dependencies(missing_tech_output()));
    };
    let tech = match read_tech(tech_file) {
        Ok(tech) => tech,
        Err(error) => {
            return Some(TrackedExecution::without_dependencies(invalid_tech_output(
                ast,
                error.to_string(),
            )));
        }
    };
    let mut execution = ExecPass::with_artifacts_cancellable(
        ast,
        tech,
        &config.gds_imports,
        artifacts,
        continuations,
        cancellation,
    )
    .execute_invocation_tracked(invocation);
    if execution.cancelled {
        return None;
    }
    execution.output = check_output_layers(execution.output, tech_file);
    Some(execution)
}

pub(crate) struct TrackedExecution {
    pub(crate) output: CompileOutput,
    pub(crate) dependencies: IndexSet<VarId>,
    pub(crate) artifacts: Vec<CellArtifact>,
    pub(crate) continuation: Option<CellContinuation>,
    pub(crate) artifact_hits: u64,
    pub(crate) artifact_misses: u64,
    pub(crate) continuation_hits: u64,
    pub(crate) continuation_misses: u64,
    cancelled: bool,
}

impl TrackedExecution {
    fn without_dependencies(output: CompileOutput) -> Self {
        Self {
            output,
            dependencies: IndexSet::new(),
            artifacts: Vec::new(),
            continuation: None,
            artifact_hits: 0,
            artifact_misses: 0,
            continuation_hits: 0,
            continuation_misses: 0,
            cancelled: false,
        }
    }
}

fn missing_tech_output() -> CompileOutput {
    CompileOutput::ExecErrors(ExecErrorCompileOutput {
        errors: vec![ExecError {
            span: None,
            cell: 0,
            kind: ExecErrorKind::MissingTech,
        }],
        output: None,
    })
}

fn invalid_tech_output(ast: &WorkspaceAst<VarIdTyMetadata>, error: String) -> CompileOutput {
    CompileOutput::StaticErrors(StaticErrorCompileOutput {
        errors: vec![StaticError {
            span: Span {
                path: ast[&ModPath::new()].path.clone(),
                span: cfgrammar::Span::new(0, 0),
            },
            kind: StaticErrorKind::InvalidTech(error),
        }],
    })
}

fn check_output_layers(res: CompileOutput, tech_file: &FsPath) -> CompileOutput {
    let (data, mut errors) = match res {
        CompileOutput::ExecErrors(ExecErrorCompileOutput { errors, output }) => {
            if let Some(output) = output {
                (output, errors)
            } else {
                return CompileOutput::ExecErrors(ExecErrorCompileOutput { errors, output });
            }
        }
        CompileOutput::Valid(v) => (v, Vec::new()),
        o => return o,
    };
    check_layers(&data, tech_file, &mut errors);
    if errors.is_empty() {
        CompileOutput::Valid(data)
    } else {
        CompileOutput::ExecErrors(ExecErrorCompileOutput {
            errors,
            output: Some(data),
        })
    }
}

/// Rebinds a cached geometry snapshot to the current presentation technology.
/// Layer diagnostics are a post-execution concern, so callers can request a
/// fresh validation pass without rerunning evaluation or the solver.
pub(crate) fn refresh_output_technology(
    res: CompileOutput,
    tech: Technology,
    tech_file: &FsPath,
    recheck_layers: bool,
) -> CompileOutput {
    let res = match res {
        CompileOutput::Valid(mut data) => {
            data.tech = tech;
            CompileOutput::Valid(data)
        }
        CompileOutput::ExecErrors(mut output) => {
            if let Some(data) = &mut output.output {
                data.tech = tech;
            }
            if recheck_layers {
                output.errors.retain(|error| {
                    !matches!(
                        error.kind,
                        ExecErrorKind::IllegalLayer { .. } | ExecErrorKind::IllegalTextLayer { .. }
                    )
                });
            }
            CompileOutput::ExecErrors(output)
        }
        output => output,
    };
    if recheck_layers {
        check_output_layers(res, tech_file)
    } else {
        res
    }
}

pub fn compile(
    ast: &WorkspaceParseAst,
    input: CompileInput<'_>,
    config: &WorkspaceConfig,
) -> CompileOutput {
    let (ast, static_output) = if let Some(static_output) = static_compile(ast) {
        static_output
    } else {
        return CompileOutput::FatalParseErrors;
    };
    if !static_output.errors.is_empty() {
        return CompileOutput::StaticErrors(static_output);
    };

    execute_cell(&ast, input, config)
}

type ModDag<'a> = IndexMap<&'a ModPath, IndexMap<&'a ModPath, cfgrammar::Span>>;

pub(crate) type ModuleDependencies = IndexMap<ModPath, Vec<ModPath>>;

pub(crate) struct ImportPass<'a> {
    ast: &'a WorkspaceParseAst,
    current_path: &'a ModPath,
    deps: IndexMap<&'a ModPath, cfgrammar::Span>,
    errors: Vec<StaticError>,
}

pub(crate) fn construct_dag(ast: &WorkspaceParseAst) -> (ModDag<'_>, Vec<StaticError>) {
    let mut errors = Vec::new();
    let dag = ast
        .keys()
        .map(|path| {
            let (children, new_errors) = ImportPass::new(ast, path).execute();
            errors.extend(new_errors);

            (path, children)
        })
        .collect();
    errors.extend(dependency_cycle_errors(ast, &dag));
    (dag, errors)
}

pub(crate) fn module_dependencies(
    ast: &WorkspaceParseAst,
) -> (ModuleDependencies, Vec<StaticError>) {
    let (dag, errors) = construct_dag(ast);
    let dependencies = dag
        .into_iter()
        .map(|(module, dependencies)| {
            (
                module.clone(),
                dependencies.keys().map(|path| (*path).clone()).collect(),
            )
        })
        .collect();
    (dependencies, errors)
}

fn module_name(path: &ModPath) -> String {
    if path.is_empty() {
        "lib".to_owned()
    } else {
        path.join("::")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModuleVisitState {
    Visiting,
    Visited,
}

fn dependency_cycle_errors(ast: &WorkspaceParseAst, dag: &ModDag<'_>) -> Vec<StaticError> {
    fn visit<'a>(
        module: &'a ModPath,
        ast: &'a WorkspaceParseAst,
        dag: &ModDag<'a>,
        states: &mut IndexMap<&'a ModPath, ModuleVisitState>,
        stack: &mut Vec<&'a ModPath>,
        errors: &mut Vec<StaticError>,
    ) {
        states.insert(module, ModuleVisitState::Visiting);
        stack.push(module);

        for (dependency, reference_span) in &dag[module] {
            let dependency = *dependency;
            match states.get(dependency).copied() {
                Some(ModuleVisitState::Visiting) => {
                    let cycle_start = stack
                        .iter()
                        .position(|candidate| *candidate == dependency)
                        .expect("a visiting module is on the dependency stack");
                    let mut cycle = stack[cycle_start..]
                        .iter()
                        .map(|path| module_name(path))
                        .collect_vec();
                    cycle.push(module_name(dependency));
                    errors.push(StaticError {
                        span: Span {
                            path: ast[module].path.clone(),
                            span: *reference_span,
                        },
                        kind: StaticErrorKind::CyclicModuleDependency {
                            cycle: cycle.join(" -> "),
                        },
                    });
                }
                Some(ModuleVisitState::Visited) => {}
                None => visit(dependency, ast, dag, states, stack, errors),
            }
        }

        stack.pop();
        states.insert(module, ModuleVisitState::Visited);
    }

    let mut states = IndexMap::new();
    let mut stack = Vec::new();
    let mut errors = Vec::new();
    for module in dag.keys().copied() {
        if !states.contains_key(module) {
            visit(module, ast, dag, &mut states, &mut stack, &mut errors);
        }
    }
    errors
}

impl<'a> ImportPass<'a> {
    fn new(ast: &'a WorkspaceParseAst, current_path: &'a ModPath) -> Self {
        Self {
            ast,
            current_path,
            deps: Default::default(),
            errors: Default::default(),
        }
    }

    fn span(&self, span: cfgrammar::Span) -> Span {
        Span {
            path: self.ast[self.current_path].path.clone(),
            span,
        }
    }

    fn record_dependency(&mut self, path: ModPath, span: cfgrammar::Span) {
        // An unqualified name, or an explicitly qualified name that resolves
        // back to this module, does not create an edge between modules.
        if path == *self.current_path {
            return;
        }
        if let Some((path_ref, _)) = self.ast.get_key_value(&path) {
            self.deps.entry(path_ref).or_insert(span);
        } else {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::InvalidMod {
                    module: module_name(&path),
                },
            });
        }
    }

    fn use_module_path(&self, use_decl: &UseDecl<Substr, ParseMetadata>) -> ModPath {
        match use_decl.path[0].name.as_str() {
            "std" => vec!["std".to_string()],
            "lib" => use_decl
                .path
                .iter()
                .skip(1)
                .dropping_back(1)
                .map(|ident| ident.name.to_string())
                .collect(),
            _ => self
                .current_path
                .iter()
                .cloned()
                .chain(
                    use_decl
                        .path
                        .iter()
                        .dropping_back(1)
                        .map(|ident| ident.name.to_string()),
                )
                .collect(),
        }
    }

    pub(crate) fn execute(mut self) -> (IndexMap<&'a ModPath, cfgrammar::Span>, Vec<StaticError>) {
        for decl in &self.ast[self.current_path].ast.decls {
            match decl {
                Decl::Fn(f) => {
                    self.transform_fn_decl(f);
                }
                Decl::Cell(c) => {
                    self.transform_cell_decl(c);
                }
                Decl::Mod(_) => {}
                Decl::Use(u) => {
                    self.record_dependency(self.use_module_path(u), u.span);
                }
                Decl::Enum(_) => {}
                // `parse_ast` rejects these before this pass. Keep direct
                // library callers non-panicking if they construct an AST.
                Decl::Struct(_) | Decl::Constant(_) => continue,
            }
        }

        (self.deps, self.errors)
    }
}

impl<'a> AstTransformer for ImportPass<'a> {
    type InputMetadata = ParseMetadata;
    type OutputMetadata = ParseMetadata;
    type InputS = Substr;
    type OutputS = Substr;

    fn dispatch_ident(
        &mut self,
        _input: &Ident<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::Ident {
    }

    fn dispatch_ident_path(
        &mut self,
        input: &IdentPath<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IdentPath {
        // Multi-component identifier paths are enum values. The final two
        // components are the enum and variant, leaving the module path.
        if input.path.len() <= 2 || input.path[0].name == "std" {
            return;
        }
        let path = if input.path[0].name == "lib" {
            input
                .path
                .iter()
                .skip(1)
                .dropping_back(2)
                .map(|ident| ident.name.to_string())
                .collect_vec()
        } else {
            self.current_path
                .iter()
                .cloned()
                .chain(
                    input
                        .path
                        .iter()
                        .dropping_back(2)
                        .map(|ident| ident.name.to_string()),
                )
                .collect_vec()
        };
        self.record_dependency(path, input.span);
    }

    fn dispatch_enum_decl(
        &mut self,
        _input: &crate::ast::EnumDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _variants: &[Ident<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        _input: &CellDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _args: &[ArgDecl<Self::OutputS, Self::OutputMetadata>],
        _scope: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CellDecl {
    }

    fn dispatch_fn_decl(
        &mut self,
        _input: &FnDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _args: &[ArgDecl<Self::OutputS, Self::OutputMetadata>],
        _return_ty: &Option<TySpec<Self::OutputS, Self::OutputMetadata>>,
        _scope: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FnDecl {
    }

    fn dispatch_constant_decl(
        &mut self,
        _input: &ConstantDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _ty: &Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ConstantDecl {
    }

    fn dispatch_let_binding(
        &mut self,
        _input: &LetBinding<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::LetBinding {
    }

    fn dispatch_for_loop(
        &mut self,
        _input: &crate::ast::ForLoop<Self::InputS, Self::InputMetadata>,
        _var: &Ident<Self::OutputS, Self::OutputMetadata>,
        _seq: &Expr<Self::OutputS, Self::OutputMetadata>,
        _body: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ForLoop {
    }

    fn dispatch_if_expr(
        &mut self,
        _input: &IfExpr<Self::InputS, Self::InputMetadata>,
        _cond: &Expr<Self::OutputS, Self::OutputMetadata>,
        _then: &Scope<Self::OutputS, Self::OutputMetadata>,
        _else_: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IfExpr {
    }

    fn dispatch_match_expr(
        &mut self,
        _input: &crate::ast::MatchExpr<Self::InputS, Self::InputMetadata>,
        _scrutinee: &Expr<Self::OutputS, Self::OutputMetadata>,
        _arms: &[crate::ast::MatchArm<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::MatchExpr {
    }

    fn dispatch_bin_op_expr(
        &mut self,
        _input: &BinOpExpr<Self::InputS, Self::InputMetadata>,
        _left: &Expr<Self::OutputS, Self::OutputMetadata>,
        _right: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::BinOpExpr {
    }

    fn dispatch_unary_op_expr(
        &mut self,
        _input: &crate::ast::UnaryOpExpr<Self::InputS, Self::InputMetadata>,
        _operand: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::UnaryOpExpr {
    }

    fn dispatch_comparison_expr(
        &mut self,
        _input: &ComparisonExpr<Self::InputS, Self::InputMetadata>,
        _left: &Expr<Self::OutputS, Self::OutputMetadata>,
        _right: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ComparisonExpr {
    }

    fn dispatch_cast(
        &mut self,
        _input: &crate::ast::CastExpr<Self::InputS, Self::InputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
        _ty: &TySpec<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CastExpr {
    }

    fn dispatch_tuple_expr(
        &mut self,
        _input: &crate::ast::TupleExpr<Self::InputS, Self::InputMetadata>,
        _items: &[Expr<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::TupleExpr {
    }

    fn dispatch_field_access_expr(
        &mut self,
        _input: &FieldAccessExpr<Self::InputS, Self::InputMetadata>,
        _base: &Expr<Self::OutputS, Self::OutputMetadata>,
        _field: &Ident<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FieldAccessExpr {
    }

    fn dispatch_index_field_access_expr(
        &mut self,
        _input: &IndexFieldAccessExpr<Self::InputS, Self::InputMetadata>,
        _base: &Expr<Self::OutputS, Self::OutputMetadata>,
        _field: &IntLiteral,
    ) -> <Self::OutputMetadata as AstMetadata>::IndexFieldAccessExpr {
    }

    fn dispatch_index_expr(
        &mut self,
        _input: &crate::ast::IndexExpr<Self::InputS, Self::InputMetadata>,
        _base: &Expr<Self::OutputS, Self::OutputMetadata>,
        _index: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IndexExpr {
    }

    fn dispatch_call_expr(
        &mut self,
        _input: &CallExpr<Self::InputS, Self::InputMetadata>,
        func: &IdentPath<Self::OutputS, Self::OutputMetadata>,
        _args: &crate::ast::Args<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CallExpr {
        if func.path.len() > 1 && func.path[0].name != "std" {
            let path = if func.path[0].name == "lib" {
                func.path
                    .iter()
                    .skip(1)
                    .dropping_back(1)
                    .map(|ident| ident.name.to_string())
                    .collect_vec()
            } else {
                self.current_path
                    .iter()
                    .cloned()
                    .chain(
                        func.path
                            .iter()
                            .dropping_back(1)
                            .map(|ident| ident.name.to_string()),
                    )
                    .collect_vec()
            };
            self.record_dependency(path, func.span);
        }
    }

    fn dispatch_emit_expr(
        &mut self,
        _input: &crate::ast::EmitExpr<Self::InputS, Self::InputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EmitExpr {
    }

    fn dispatch_args(
        &mut self,
        _input: &crate::ast::Args<Self::InputS, Self::InputMetadata>,
        _posargs: &[Expr<Self::OutputS, Self::OutputMetadata>],
        _kwargs: &[crate::ast::KwArgValue<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::Args {
    }

    fn dispatch_kw_arg_value(
        &mut self,
        _input: &crate::ast::KwArgValue<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::KwArgValue {
    }

    fn dispatch_arg_decl(
        &mut self,
        _input: &ArgDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _ty: &TySpec<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ArgDecl {
    }

    fn dispatch_scope(
        &mut self,
        _input: &Scope<Self::InputS, Self::InputMetadata>,
        _stmts: &[Statement<Self::OutputS, Self::OutputMetadata>],
        _tail: &Option<Expr<Self::OutputS, Self::OutputMetadata>>,
    ) -> <Self::OutputMetadata as AstMetadata>::Scope {
    }

    fn transform_s(&mut self, s: &Self::InputS) -> Self::OutputS {
        s.clone()
    }
}

fn check_layers(data: &CompiledData, tech_file: &FsPath, errs: &mut Vec<ExecError>) {
    let mut layers = IndexSet::new();
    for layer in &data.tech.layers {
        layers.insert(layer.name.clone());
    }
    for (cell_id, cell) in data.cells.iter() {
        for (_, obj) in cell.objects.iter() {
            match obj {
                SolvedValue::Rect(r) => {
                    if let Some(layer) = &r.layer
                        && !layers.contains(layer)
                    {
                        errs.push(ExecError {
                            span: r.span.clone(),
                            cell: *cell_id,
                            kind: ExecErrorKind::IllegalLayer {
                                layer: layer.clone(),
                                tech: tech_file.display().to_string(),
                            },
                        });
                    }
                }
                SolvedValue::Polygon(polygon) if !layers.contains(&polygon.layer) => {
                    errs.push(ExecError {
                        span: polygon.span.clone(),
                        cell: *cell_id,
                        kind: ExecErrorKind::IllegalLayer {
                            layer: polygon.layer.clone(),
                            tech: tech_file.display().to_string(),
                        },
                    });
                }
                SolvedValue::Path(path) if !layers.contains(&path.layer) => {
                    errs.push(ExecError {
                        span: path.span.clone(),
                        cell: *cell_id,
                        kind: ExecErrorKind::IllegalLayer {
                            layer: path.layer.clone(),
                            tech: tech_file.display().to_string(),
                        },
                    });
                }
                SolvedValue::Text(text) if !layers.contains(&text.layer) => {
                    errs.push(ExecError {
                        span: text.span.clone(),
                        cell: *cell_id,
                        kind: ExecErrorKind::IllegalTextLayer {
                            layer: text.layer.clone(),
                            tech: tech_file.display().to_string(),
                        },
                    });
                }
                _ => {}
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
pub(crate) struct VarIdTyFrame {
    var_bindings: IndexMap<Substr, (VarId, Ty)>,
}

impl VarIdTyFrame {
    pub(crate) fn interface_fingerprint(&self) -> Vec<u8> {
        let mut bindings = self.var_bindings.iter().collect_vec();
        bindings.sort_by_key(|(name, _)| *name);
        bincode::serialize(&bindings).expect("module interface bindings should always serialize")
    }
}

pub(crate) struct TypedModuleOutput {
    pub(crate) ast: AnnotatedAst<VarIdTyMetadata>,
    pub(crate) bindings: VarIdTyFrame,
    pub(crate) errors: Vec<StaticError>,
}

pub(crate) struct VarIdTyPass<'a> {
    ast: &'a AnnotatedAst<ParseMetadata>,
    mod_bindings: &'a IndexMap<ModPath, VarIdTyFrame>,
    current_path: &'a ModPath,
    next_id: VarId,
    bindings: Vec<VarIdTyFrame>,
    errors: Vec<StaticError>,
    cancellation: Option<&'a CancellationToken>,
}

struct WorkspaceVarIdTyPass<'a> {
    ast: &'a WorkspaceParseAst,
    dag: &'a ModDag<'a>,
    cancellation: Option<&'a CancellationToken>,
    mod_bindings: IndexMap<ModPath, VarIdTyFrame>,
    workspace_ast: WorkspaceAst<VarIdTyMetadata>,
    errors: Vec<StaticError>,
    next_id: VarId,
}

pub(crate) fn execute_var_id_ty_pass<'a>(
    ast: &'a WorkspaceParseAst,
    dag: &'a ModDag<'a>,
    cancellation: Option<&'a CancellationToken>,
) -> Option<(WorkspaceAst<VarIdTyMetadata>, Vec<StaticError>)> {
    let mut pass = WorkspaceVarIdTyPass {
        ast,
        dag,
        cancellation,
        mod_bindings: IndexMap::new(),
        workspace_ast: IndexMap::new(),
        errors: Vec::new(),
        next_id: 1,
    };
    let std_mod_path = vec!["std".to_string()];
    let std_mod_path = ast.get_key_value(&std_mod_path).map(|(k, _)| k);
    if let Some((root, _)) = ast.get_key_value(&vec![]) {
        for path in [std_mod_path, Some(root)]
            .into_iter()
            .flatten()
            .chain(ast.keys())
        {
            if !pass.analyze(path) {
                return None;
            }
        }
    }
    Some((pass.workspace_ast, pass.errors))
}

impl<'a> WorkspaceVarIdTyPass<'a> {
    fn analyze(&mut self, current_path: &'a ModPath) -> bool {
        if self
            .cancellation
            .is_some_and(CancellationToken::is_cancelled)
        {
            return false;
        }
        // TODO: fix hacky way to track visited modules.
        if self.mod_bindings.contains_key(current_path) {
            return true;
        }
        self.mod_bindings
            .insert(current_path.clone(), VarIdTyFrame::default());

        let dependencies = self.dag[current_path].keys().copied().collect_vec();
        for dependency in dependencies {
            if !self.analyze(dependency) {
                return false;
            }
        }

        let mut pass = VarIdTyPass {
            ast: &self.ast[current_path],
            mod_bindings: &self.mod_bindings,
            current_path,
            next_id: self.next_id,
            bindings: vec![VarIdTyFrame::default()],
            errors: vec![],
            cancellation: self.cancellation,
        };
        let ast = pass.execute();
        self.workspace_ast.insert(current_path.clone(), ast);
        self.errors.extend(pass.errors);
        self.next_id = pass.next_id;
        self.mod_bindings.insert(
            current_path.clone(),
            pass.bindings.into_iter().next().unwrap(),
        );
        !self
            .cancellation
            .is_some_and(CancellationToken::is_cancelled)
    }
}

pub(crate) fn analyze_module_cancellable<'a>(
    ast: &'a WorkspaceParseAst,
    current_path: &'a ModPath,
    mod_bindings: &'a IndexMap<ModPath, VarIdTyFrame>,
    cancellation: Option<&'a CancellationToken>,
) -> Option<TypedModuleOutput> {
    let mut pass = VarIdTyPass {
        ast: &ast[current_path],
        mod_bindings,
        current_path,
        next_id: semantic_local_id_start(current_path),
        bindings: vec![VarIdTyFrame::default()],
        errors: Vec::new(),
        cancellation,
    };
    let ast = pass.execute();
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return None;
    }
    Some(TypedModuleOutput {
        ast,
        bindings: pass.bindings.into_iter().next().unwrap(),
        errors: pass.errors,
    })
}

#[derive(Debug, Clone)]
pub struct VarIdTyMetadata;

#[enumify]
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ty {
    /// A type that does not exist; usually encountered due to user error.
    ///
    /// Suppresses type checking of dependent properties.
    #[default]
    Unknown,
    /// Catch-all any type.
    ///
    /// Should eventually be removed.
    Any,
    Bool,
    Float,
    Int,
    Rect,
    Polygon,
    Path,
    Point,
    String,
    Cell(Arc<CellTy>),
    Inst(Arc<CellTy>),
    Nil,
    SeqNil,
    Fn(Box<FnTy>),
    /// An enum variant type, e.g. the type of `MyEnum::MyVariant`.
    Enum(EnumTy),
    CellFn(Box<CellFnTy>),
    Seq(Box<Ty>),
    Tuple(Vec<Ty>),
}

#[derive(Debug, Clone, Copy)]
enum PolygonAxis {
    X,
    Y,
}

#[derive(Debug, Clone, Copy)]
struct PolygonCoordinate {
    axis: PolygonAxis,
    index: usize,
    initial: bool,
}

fn polygon_coordinate(name: &str) -> Option<PolygonCoordinate> {
    let (axis, index) = name.split_at_checked(1)?;
    let (index, initial) = index
        .strip_suffix('i')
        .map_or((index, false), |index| (index, true));
    if index.is_empty() || !index.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let index = index.parse().ok()?;
    let axis = match axis {
        "x" => PolygonAxis::X,
        "y" => PolygonAxis::Y,
        _ => return None,
    };
    Some(PolygonCoordinate {
        axis,
        index,
        initial,
    })
}

impl Ty {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Bool" => Some(Ty::Bool),
            "Int" => Some(Ty::Int),
            "Float" => Some(Ty::Float),
            "Rect" => Some(Ty::Rect),
            "Polygon" => Some(Ty::Polygon),
            "Path" => Some(Ty::Path),
            "Point" => Some(Ty::Point),
            "Any" => Some(Ty::Any),
            "String" => Some(Ty::String),
            "()" => Some(Ty::Nil),
            "[]" => Some(Ty::SeqNil),
            _ => None,
        }
    }

    /// Computes the least upper bound (LUB) of self and other.
    /// For use in type promotion.
    pub fn lub(&self, other: &Self) -> Self {
        match (self, other) {
            // Unknown promotes to any type.
            (Ty::Unknown, other) | (other, Ty::Unknown) => other.clone(),
            // At least one Any results in Any.
            (Ty::Any, _) | (_, Ty::Any) => Ty::Any,
            // SeqNil promotes to any sequence type.
            (Ty::SeqNil, Ty::Seq(inner)) | (Ty::Seq(inner), Ty::SeqNil) => Ty::Seq(inner.clone()),
            // Mismatched types promote to any.
            (a, b) => {
                if a == b {
                    a.clone()
                } else {
                    Ty::Any
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FnTy {
    args: Vec<Ty>,
    ret: Ty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellFnTy {
    args: Vec<Ty>,
    /// The structural type produced when this cell function is called.
    ///
    /// Stored behind an `Arc` so that every caller (and every `inst` of the
    /// resulting cell) shares one allocation instead of deep-copying it. This
    /// keeps the type representation a DAG rather than a tree: a cell that
    /// references a child twice embeds two `Arc`s to the *same* `CellTy`, so
    /// type size stays linear in hierarchy depth instead of doubling per level.
    cell: Arc<CellTy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellTy {
    data: IndexMap<String, Ty>,
    /// GDS-backed cells publish geometry fields at execution time. Their
    /// generated source declaration is intentionally only a signature, so
    /// field names cannot be enumerated by the source type pass.
    dynamic_fields: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumTy {
    id: EnumId,
    variants: IndexSet<String>,
}

impl AstMetadata for VarIdTyMetadata {
    type Ident = ();
    type IdentPath = (Option<VarId>, Ty);
    type EnumDecl = ();
    type StructDecl = ();
    type StructField = ();
    type CellDecl = (PathBuf, VarId);
    type ConstantDecl = ();
    type LetBinding = VarId;
    type ForLoop = VarId; // the var ID of the var Ident
    type FnDecl = (PathBuf, VarId);
    type IfExpr = Ty;
    type MatchExpr = Ty;
    type BinOpExpr = Ty;
    type UnaryOpExpr = Ty;
    type ComparisonExpr = Ty;
    type FieldAccessExpr = Ty;
    type IndexFieldAccessExpr = Ty;
    type IndexExpr = Ty;
    type CallExpr = (Option<VarId>, Ty);
    type EmitExpr = Ty;
    type Args = ();
    type KwArgValue = Ty;
    type ArgDecl = (VarId, Ty);
    type Scope = Ty;
    type Typ = ();
    type CastExpr = Ty;
    type TupleExpr = Ty;
}

impl<'a> VarIdTyPass<'a> {
    fn span(&self, span: cfgrammar::Span) -> Span {
        Span {
            path: self.ast.path.clone(),
            span,
        }
    }

    fn lookup(&self, name: &str) -> Option<(VarId, Ty)> {
        for frame in self.bindings.iter().rev() {
            if let Some(info) = frame.var_bindings.get(name) {
                return Some(info.clone());
            }
        }
        None
    }

    fn unresolved_local_name_error(
        &self,
        name: &str,
        use_span: cfgrammar::Span,
    ) -> StaticErrorKind {
        let is_cell_declared_later = self.ast.ast.decls.iter().any(|decl| {
            matches!(decl, Decl::Cell(cell)
                if cell.name.name.as_str() == name
                    && cell.name.span.start() > use_span.start())
        });

        if is_cell_declared_later {
            StaticErrorKind::UseBeforeDeclaration {
                name: name.to_owned(),
            }
        } else {
            StaticErrorKind::UndeclaredVar {
                name: name.to_owned(),
            }
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn alloc(&mut self, name: &Substr, ty: Ty) -> VarId {
        let id = self.alloc_id();
        self.bind(name, id, ty)
    }

    fn alloc_declaration(&mut self, kind: &str, name: &Substr, ty: Ty) -> VarId {
        let id = semantic_declaration_id(self.current_path, kind, name);
        self.bind(name, id, ty)
    }

    fn bind(&mut self, name: &Substr, id: VarId, ty: Ty) -> VarId {
        self.bindings
            .last_mut()
            .unwrap()
            .var_bindings
            .insert(name.clone(), (id, ty));
        id
    }

    fn execute(&mut self) -> AnnotatedAst<VarIdTyMetadata> {
        let mut decls = Vec::new();
        // Enum types must exist before imports and function signatures are
        // resolved. Imports are then installed before functions are declared,
        // allowing imported enum types in signatures and imported functions in
        // any declaration body regardless of source order.
        for decl in &self.ast.ast.decls {
            if self.should_cancel() {
                break;
            }
            if let Decl::Enum(e) = decl {
                self.declare_enum_decl(e);
            }
        }
        for decl in &self.ast.ast.decls {
            if self.should_cancel() {
                break;
            }
            if let Decl::Use(u) = decl {
                self.declare_use_decl(u);
            }
        }
        for decl in &self.ast.ast.decls {
            if self.should_cancel() {
                break;
            }
            if let Decl::Fn(f) = decl {
                self.declare_fn_decl(f);
            }
        }

        for decl in &self.ast.ast.decls {
            if self.should_cancel() {
                break;
            }
            match decl {
                Decl::Fn(f) => {
                    decls.push(Decl::Fn(self.transform_fn_decl(f)));
                }
                Decl::Cell(c) => {
                    decls.push(Decl::Cell(self.transform_cell_decl(c)));
                }
                Decl::Mod(m) => {
                    decls.push(Decl::Mod(self.transform_mod_decl(m)));
                }
                Decl::Use(u) => {
                    decls.push(Decl::Use(self.transform_use_decl(u)));
                }
                Decl::Enum(e) => {
                    decls.push(Decl::Enum(self.transform_enum_decl(e)));
                }
                // `parse_ast` rejects these before this pass. Keep direct
                // library callers non-panicking if they construct an AST.
                Decl::Struct(_) | Decl::Constant(_) => continue,
            }
        }

        AnnotatedAst::new(
            self.ast.text.clone(),
            &Ast {
                decls,
                span: self.ast.ast.span,
            },
            self.ast.path.clone(),
        )
    }

    fn use_module_path(&self, use_decl: &UseDecl<Substr, ParseMetadata>) -> ModPath {
        match use_decl.path[0].name.as_str() {
            "std" => vec!["std".to_string()],
            "lib" => use_decl
                .path
                .iter()
                .skip(1)
                .dropping_back(1)
                .map(|ident| ident.name.to_string())
                .collect(),
            _ => self
                .current_path
                .iter()
                .cloned()
                .chain(
                    use_decl
                        .path
                        .iter()
                        .dropping_back(1)
                        .map(|ident| ident.name.to_string()),
                )
                .collect(),
        }
    }

    fn declare_use_decl(&mut self, use_decl: &UseDecl<Substr, ParseMetadata>) {
        let module = self.use_module_path(use_decl);
        let item = use_decl.path.last().expect("use paths are non-empty");
        let local_name = use_decl.alias.as_ref().unwrap_or(item);
        let imported = if &module == self.current_path {
            self.lookup(&item.name)
        } else {
            self.mod_bindings
                .get(&module)
                .and_then(|frame| frame.var_bindings.get(item.name.as_str()).cloned())
        };

        if let Some(binding) = imported {
            self.bindings
                .last_mut()
                .unwrap()
                .var_bindings
                .insert(local_name.name.clone(), binding);
        } else {
            self.errors.push(StaticError {
                span: self.span(use_decl.span),
                kind: StaticErrorKind::UnresolvedImport {
                    path: use_decl
                        .path
                        .iter()
                        .map(|ident| ident.name.as_str())
                        .join("::"),
                },
            });
        }
    }

    fn declare_fn_decl(&mut self, input: &'a FnDecl<Substr, ParseMetadata>) {
        if BUILTINS.contains(&input.name.name.as_str()) {
            self.errors.push(StaticError {
                span: self.span(input.name.span),
                kind: StaticErrorKind::RedeclarationOfBuiltin,
            });
        }
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| {
                let ty_spec = self.transform_ty_spec(&arg.ty);
                self.ty_from_spec(&ty_spec)
            })
            .collect();
        let ty = Ty::Fn(Box::new(FnTy {
            args,
            ret: if let Some(return_ty) = &input.return_ty {
                self.ty_from_spec(return_ty)
            } else {
                Ty::Nil
            },
        }));
        self.alloc_declaration("fn", &input.name.name, ty);
    }

    fn declare_enum_decl(&mut self, input: &'a EnumDecl<Substr, ParseMetadata>) {
        if BUILTINS.contains(&input.name.name.as_str()) {
            self.errors.push(StaticError {
                span: self.span(input.name.span),
                kind: StaticErrorKind::RedeclarationOfBuiltin,
            });
            return;
        }
        let mut variants = IndexSet::with_capacity(input.variants.len());
        for variant in input.variants.iter() {
            if variants.contains(variant.name.as_str()) {
                self.errors.push(StaticError {
                    span: self.span(variant.span),
                    kind: StaticErrorKind::DuplicateNameDeclaration,
                });
            }
            variants.insert(variant.name.to_string());
        }
        let ty = Ty::Enum(EnumTy {
            id: semantic_declaration_id(self.current_path, "enum-type", &input.name.name),
            variants,
        });
        self.alloc_declaration("enum", &input.name.name, ty);
    }

    fn ty_from_spec<M: AstMetadata>(&mut self, spec: &TySpec<Substr, M>) -> Ty {
        match &spec.kind {
            TySpecKind::Ident(ident) => Ty::from_name(ident.name.as_str()).unwrap_or_else(|| {
                if let Some((_, ty)) = self.lookup(ident.name.as_str()) {
                    ty
                } else {
                    self.errors.push(StaticError {
                        span: self.span(ident.span),
                        kind: StaticErrorKind::UnknownType,
                    });
                    Ty::Unknown
                }
            }),
            TySpecKind::Seq(inner) => Ty::Seq(Box::new(self.ty_from_spec(inner))),
            // The empty tuple type `()` is the unit type, i.e. the type of the
            // `()` value (`Expr::Nil` => `Ty::Nil`). Lower it to `Ty::Nil` so the
            // two agree — otherwise `Ty::Tuple([])` would be a distinct type no
            // value could inhabit (`is_eq_ty` never equates it with `Ty::Nil`).
            TySpecKind::Tuple(t) if t.is_empty() => Ty::Nil,
            TySpecKind::Tuple(t) => Ty::Tuple(t.iter().map(|x| self.ty_from_spec(x)).collect()),
        }
    }

    fn no_field_on_ty<M: AstMetadata>(&mut self, field: &Ident<Substr, M>, ty: Ty) -> Ty {
        self.errors.push(StaticError {
            span: self.span(field.span),
            kind: StaticErrorKind::NoFieldOnTy {
                field: field.name.to_string(),
                ty,
            },
        });
        Ty::Unknown
    }

    fn cannot_index<M: AstMetadata>(&mut self, base: &Expr<Substr, M>, ty: Ty) -> Ty {
        self.errors.push(StaticError {
            span: self.span(base.span()),
            kind: StaticErrorKind::CannotIndex { ty },
        });
        Ty::Unknown
    }

    fn is_eq_ty(a: &Ty, b: &Ty) -> bool {
        if *a == Ty::Any || *b == Ty::Any {
            return true;
        }

        // An empty sequence belongs to every sequence type, as `Ty::lub` and the
        // `cons` tail check already assume. The runtime check in
        // `CellArg::matches_ty` still enforces emptiness where `[]` is declared.
        if matches!((a, b), (Ty::SeqNil, Ty::Seq(_)) | (Ty::Seq(_), Ty::SeqNil)) {
            return true;
        }

        if let Ty::Seq(a) = a
            && let Ty::Seq(b) = b
        {
            return VarIdTyPass::is_eq_ty(a, b);
        }

        if let Ty::Tuple(a) = a
            && let Ty::Tuple(b) = b
        {
            if a.len() != b.len() {
                return false;
            }
            return a
                .iter()
                .zip(b.iter())
                .all(|(a, b)| VarIdTyPass::is_eq_ty(a, b));
        }

        *a == *b
    }

    fn assert_eq_ty(&mut self, span: cfgrammar::Span, found: &Ty, expected: &Ty) {
        if !VarIdTyPass::is_eq_ty(found, expected) {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::IncorrectTy {
                    found: found.clone(),
                    expected: expected.clone(),
                },
            });
        }
    }

    fn assert_ty_is_cell(&mut self, span: cfgrammar::Span, ty: &Ty) {
        if !matches!(ty, Ty::Cell(_) | Ty::Any) {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::IncorrectTyCategory {
                    found: ty.clone(),
                    expected: "Cell".into(),
                },
            });
        }
    }

    fn assert_ty_is_enum(&mut self, span: cfgrammar::Span, ty: &Ty) {
        if !matches!(ty, Ty::Enum(_) | Ty::Any) {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::IncorrectTyCategory {
                    found: ty.clone(),
                    expected: "Enum".into(),
                },
            });
        }
    }

    fn assert_eq_arity(&mut self, span: cfgrammar::Span, found: usize, expected: usize) {
        if found != expected {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::CallIncorrectPositionalArity { expected, found },
            });
        }
    }

    fn typecheck_kwargs(
        &mut self,
        kwargs: &[KwArgValue<Substr, VarIdTyMetadata>],
        kwarg_defs: IndexMap<&str, Ty>,
    ) {
        let mut defined = IndexSet::new();
        for kwarg in kwargs {
            let mut cont = false;
            if !kwarg_defs.contains_key(&kwarg.name.name.as_str()) {
                self.errors.push(StaticError {
                    span: self.span(kwarg.name.span),
                    kind: StaticErrorKind::InvalidKwArg,
                });
                cont = true;
            }
            if defined.contains(&&kwarg.name.name) {
                self.errors.push(StaticError {
                    span: self.span(kwarg.name.span),
                    kind: StaticErrorKind::DuplicateKwArg,
                });
                cont = true;
            }
            defined.insert(&kwarg.name.name);
            if !cont {
                self.assert_eq_ty(
                    kwarg.value.span(),
                    &kwarg.value.ty(),
                    kwarg_defs.get(&kwarg.name.name.as_str()).unwrap(),
                );
            }
        }
    }

    fn typecheck_posargs(
        &mut self,
        call_span: cfgrammar::Span,
        args: &[Expr<Substr, VarIdTyMetadata>],
        arg_defs: &[Ty],
    ) {
        self.assert_eq_arity(call_span, args.len(), arg_defs.len());
        for (found, expected) in args.iter().zip(arg_defs) {
            self.assert_eq_ty(found.span(), &found.ty(), expected);
        }
    }

    fn typecheck_args(
        &mut self,
        call_span: cfgrammar::Span,
        args: &crate::ast::Args<Substr, VarIdTyMetadata>,
        arg_defs: &[Ty],
        kwarg_defs: IndexMap<&str, Ty>,
    ) {
        self.typecheck_posargs(call_span, &args.posargs, arg_defs);
        self.typecheck_kwargs(&args.kwargs, kwarg_defs);
    }

    fn typecheck_call(
        &mut self,
        name: &str,
        lookup: Option<(VarId, Ty)>,
        call_span: cfgrammar::Span,
        args: &crate::ast::Args<Substr, VarIdTyMetadata>,
        is_local: bool,
    ) -> (Option<VarId>, Ty) {
        if let Some((varid, ty)) = lookup {
            match ty {
                Ty::Fn(ty) => {
                    self.typecheck_args(call_span, args, &ty.args, IndexMap::new());
                    (Some(varid), ty.ret.clone())
                }
                Ty::CellFn(ty) => {
                    self.typecheck_args(call_span, args, &ty.args, IndexMap::new());
                    (Some(varid), Ty::Cell(ty.cell.clone()))
                }
                ty => {
                    self.errors.push(StaticError {
                        span: self.span(call_span),
                        kind: StaticErrorKind::CannotCall(ty),
                    });
                    (None, Ty::Unknown)
                }
            }
        } else {
            self.errors.push(StaticError {
                span: self.span(call_span),
                kind: if is_local {
                    self.unresolved_local_name_error(name, call_span)
                } else {
                    StaticErrorKind::UndeclaredVar {
                        name: name.to_owned(),
                    }
                },
            });
            (None, Ty::Unknown)
        }
    }
}

impl<S> Expr<S, VarIdTyMetadata> {
    fn ty(&self) -> Ty {
        match self {
            Expr::If(if_expr) => if_expr.metadata.clone(),
            Expr::Match(match_expr) => match_expr.metadata.clone(),
            Expr::Comparison(comparison_expr) => comparison_expr.metadata.clone(),
            Expr::BinOp(bin_op_expr) => bin_op_expr.metadata.clone(),
            Expr::Call(call_expr) => call_expr.metadata.1.clone(),
            Expr::Emit(emit_expr) => emit_expr.metadata.clone(),
            Expr::IdentPath(path) => path.metadata.1.clone(),
            Expr::FieldAccess(field_access_expr) => field_access_expr.metadata.clone(),
            Expr::IndexFieldAccess(index_field_access_expr) => {
                index_field_access_expr.metadata.clone()
            }
            Expr::Index(index_expr) => index_expr.metadata.clone(),
            Expr::Nil(_) => Ty::Nil,
            Expr::SeqNil(_) => Ty::SeqNil,
            Expr::FloatLiteral(_) => Ty::Float,
            Expr::IntLiteral(_) => Ty::Int,
            Expr::BoolLiteral(_) => Ty::Bool,
            Expr::StringLiteral(_) => Ty::String,
            Expr::Scope(scope) => scope.metadata.clone(),
            Expr::Cast(cast) => cast.metadata.clone(),
            Expr::UnaryOp(unary_op_expr) => unary_op_expr.metadata.clone(),
            Expr::Tuple(t) => t.metadata.clone(),
        }
    }
}

/// Stable typed contract for cache invalidation. Variable IDs are omitted so
/// one-shot invocation analysis and per-module incremental analysis produce
/// identical versions, while resolved enum variants and structural cell field
/// types remain part of the contract.
pub(crate) fn declaration_contract_fingerprint(
    declaration: &Decl<Substr, VarIdTyMetadata>,
) -> Option<Vec<u8>> {
    match declaration {
        Decl::Cell(cell) => {
            let args = cell
                .args
                .iter()
                .map(|argument| &argument.metadata.1)
                .collect_vec();
            let fields = cell
                .scope
                .stmts
                .iter()
                .filter_map(|statement| match statement {
                    Statement::LetBinding(binding) => {
                        Some((binding.name.name.as_str(), binding.value.ty()))
                    }
                    _ => None,
                })
                .collect_vec();
            Some(
                bincode::serialize(&("cell", args, fields))
                    .expect("typed cell contracts should always serialize"),
            )
        }
        Decl::Fn(function) => {
            let args = function
                .args
                .iter()
                .map(|argument| &argument.metadata.1)
                .collect_vec();
            Some(
                bincode::serialize(&("fn", args, &function.scope.metadata))
                    .expect("typed function contracts should always serialize"),
            )
        }
        _ => None,
    }
}

impl<'a> AstTransformer for VarIdTyPass<'a> {
    type InputMetadata = ParseMetadata;
    type OutputMetadata = VarIdTyMetadata;
    type InputS = Substr;
    type OutputS = Substr;

    fn should_cancel(&self) -> bool {
        self.cancellation
            .is_some_and(CancellationToken::is_cancelled)
    }

    fn dispatch_ident(
        &mut self,
        _input: &Ident<Substr, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::Ident {
    }

    fn dispatch_ident_path(
        &mut self,
        input: &IdentPath<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IdentPath {
        // Currently, ident path exprs are either single variables or enum values.
        // Parser grammar ensures paths cannot be empty.
        assert!(!input.path.is_empty());
        if input.path.len() == 1 {
            if let Some((varid, ty)) = self.lookup(&input.path[0].name) {
                (Some(varid), ty)
            } else {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: self.unresolved_local_name_error(&input.path[0].name, input.span),
                });
                (None, Ty::Unknown)
            }
        } else {
            // look up enum
            let path = match input.path[0].name.as_str() {
                "std" => {
                    vec!["std".to_string()]
                }
                "lib" => input
                    .path
                    .iter()
                    .skip(1)
                    .dropping_back(2)
                    .map(|ident| ident.name.to_string())
                    .collect_vec(),
                _ => self
                    .current_path
                    .iter()
                    .cloned()
                    .chain(
                        input
                            .path
                            .iter()
                            .dropping_back(2)
                            .map(|ident| ident.name.to_string()),
                    )
                    .collect_vec(),
            };
            let enum_ = &input.path[input.path.len() - 2];
            let lookup = if path.is_empty() || &path == self.current_path {
                self.lookup(&enum_.name)
            } else {
                self.mod_bindings
                    .get(&path)
                    .as_ref()
                    .and_then(|mod_binding| {
                        mod_binding.var_bindings.get(enum_.name.as_str()).cloned()
                    })
            };
            if let Some((_, ty)) = lookup {
                if let Ty::Enum(ref e) = ty {
                    let variant = &input.path.last().unwrap().name;
                    if !e.variants.contains(variant.as_str()) {
                        self.errors.push(StaticError {
                            span: self.span(enum_.span),
                            kind: StaticErrorKind::InvalidVariant(variant.to_string()),
                        });
                    }
                    (None, ty)
                } else {
                    self.errors.push(StaticError {
                        span: self.span(enum_.span),
                        kind: StaticErrorKind::NotAnEnum,
                    });
                    (None, Ty::Unknown)
                }
            } else {
                self.errors.push(StaticError {
                    span: self.span(enum_.span),
                    kind: StaticErrorKind::NotAnEnum,
                });
                (None, Ty::Unknown)
            }
        }
    }

    fn dispatch_enum_decl(
        &mut self,
        _input: &crate::ast::EnumDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _variants: &[Ident<Substr, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        _input: &CellDecl<Substr, Self::InputMetadata>,
        name: &Ident<Substr, Self::OutputMetadata>,
        _args: &[ArgDecl<Substr, Self::OutputMetadata>],
        _scope: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CellDecl {
        // TODO: Argument checks
        (self.ast.path.clone(), self.lookup(&name.name).unwrap().0)
    }

    fn dispatch_fn_decl(
        &mut self,
        _input: &FnDecl<Substr, Self::InputMetadata>,
        name: &Ident<Substr, Self::OutputMetadata>,
        _args: &[ArgDecl<Substr, Self::OutputMetadata>],
        _return_ty: &Option<TySpec<Substr, Self::OutputMetadata>>,
        _scope: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FnDecl {
        (self.ast.path.clone(), self.lookup(&name.name).unwrap().0)
    }

    fn transform_fn_decl(
        &mut self,
        input: &FnDecl<Substr, Self::InputMetadata>,
    ) -> FnDecl<Substr, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let return_ty = input
            .return_ty
            .as_ref()
            .map(|spec| self.transform_ty_spec(spec));
        self.enter_scope(&input.scope);
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let scope = self.transform_scope_contents(&input.scope);
        self.exit_scope(&input.scope, &scope);
        let metadata = self.dispatch_fn_decl(input, &name, &args, &return_ty, &scope);
        FnDecl {
            name,
            args,
            return_ty,
            scope,
            span: input.span,
            metadata,
        }
    }

    fn transform_cell_decl(
        &mut self,
        input: &CellDecl<Substr, Self::InputMetadata>,
    ) -> CellDecl<Substr, Self::OutputMetadata> {
        if BUILTINS.contains(&input.name.name.as_str()) {
            self.errors.push(StaticError {
                span: self.span(input.name.span),
                kind: StaticErrorKind::RedeclarationOfBuiltin,
            });
        }
        self.enter_scope(&input.scope);
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let scope = self.transform_scope_contents(&input.scope);
        self.exit_scope(&input.scope, &scope);
        if let Some(tail) = scope.tail.as_ref() {
            self.errors.push(StaticError {
                span: self.span(tail.span()),
                kind: StaticErrorKind::CellWithTailExpr,
            });
        }
        let data = scope
            .stmts
            .iter()
            .filter_map(|stmt| {
                if let Statement::LetBinding(lt) = stmt {
                    if ["x", "y"].contains(&lt.name.name.as_str()) {
                        self.errors.push(StaticError {
                            span: self.span(lt.name.span),
                            kind: StaticErrorKind::RedeclarationOfBuiltin,
                        });
                    }
                    Some((lt.name.name.to_string(), lt.value.ty()))
                } else {
                    None
                }
            })
            .collect();
        let dynamic_fields = self
            .ast
            .ast
            .decls
            .iter()
            .take(self.ast.generated_declarations)
            .any(|decl| {
                matches!(decl, Decl::Cell(cell) if cell.span == input.span && cell.name.name == input.name.name)
            });
        let ty = Ty::CellFn(Box::new(CellFnTy {
            args: args.iter().map(|arg| arg.metadata.1.clone()).collect(),
            cell: Arc::new(CellTy {
                data,
                dynamic_fields,
            }),
        }));
        self.alloc_declaration("cell", &input.name.name, ty);
        let name = self.transform_ident(&input.name);
        let metadata = self.dispatch_cell_decl(input, &name, &args, &scope);
        CellDecl {
            name,
            scope,
            args,
            span: input.span,
            metadata,
        }
    }

    fn transform_call_expr(
        &mut self,
        input: &CallExpr<Self::InputS, Self::InputMetadata>,
    ) -> CallExpr<Self::OutputS, Self::OutputMetadata> {
        let func = IdentPath {
            path: input
                .func
                .path
                .iter()
                .map(|ident| self.transform_ident(ident))
                .collect(),
            metadata: (None, Ty::Unknown),
            span: input.func.span,
        };
        let args = self.transform_args(&input.args);
        let metadata = self.dispatch_call_expr(input, &func, &args);
        CallExpr {
            scope_order: input.scope_order,
            func,
            args,
            span: input.span,
            metadata,
        }
    }

    fn dispatch_constant_decl(
        &mut self,
        _input: &ConstantDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _ty: &Ident<Substr, Self::OutputMetadata>,
        _value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ConstantDecl {
    }

    fn dispatch_if_expr(
        &mut self,
        _input: &IfExpr<Substr, Self::InputMetadata>,
        cond: &Expr<Substr, Self::OutputMetadata>,
        then: &Scope<Substr, Self::OutputMetadata>,
        else_: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IfExpr {
        let cond_ty = cond.ty();
        let then_ty = then.metadata.clone();
        let else_ty = else_.metadata.clone();
        if cond_ty != Ty::Bool {
            self.errors.push(StaticError {
                span: self.span(cond.span()),
                kind: StaticErrorKind::IfCondNotBool,
            });
        }
        then_ty.lub(&else_ty)
    }

    fn dispatch_match_expr(
        &mut self,
        input: &crate::ast::MatchExpr<Self::InputS, Self::InputMetadata>,
        scrutinee: &Expr<Self::OutputS, Self::OutputMetadata>,
        arms: &[crate::ast::MatchArm<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::MatchExpr {
        let scrutinee_ty = scrutinee.ty();
        self.assert_ty_is_enum(scrutinee.span(), &scrutinee_ty);
        let mut lub_ty: Option<Ty> = None;

        if let Ty::Enum(ref e) = scrutinee_ty {
            let mut covered = IndexSet::new();
            let mut remaining = e.variants.clone();
            for arm in arms.iter() {
                let arm_ty = &arm.pattern.metadata.1;
                self.assert_eq_ty(arm.pattern.span, arm_ty, &scrutinee_ty);

                let variant = arm.pattern.path.last().unwrap().name.clone();
                remaining.swap_remove(variant.as_str());
                if !covered.insert(variant) {
                    self.errors.push(StaticError {
                        span: self.span(arm.pattern.span),
                        kind: StaticErrorKind::DuplicateMatchArm,
                    });
                }

                if let Some(ref inner) = lub_ty {
                    lub_ty = Some(inner.lub(&arm.expr.ty()));
                } else {
                    lub_ty = Some(arm.expr.ty());
                }
            }

            if !remaining.is_empty() {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::MatchArmsNotComprehensive,
                });
            }
        }

        lub_ty.unwrap_or_default()
    }

    fn dispatch_bin_op_expr(
        &mut self,
        input: &BinOpExpr<Substr, Self::InputMetadata>,
        left: &Expr<Substr, Self::OutputMetadata>,
        right: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::BinOpExpr {
        let left_ty = left.ty();
        let right_ty = right.ty();
        if !VarIdTyPass::is_eq_ty(&left_ty, &right_ty) {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::BinOpMismatchedTypes,
            });
        }
        if ![Ty::Float, Ty::Int, Ty::Any].contains(&left_ty) {
            self.errors.push(StaticError {
                span: self.span(left.span()),
                kind: StaticErrorKind::BinOpInvalidType(left_ty.clone()),
            });
        }
        if ![Ty::Float, Ty::Int, Ty::Any].contains(&right_ty) {
            self.errors.push(StaticError {
                span: self.span(right.span()),
                kind: StaticErrorKind::BinOpInvalidType(right_ty),
            });
        }
        left_ty
    }

    fn dispatch_unary_op_expr(
        &mut self,
        input: &crate::ast::UnaryOpExpr<Substr, Self::InputMetadata>,
        operand: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::UnaryOpExpr {
        match input.op {
            UnaryOp::Not => {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::Unimplemented,
                });
                Ty::Bool
            }
            UnaryOp::Neg => {
                let operand_ty = operand.ty();
                if ![Ty::Float, Ty::Int, Ty::Any].contains(&operand_ty) {
                    self.errors.push(StaticError {
                        span: self.span(operand.span()),
                        kind: StaticErrorKind::UnaryOpInvalidType,
                    });
                }
                operand_ty
            }
        }
    }

    fn dispatch_comparison_expr(
        &mut self,
        input: &ComparisonExpr<Substr, Self::InputMetadata>,
        left: &Expr<Substr, Self::OutputMetadata>,
        right: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ComparisonExpr {
        let left_ty = left.ty();
        let right_ty = right.ty();
        let lub_ty = left_ty.lub(&right_ty);
        if left_ty == Ty::Float && (input.op == ComparisonOp::Eq || input.op == ComparisonOp::Ne) {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::FloatEquality,
            });
        }
        if matches!(left_ty, Ty::Enum(_))
            && (input.op != ComparisonOp::Eq && input.op != ComparisonOp::Ne)
        {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::EnumsNotOrd,
            });
        }
        if matches!(left_ty, Ty::Nil)
            && matches!(right_ty, Ty::Nil)
            && (input.op != ComparisonOp::Eq && input.op != ComparisonOp::Ne)
        {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::NilNotOrd,
            });
        }
        if matches!(left_ty, Ty::SeqNil)
            && matches!(right_ty, Ty::SeqNil)
            && (input.op != ComparisonOp::Eq && input.op != ComparisonOp::Ne)
        {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::SeqNilNotOrd,
            });
        }
        if matches!(lub_ty, Ty::Seq(_))
            && (input.op != ComparisonOp::Eq && input.op != ComparisonOp::Ne)
            && !(left_ty == Ty::SeqNil || right_ty == Ty::SeqNil)
        {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::SeqMustCompareEqSeqNil,
            });
        }
        if !matches!(
            left_ty,
            Ty::Float | Ty::Int | Ty::Enum(_) | Ty::Seq(_) | Ty::Nil | Ty::SeqNil
        ) {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::ComparisonInvalidType,
            });
        }

        Ty::Bool
    }

    fn dispatch_field_access_expr(
        &mut self,
        _input: &crate::ast::FieldAccessExpr<Substr, Self::InputMetadata>,
        base: &Expr<Substr, Self::OutputMetadata>,
        field: &Ident<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FieldAccessExpr {
        let base_ty = base.ty();
        match base_ty {
            Ty::Rect => match field.name.as_str() {
                "x0" | "x1" | "y0" | "y1" | "w" | "h" => Ty::Float,
                "layer" => Ty::String,
                _ => self.no_field_on_ty(field, Ty::Rect),
            },
            Ty::Polygon => match field.name.as_str() {
                "points" => Ty::Seq(Box::new(Ty::Point)),
                "layer" => Ty::String,
                name if polygon_coordinate(name).is_some_and(|coordinate| !coordinate.initial) => {
                    Ty::Float
                }
                _ => self.no_field_on_ty(field, Ty::Polygon),
            },
            Ty::Path => match field.name.as_str() {
                "points" => Ty::Seq(Box::new(Ty::Point)),
                "layer" => Ty::String,
                "width" | "begin_extension" | "end_extension" => Ty::Float,
                name if polygon_coordinate(name).is_some_and(|coordinate| !coordinate.initial) => {
                    Ty::Float
                }
                _ => self.no_field_on_ty(field, Ty::Path),
            },
            Ty::Point => match field.name.as_str() {
                "x" | "y" => Ty::Float,
                _ => self.no_field_on_ty(field, Ty::Point),
            },
            Ty::Inst(ref c) => match field.name.as_str() {
                "x" | "y" => Ty::Float,
                name => c.data.get(name).cloned().unwrap_or_else(|| {
                    if c.dynamic_fields {
                        Ty::Any
                    } else {
                        self.no_field_on_ty(field, base_ty.clone())
                    }
                }),
            },
            // Propagate any and unknown types without throwing an error.
            Ty::Any => Ty::Any,
            Ty::Unknown => Ty::Unknown,
            _ => self.no_field_on_ty(field, base_ty.clone()),
        }
    }

    fn dispatch_index_field_access_expr(
        &mut self,
        _input: &crate::ast::IndexFieldAccessExpr<Substr, Self::InputMetadata>,
        base: &Expr<Substr, Self::OutputMetadata>,
        field: &IntLiteral,
    ) -> <Self::OutputMetadata as AstMetadata>::IndexFieldAccessExpr {
        let base_ty = base.ty();
        match base_ty {
            Ty::Tuple(t) => usize::try_from(field.value)
                .map(|i| {
                    t.get(i).cloned().unwrap_or_else(|| {
                        self.errors.push(StaticError {
                            span: self.span(field.span),
                            kind: StaticErrorKind::TupleIndexOutOfRange,
                        });
                        Ty::Unknown
                    })
                })
                .unwrap_or_else(|_| {
                    self.errors.push(StaticError {
                        span: self.span(field.span),
                        kind: StaticErrorKind::TupleIndexOutOfRange,
                    });
                    Ty::Unknown
                }),
            // Propagate any and unknown types without throwing an error.
            Ty::Any => Ty::Any,
            Ty::Unknown => Ty::Unknown,
            _ => {
                self.errors.push(StaticError {
                    span: self.span(field.span),
                    kind: StaticErrorKind::CannotIndexFieldAccess { ty: base_ty },
                });
                Ty::Unknown
            }
        }
    }

    fn dispatch_index_expr(
        &mut self,
        _input: &crate::ast::IndexExpr<Self::InputS, Self::InputMetadata>,
        base: &Expr<Self::OutputS, Self::OutputMetadata>,
        index: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IndexExpr {
        let base_ty = base.ty();
        self.assert_eq_ty(index.span(), &index.ty(), &Ty::Int);
        match base_ty {
            Ty::Seq(s) => (*s).clone(),
            // Propagate any and unknown types without throwing an error.
            Ty::Any => Ty::Any,
            Ty::Unknown => Ty::Unknown,
            _ => self.cannot_index(base, base_ty.clone()),
        }
    }

    fn dispatch_call_expr(
        &mut self,
        input: &crate::ast::CallExpr<Substr, Self::InputMetadata>,
        func: &IdentPath<Substr, Self::OutputMetadata>,
        args: &crate::ast::Args<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CallExpr {
        if func.path.len() == 1 {
            match func.path[0].name.as_str() {
                name @ "crect" | name @ "rect" => {
                    let kwarg_defs = if name == "crect" {
                        self.typecheck_posargs(input.span, &args.posargs, &[]);
                        IndexMap::from_iter([
                            ("x0", Ty::Float),
                            ("x1", Ty::Float),
                            ("y0", Ty::Float),
                            ("y1", Ty::Float),
                            ("x0i", Ty::Float),
                            ("x1i", Ty::Float),
                            ("y0i", Ty::Float),
                            ("y1i", Ty::Float),
                            ("w", Ty::Float),
                            ("h", Ty::Float),
                            ("layer", Ty::String),
                        ])
                    } else {
                        self.typecheck_posargs(input.span, &args.posargs, &[Ty::String]);
                        IndexMap::from_iter([
                            ("x0", Ty::Float),
                            ("x1", Ty::Float),
                            ("y0", Ty::Float),
                            ("y1", Ty::Float),
                            ("x0i", Ty::Float),
                            ("x1i", Ty::Float),
                            ("y0i", Ty::Float),
                            ("y1i", Ty::Float),
                            ("w", Ty::Float),
                            ("h", Ty::Float),
                        ])
                    };
                    self.typecheck_kwargs(&args.kwargs, kwarg_defs);
                    (None, Ty::Rect)
                }
                "polygon" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 2);
                    if let Some(layer) = args.posargs.first() {
                        self.assert_eq_ty(layer.span(), &layer.ty(), &Ty::String);
                    }
                    if let Some(points) = args.posargs.get(1) {
                        self.assert_eq_ty(points.span(), &points.ty(), &Ty::Int);
                    }
                    let kwarg_defs = args
                        .kwargs
                        .iter()
                        .filter_map(|kwarg| {
                            polygon_coordinate(kwarg.name.name.as_str())
                                .map(|_| (kwarg.name.name.as_str(), Ty::Float))
                        })
                        .collect();
                    self.typecheck_kwargs(&args.kwargs, kwarg_defs);
                    (None, Ty::Polygon)
                }
                "path" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 2);
                    if let Some(layer) = args.posargs.first() {
                        self.assert_eq_ty(layer.span(), &layer.ty(), &Ty::String);
                    }
                    if let Some(points) = args.posargs.get(1) {
                        self.assert_eq_ty(points.span(), &points.ty(), &Ty::Int);
                    }
                    let kwarg_defs = args
                        .kwargs
                        .iter()
                        .filter_map(|kwarg| {
                            let name = kwarg.name.name.as_str();
                            (matches!(
                                name,
                                "width"
                                    | "widthi"
                                    | "begin_extension"
                                    | "begin_extensioni"
                                    | "end_extension"
                                    | "end_extensioni"
                            ) || polygon_coordinate(name).is_some())
                            .then_some((name, Ty::Float))
                        })
                        .collect();
                    self.typecheck_kwargs(&args.kwargs, kwarg_defs);
                    (None, Ty::Path)
                }
                "text" => {
                    // text, layer, x, y
                    self.typecheck_posargs(
                        input.span,
                        &args.posargs,
                        &[Ty::String, Ty::String, Ty::Float, Ty::Float],
                    );
                    self.typecheck_kwargs(&args.kwargs, IndexMap::default());
                    (None, Ty::Nil)
                }
                "cons" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 2);
                    if args.posargs.len() == 2 {
                        let seqty = Ty::Seq(Box::new(args.posargs[0].ty()));
                        let tailty = args.posargs[1].ty();
                        if !(tailty == Ty::SeqNil || VarIdTyPass::is_eq_ty(&tailty, &seqty)) {
                            self.errors.push(StaticError {
                                span: self.span(args.posargs[1].span()),
                                kind: StaticErrorKind::IncorrectTy {
                                    found: tailty,
                                    expected: seqty.clone(),
                                },
                            });
                        }
                        (None, seqty)
                    } else {
                        (None, Ty::SeqNil)
                    }
                }
                "list" => {
                    self.typecheck_kwargs(&args.kwargs, IndexMap::default());
                    if args.posargs.is_empty() {
                        self.errors.push(StaticError {
                            span: self.span(input.span),
                            kind: StaticErrorKind::EmptyListConstructor,
                        });
                        (None, Ty::Nil)
                    } else {
                        let elem_ty = args
                            .posargs
                            .iter()
                            .map(Expr::ty)
                            .reduce(|acc, e| acc.lub(&e))
                            .unwrap();
                        (None, Ty::Seq(Box::new(elem_ty)))
                    }
                }
                "range_full" => {
                    // Native builtin backing `std::range`/`std::range_full`: builds the
                    // whole `[Int]` in one pass instead of recursive `cons`.
                    self.typecheck_posargs(input.span, &args.posargs, &[Ty::Int, Ty::Int, Ty::Int]);
                    self.typecheck_kwargs(&args.kwargs, IndexMap::default());
                    (None, Ty::Seq(Box::new(Ty::Int)))
                }
                "head" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 1);
                    if args.posargs.len() == 1 {
                        let argty = args.posargs[0].ty();
                        let vty = match argty {
                            Ty::Seq(i) => *i,
                            Ty::Any => Ty::Any,
                            Ty::Unknown => Ty::Unknown,
                            _ => {
                                self.errors.push(StaticError {
                                    span: self.span(input.span),
                                    kind: StaticErrorKind::IncorrectTyCategory {
                                        found: argty,
                                        expected: "Seq".to_string(),
                                    },
                                });
                                Ty::Unknown
                            }
                        };
                        (None, vty)
                    } else {
                        (None, Ty::Unknown)
                    }
                }
                "tail" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 1);
                    if args.posargs.len() == 1 {
                        let argty = args.posargs[0].ty();
                        let vty = match argty {
                            Ty::Seq(_) => argty,
                            Ty::Any => Ty::Any,
                            Ty::Unknown => Ty::Unknown,
                            _ => {
                                self.errors.push(StaticError {
                                    span: self.span(input.span),
                                    kind: StaticErrorKind::IncorrectTyCategory {
                                        found: argty,
                                        expected: "Seq".to_string(),
                                    },
                                });
                                Ty::Unknown
                            }
                        };
                        (None, vty)
                    } else {
                        (None, Ty::Nil)
                    }
                }
                "bbox" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 1);
                    let argty = args.posargs[0].ty();
                    if !matches!(argty, Ty::Cell(_) | Ty::Inst(_)) {
                        self.errors.push(StaticError {
                            span: self.span(input.span),
                            kind: StaticErrorKind::IncorrectTyCategory {
                                found: argty,
                                expected: "Cell/Inst".to_string(),
                            },
                        });
                    }
                    (None, Ty::Rect)
                }
                "float" => {
                    self.typecheck_args(input.span, args, &[], IndexMap::new());
                    (None, Ty::Float)
                }
                "eq" => {
                    self.typecheck_args(input.span, args, &[Ty::Float, Ty::Float], IndexMap::new());
                    (None, Ty::Nil)
                }
                "dimension" => {
                    self.typecheck_args(
                        input.span,
                        args,
                        &[
                            Ty::Float,
                            Ty::Float,
                            Ty::Float,
                            Ty::Float,
                            Ty::Float,
                            Ty::Float,
                            Ty::Bool,
                        ],
                        IndexMap::new(),
                    );
                    (None, Ty::Nil)
                }
                "inst" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 1);
                    self.typecheck_kwargs(
                        &args.kwargs,
                        IndexMap::from_iter([
                            ("reflect", Ty::Bool),
                            ("angle", Ty::Int),
                            ("x", Ty::Float),
                            ("y", Ty::Float),
                            ("xi", Ty::Float),
                            ("yi", Ty::Float),
                            ("construction", Ty::Bool),
                        ]),
                    );
                    if let Some(ty) = args.posargs.first() {
                        self.assert_ty_is_cell(ty.span(), &ty.ty());
                        match ty.ty() {
                            Ty::Cell(c) => (None, Ty::Inst(c.clone())),
                            Ty::Any => (None, Ty::Any),
                            _ => (None, Ty::Unknown),
                        }
                    } else {
                        (None, Ty::Unknown)
                    }
                }
                name => self.typecheck_call(name, self.lookup(name), input.span, args, true),
            }
        } else {
            let path = match func.path[0].name.as_str() {
                "std" => {
                    vec!["std".to_string()]
                }
                "lib" => func
                    .path
                    .iter()
                    .skip(1)
                    .dropping_back(1)
                    .map(|ident| ident.name.to_string())
                    .collect_vec(),
                _ => self
                    .current_path
                    .iter()
                    .cloned()
                    .chain(
                        func.path
                            .iter()
                            .dropping_back(1)
                            .map(|ident| ident.name.to_string()),
                    )
                    .collect_vec(),
            };
            let name = &func.path.last().unwrap().name;
            let lookup = self
                .mod_bindings
                .get(&path)
                .as_ref()
                .and_then(|mod_binding| mod_binding.var_bindings.get(name.as_str()).cloned());
            self.typecheck_call(name, lookup, input.span, args, false)
        }
    }

    fn dispatch_emit_expr(
        &mut self,
        _input: &crate::ast::EmitExpr<Substr, Self::InputMetadata>,
        value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EmitExpr {
        value.ty()
    }

    fn dispatch_args(
        &mut self,
        _input: &crate::ast::Args<Substr, Self::InputMetadata>,
        _posargs: &[Expr<Substr, Self::OutputMetadata>],
        _kwargs: &[crate::ast::KwArgValue<Substr, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::Args {
    }

    fn dispatch_cast(
        &mut self,
        input: &crate::ast::CastExpr<Substr, Self::InputMetadata>,
        value: &Expr<Substr, Self::OutputMetadata>,
        ty: &TySpec<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CastExpr {
        let ty = self.ty_from_spec(ty);
        match (value.ty(), &ty) {
            (Ty::Int, Ty::Float)
            | (Ty::Int, Ty::Int)
            | (Ty::Float, Ty::Int)
            | (Ty::Float, Ty::Float) => (),
            (_, Ty::Unknown) => (),
            (Ty::Any, _) | (_, Ty::Any) => (),
            _ => {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::InvalidCast,
                });
            }
        };
        ty
    }

    fn dispatch_tuple_expr(
        &mut self,
        _input: &crate::ast::TupleExpr<Self::InputS, Self::InputMetadata>,
        items: &[Expr<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::TupleExpr {
        Ty::Tuple(items.iter().map(|i| i.ty()).collect())
    }

    fn dispatch_kw_arg_value(
        &mut self,
        _input: &crate::ast::KwArgValue<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::KwArgValue {
        value.ty()
    }

    fn dispatch_arg_decl(
        &mut self,
        input: &ArgDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _ty: &TySpec<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ArgDecl {
        let ty = self.ty_from_spec(&input.ty);
        (self.alloc(&input.name.name, ty.clone()), ty)
    }

    fn dispatch_scope(
        &mut self,
        _input: &Scope<Substr, Self::InputMetadata>,
        _stmts: &[Statement<Substr, Self::OutputMetadata>],
        tail: &Option<Expr<Substr, Self::OutputMetadata>>,
    ) -> <Self::OutputMetadata as AstMetadata>::Scope {
        tail.as_ref().map(|tail| tail.ty()).unwrap_or(Ty::Nil)
    }

    fn enter_scope(&mut self, _input: &crate::ast::Scope<Substr, Self::InputMetadata>) {
        self.bindings.push(Default::default());
    }

    fn exit_scope(
        &mut self,
        _input: &crate::ast::Scope<Substr, Self::InputMetadata>,
        _output: &crate::ast::Scope<Substr, Self::OutputMetadata>,
    ) {
        self.bindings.pop();
    }

    fn dispatch_let_binding(
        &mut self,
        _input: &LetBinding<Substr, Self::InputMetadata>,
        name: &Ident<Substr, Self::OutputMetadata>,
        value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::LetBinding {
        self.alloc(&name.name, value.ty())
    }

    fn transform_for_loop(
        &mut self,
        input: &crate::ast::ForLoop<Self::InputS, Self::InputMetadata>,
    ) -> crate::ast::ForLoop<Self::OutputS, Self::OutputMetadata> {
        let var = self.transform_ident(&input.var);
        let seq = self.transform_expr(&input.seq);
        let seq_ty = seq.ty();
        let elem_ty = match seq_ty {
            Ty::Any => Ty::Any,
            Ty::Unknown => Ty::Unknown,
            Ty::Seq(t) => (*t).clone(),
            Ty::SeqNil => Ty::Any,
            _ => {
                self.errors.push(StaticError {
                    span: self.span(input.seq.span()),
                    kind: StaticErrorKind::CannotIterate { ty: seq_ty },
                });
                Ty::Unknown
            }
        };
        self.enter_scope(&input.body);
        let var_id = self.alloc(&input.var.name, elem_ty);
        let body = self.transform_scope_contents(&input.body);
        self.exit_scope(&input.body, &body);
        let metadata = var_id;
        ForLoop {
            var,
            seq,
            body,
            scope_order: input.scope_order,
            metadata,
            span: input.span,
        }
    }
    fn dispatch_for_loop(
        &mut self,
        _input: &crate::ast::ForLoop<Self::InputS, Self::InputMetadata>,
        _var: &Ident<Self::OutputS, Self::OutputMetadata>,
        _seq: &Expr<Self::OutputS, Self::OutputMetadata>,
        _body: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ForLoop {
        unreachable!()
    }

    fn transform_s(&mut self, s: &Self::InputS) -> Self::OutputS {
        s.clone()
    }
}

#[derive(Debug, Clone)]
pub enum CellArg {
    Float(f64),
    Int(i64),
    Bool(bool),
    String(String),
    /// An enum variant, identified by name like [`Value::EnumValue`].
    Enum(String),
    Seq(Vec<CellArg>),
}

impl CellArg {
    fn matches_ty(&self, ty: &Ty) -> bool {
        match (self, ty) {
            (_, Ty::Any) => true,
            (Self::Float(_), Ty::Float)
            | (Self::Int(_), Ty::Int)
            | (Self::Bool(_), Ty::Bool)
            | (Self::String(_), Ty::String) => true,
            (Self::Enum(variant), Ty::Enum(ty)) => ty.variants.contains(variant),
            (Self::Seq(values), Ty::Seq(inner)) => {
                values.iter().all(|value| value.matches_ty(inner))
            }
            (Self::Seq(values), Ty::SeqNil) => values.is_empty(),
            _ => false,
        }
    }

    fn ty_name(&self) -> &'static str {
        match self {
            Self::Float(_) => "Float",
            Self::Int(_) => "Int",
            Self::Bool(_) => "Bool",
            Self::String(_) => "String",
            Self::Enum(_) => "enum variant",
            Self::Seq(_) => "sequence",
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
struct CellExecKey {
    cell: VarId,
    args: Vec<CellArgKey>,
    scope_name: Option<String>,
}

fn stable_hasher(domain: &[u8]) -> FnvHasher {
    let mut hasher = FnvHasher::default();
    hasher.write(domain);
    hasher
}

/// Assign a deterministic ID to a semantic cell execution. Incremental
/// artifacts can then retain their immutable compiled cells without remapping
/// every instance edge into the next execution's allocation namespace.
fn stable_cell_id(key: &CellExecKey) -> CellId {
    stable_cell_subgraph_id(key, u64::MAX)
}

/// Imported GDS structures are children of one source-level cell execution.
/// Give each structure a stable ID in that execution's namespace as well.
fn stable_cell_subgraph_id(key: &CellExecKey, index: u64) -> CellId {
    let encoded =
        bincode::serialize(&(key, index)).expect("cell execution keys should always serialize");
    let mut hasher = stable_hasher(b"argon-compiled-cell");
    hasher.write(&encoded);
    let hash = hasher.finish();
    // Zero is reserved by diagnostics as an unknown cell.
    if hash == 0 { 1 } else { hash }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
enum CellArgKey {
    Float(u64),
    Int(i64),
    Bool(bool),
    String(String),
    Enum(String),
    Seq(Vec<CellArgKey>),
}

/// A solved cell subgraph that can be imported into a later execution after
/// its declaration dependencies have been validated by the incremental
/// session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CellArtifact {
    cell_keys: HashMap<CellExecKey, CellId>,
    pub(crate) cells: IndexMap<CellId, Arc<CompiledCell>>,
    root: CellId,
    pub(crate) dependencies: IndexSet<VarId>,
    pub(crate) errors: Vec<ExecError>,
}

impl CellArtifact {
    pub(crate) fn root_cell(&self) -> VarId {
        self.cell_keys
            .iter()
            .find_map(|(key, cell)| (*cell == self.root).then_some(key.cell))
            .expect("cell artifacts contain their root execution key")
    }

    pub(crate) fn same_key(&self, other: &Self) -> bool {
        let root_key = self
            .cell_keys
            .iter()
            .find_map(|(key, cell)| (*cell == self.root).then_some(key));
        let other_root_key = other
            .cell_keys
            .iter()
            .find_map(|(key, cell)| (*cell == other.root).then_some(key));
        root_key == other_root_key
    }
}

/// A solved entry-cell execution retained immediately before emission. It can
/// be resumed when the next typed body is an exact statement-prefix extension.
#[derive(Clone)]
pub(crate) struct CellContinuation {
    key: CellExecKey,
    cell: VarId,
    arguments: Vec<u8>,
    statements: Vec<Vec<u8>>,
    global_value_limit: ValueId,
    cell_id: CellId,
    frame: FrameId,
    root_scope: ScopeId,
    next_seq_num: SeqNum,
    error_start: usize,
    dependencies: IndexSet<VarId>,
    state: RootExecutionState,
}

impl CellContinuation {
    pub(crate) fn cell(&self) -> VarId {
        self.cell
    }

    pub(crate) fn dependencies(&self) -> &IndexSet<VarId> {
        &self.dependencies
    }

    pub(crate) fn same_key(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

fn encoded_cell_arguments(cell: &CellDecl<Substr, VarIdTyMetadata>) -> Vec<u8> {
    bincode::serialize(&cell.args).expect("typed cell arguments should always serialize")
}

fn encoded_cell_statements(cell: &CellDecl<Substr, VarIdTyMetadata>) -> Vec<Vec<u8>> {
    cell.scope
        .stmts
        .iter()
        .map(|statement| {
            bincode::serialize(statement).expect("typed cell statements should always serialize")
        })
        .collect()
}

fn is_numeric_literal(expression: &Expr<Substr, VarIdTyMetadata>) -> bool {
    match expression {
        Expr::FloatLiteral(_) | Expr::IntLiteral(_) => true,
        Expr::UnaryOp(expression) if expression.op == UnaryOp::Neg => {
            matches!(
                expression.operand,
                Expr::FloatLiteral(_) | Expr::IntLiteral(_)
            )
        }
        _ => false,
    }
}

/// GUI-drawn rectangles are independent additions: all their coordinates are
/// literals, so continuing from a solved prefix cannot constrain an old solver
/// variable. More general suffixes deliberately take the full execution path.
fn is_independent_emitted_rect(statement: &Statement<Substr, VarIdTyMetadata>) -> bool {
    let Statement::LetBinding(binding) = statement else {
        return false;
    };
    let Expr::Emit(emitted) = &binding.value else {
        return false;
    };
    let Expr::Call(call) = &emitted.value else {
        return false;
    };
    if call.metadata.0.is_some()
        || call.func.path.len() != 1
        || !matches!(call.func.path[0].name.as_str(), "rect" | "crect")
        || !call
            .args
            .posargs
            .iter()
            .all(|argument| matches!(argument, Expr::StringLiteral(_)))
    {
        return false;
    }
    call.args.kwargs.iter().all(|argument| {
        if argument.name.name == "layer" {
            matches!(argument.value, Expr::StringLiteral(_))
        } else {
            is_numeric_literal(&argument.value)
        }
    })
}

impl From<&CellArg> for CellArgKey {
    fn from(value: &CellArg) -> Self {
        match value {
            CellArg::Float(f) => Self::Float(f.to_bits()),
            CellArg::Int(i) => Self::Int(*i),
            CellArg::Bool(b) => Self::Bool(*b),
            CellArg::String(s) => Self::String(s.clone()),
            CellArg::Enum(v) => Self::Enum(v.clone()),
            CellArg::Seq(v) => Self::Seq(v.iter().map(Self::from).collect()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompileInput<'a> {
    /// Full path to cell.
    pub cell: &'a [&'a str],
    pub args: Vec<CellArg>,
}

pub type VarId = u64;
pub type ConstraintVarId = u64;

/// Stable process-independent identity for a top-level semantic declaration.
/// Local bindings continue to use compact sequential IDs in the low half of
/// the ID space.
fn semantic_declaration_id(module: &ModPath, kind: &str, name: &str) -> VarId {
    let mut hasher = stable_hasher(b"argon-declaration");
    hasher.write_u8(0xff);
    let mut write = |bytes: &[u8]| {
        hasher.write(bytes);
        hasher.write_u8(0xff);
    };
    for component in module {
        write(component.as_bytes());
    }
    write(kind.as_bytes());
    write(name.as_bytes());
    hasher.finish() | (1_u64 << 63)
}

/// Gives independently analyzed modules disjoint, deterministic local-ID
/// namespaces. Top-level declarations use the high half of the ID space.
fn semantic_local_id_start(module: &ModPath) -> VarId {
    let mut hasher = stable_hasher(b"argon-module-locals");
    for component in module {
        hasher.write(component.as_bytes());
        hasher.write_u8(0xff);
    }
    // Reserve enough low bits for all practical local bindings in a module,
    // while keeping the declaration-ID high bit clear.
    (hasher.finish() & 0x7fff_ffff_0000_0000) | 0x0000_0000_0010_0000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledEmit {
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BasicRect<T> {
    pub layer: Option<String>,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
    pub construction: bool,
}

/// Formats a GUI-created layout coordinate as an Argon float literal after
/// snapping it to the technology grid.
pub fn format_initial_condition(value: f64, grid: f64) -> String {
    let snapped = crate::tech::snap(value, grid);
    let value = format!("{snapped}");
    if value.contains('.') {
        value
    } else {
        format!("{value}.")
    }
}

#[cfg(test)]
mod initial_condition_format_tests {
    use super::format_initial_condition;

    #[test]
    fn snaps_gui_coordinates_to_the_technology_grid() {
        assert_eq!(format_initial_condition(12.345678, 0.1), "12.3");
        assert_eq!(format_initial_condition(12.0, 0.1), "12.");
        assert_eq!(format_initial_condition(-0.04, 0.1), "0.");
        assert_eq!(format_initial_condition(1.2000000476837158, 0.1), "1.2");
        assert_eq!(format_initial_condition(12.37, 0.25), "12.25");
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rect<T> {
    pub layer: Option<String>,
    pub id: ObjectId,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
    pub construction: bool,
    pub span: Option<Span>,
}

/// A polygon whose vertex coordinates are independent values.
///
/// Keeping every coordinate separate is important: an Argon constraint may
/// refer to `polygon.points[i].x` or `.y` without coupling the vertex to the
/// rest of the shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Polygon<T> {
    pub layer: String,
    pub id: ObjectId,
    pub points: Vec<(T, T)>,
    pub span: Option<Span>,
}

/// A constant-width GDS-style path whose centerline coordinates are
/// independent solver values.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Path<T> {
    pub layer: String,
    pub id: ObjectId,
    pub width: T,
    pub points: Vec<(T, T)>,
    /// Distance by which the path extends before and after its centerline.
    /// These retain the geometry of imported GDS path types 0, 2, and 4.
    pub begin_extension: T,
    pub end_extension: T,
    pub span: Option<Span>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dimension<T> {
    pub id: ObjectId,
    pub p: T,
    pub n: T,
    pub value: T,
    pub coord: T,
    pub pstop: T,
    pub nstop: T,
    pub horiz: bool,
    pub constraint: ConstraintId,
    pub span: Option<Span>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Text<T> {
    pub id: ObjectId,
    pub text: String,
    pub layer: String,
    pub x: T,
    pub y: T,
    pub span: Option<Span>,
}

type FrameId = u64;
type ValueId = u64;
pub type CellId = u64;
pub type EnumId = u64;

/// Sequence number.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize, Ord, PartialOrd)]
pub struct SeqNum(u64);

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub struct ObjectId(u64);

impl ObjectId {
    /// Monotonic allocation order used to preserve source creation order in
    /// consumers that flatten objects for display.
    pub fn creation_order(self) -> u64 {
        self.0
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub struct ScopeId(u64);

impl ScopeId {
    /// Build a stable ID from the semantic hierarchy rather than execution's
    /// global allocation order.
    fn semantic(parent: Option<Self>, name: &str) -> Self {
        let mut hasher = FnvHasher::default();
        if let Some(parent) = parent {
            hasher.write(&parent.0.to_le_bytes());
        }
        hasher.write(name.as_bytes());
        Self(hasher.finish())
    }
}

#[cfg(test)]
mod stable_id_tests {
    use super::{
        CellArgKey, CellExecKey, ScopeId, semantic_declaration_id, semantic_local_id_start,
        stable_cell_id,
    };

    #[test]
    fn stable_ids_keep_their_previous_values() {
        let module = vec!["layout".into(), "devices".into()];
        let key = CellExecKey {
            cell: semantic_declaration_id(&module, "cell", "inverter"),
            args: vec![CellArgKey::Int(3), CellArgKey::Bool(true)],
            scope_name: Some("preview".to_owned()),
        };
        let root_scope = ScopeId::semantic(None, "inverter");
        let child_scope = ScopeId::semantic(Some(root_scope), "devices");

        assert_eq!(key.cell, 0xcf77_1fae_9cc0_b330);
        assert_eq!(semantic_local_id_start(&module), 0x3256_5a86_0010_0000);
        assert_eq!(stable_cell_id(&key), 0xd1f6_301a_5f4e_b806);
        assert_eq!(root_scope.0, 0xd8db_071a_ec8d_0b5a);
        assert_eq!(child_scope.0, 0x2842_7fba_437f_2a82);
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub(crate) struct DynLoc {
    pub(crate) cell: CellId,
    pub(crate) frame: FrameId,
    pub(crate) scope: ScopeId,
    pub(crate) seq_num: SeqNum,
}

#[derive(Clone)]
struct Frame {
    bindings: IndexMap<VarId, ValueId>,
    parent: Option<FrameId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Emit {
    value: ValueId,
    scope: ScopeId,
    span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ObjectEmit {
    object: ObjectId,
    scope: ScopeId,
    span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExecScope {
    parent: Option<ScopeId>,
    static_parent: Option<(ScopeId, SeqNum)>,
    name: String,
    span: Span,
    bindings: IndexMap<SeqNum, (String, ValueId)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FallbackConstraint {
    priority: i32,
    constraint: LinearExpr,
    span: Span,
    initial_condition: Option<RectInitialCondition>,
}

impl PartialEq for FallbackConstraint {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl Eq for FallbackConstraint {}

impl PartialOrd for FallbackConstraint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FallbackConstraint {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}

#[derive(Clone)]
struct CellState {
    solve_iters: u64,
    solver: Solver,
    fields: IndexMap<String, ValueId>,
    emit: Vec<Emit>,
    object_emit: Vec<ObjectEmit>,
    objects: IndexMap<ObjectId, Object>,
    deferred: IndexSet<ValueId>,
    root_scope: ScopeId,
    scopes: IndexMap<ScopeId, ExecScope>,
    fallback_constraints: BinaryHeap<FallbackConstraint>,
    fallback_constraints_used: Vec<UsedFallback>,
    sse_basis: SseBasis,
    unsolved_vars: Option<IndexSet<Var>>,
    constraint_span_map: IndexMap<ConstraintId, Span>,
    var_span_map: IndexMap<Var, Span>,
    var_dependents: IndexMap<Var, IndexSet<ValueId>>,
}

impl CellState {
    fn new_solver_var(&mut self, span: &Span) -> Var {
        let var = self.solver.new_var();
        self.var_span_map.insert(var, span.clone());
        var
    }
}

#[derive(Clone)]
struct RootExecutionState {
    cell_states: IndexMap<CellId, CellState>,
    values: IndexMap<ValueId, DeferValue<VarIdTyMetadata>>,
    value_dependents: IndexMap<ValueId, IndexSet<ValueId>>,
    frames: IndexMap<FrameId, Frame>,
    next_id: u64,
    partial_cells: VecDeque<CellId>,
    compiled_cells: IndexMap<CellId, Arc<CompiledCell>>,
    compiled_cell_value_ids: HashMap<CellId, ValueId>,
    compiled_cell_cache: HashMap<CellExecKey, CellId>,
    artifacts: HashMap<CellExecKey, CellArtifact>,
    artifact_dependency_stack: Vec<IndexSet<VarId>>,
    dependencies: IndexSet<VarId>,
    errors: Vec<ExecError>,
}

struct PriorSolutionSpace {
    unsolved_vars: Option<IndexSet<Var>>,
    basis: SseBasis,
}

struct ExecPass<'a> {
    ast: &'a WorkspaceAst<VarIdTyMetadata>,
    tech: Technology,
    gds_imports: HashMap<VarId, (String, PathBuf)>,
    cell_states: IndexMap<CellId, CellState>,
    values: IndexMap<ValueId, DeferValue<VarIdTyMetadata>>,
    value_dependents: IndexMap<ValueId, IndexSet<ValueId>>,
    frames: IndexMap<FrameId, Frame>,
    nil_value: ValueId,
    seq_nil_value: ValueId,
    true_value: ValueId,
    false_value: ValueId,
    global_frame: FrameId,
    next_id: u64,
    // A stack of cells being evaluated.
    //
    // The first element of this stack is the root cell.
    // the last element of this stack is the current cell.
    partial_cells: VecDeque<CellId>,
    compiled_cells: IndexMap<CellId, Arc<CompiledCell>>,
    compiled_cell_value_ids: HashMap<CellId, ValueId>,
    compiled_cell_cache: HashMap<CellExecKey, CellId>,
    seed_artifacts: HashMap<CellExecKey, CellArtifact>,
    seed_continuations: HashMap<CellExecKey, CellContinuation>,
    artifacts: HashMap<CellExecKey, CellArtifact>,
    artifact_dependency_stack: Vec<IndexSet<VarId>>,
    artifact_hits: u64,
    artifact_misses: u64,
    continuation: Option<CellContinuation>,
    continuation_hits: u64,
    continuation_misses: u64,
    cancellation: Option<CancellationToken>,
    cancelled: bool,
    /// Cell declaration generated for a compiler entry point's invocation, and
    /// the cell it executed as. A call made directly by that cell is the
    /// top-level invocation rather than a nested instantiation, so it is named
    /// as if it were the entry point.
    entry_cell_var: Option<VarId>,
    entry_cell: Option<CellId>,
    /// Global cell and function declarations observed by this execution.
    dependencies: IndexSet<VarId>,
    errors: Vec<ExecError>,
}

fn add_scope(cell: &mut CompiledCell, state: &CellState, id: ScopeId, scope: &ExecScope) {
    if cell.scopes.contains_key(&id) {
        return;
    }
    if let Some(p) = scope.parent {
        add_scope(cell, state, p, &state.scopes[&p]);
        cell.scopes.get_mut(&p).unwrap().children.insert(id);
    }
    if let Some((p, _)) = scope.static_parent {
        add_scope(cell, state, p, &state.scopes[&p]);
    }
    cell.scopes.insert(
        id,
        CompiledScope {
            static_parent: scope.static_parent,
            bindings: Default::default(),
            children: Default::default(),
            name: scope.name.clone(),
            span: scope.span.clone(),
            emit: Vec::new(),
        },
    );
}

impl<'a> ExecPass<'a> {
    pub(crate) fn with_artifacts_cancellable(
        ast: &'a WorkspaceAst<VarIdTyMetadata>,
        tech: Technology,
        gds_imports: &[(String, PathBuf)],
        artifacts: Vec<CellArtifact>,
        continuations: Vec<CellContinuation>,
        cancellation: Option<&CancellationToken>,
    ) -> Self {
        let gds_imports = gds_imports
            .iter()
            .filter_map(|(name, path)| {
                let mut components = name.split("::").collect::<Vec<_>>();
                let cell_name = components.pop()?;
                let module = components
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let cell = ast
                    .get(&module)?
                    .ast
                    .decls
                    .iter()
                    .find_map(|decl| match decl {
                        Decl::Cell(cell) if cell.name.name == cell_name => Some(cell.metadata.1),
                        _ => None,
                    })?;
                Some((cell, (cell_name.to_owned(), path.clone())))
            })
            .collect();
        Self {
            ast,
            tech,
            gds_imports,
            cell_states: IndexMap::new(),
            values: IndexMap::from_iter([
                (1, DeferValue::Ready(Value::Nil)),
                (2, DeferValue::Ready(Value::Bool(true))),
                (3, DeferValue::Ready(Value::Bool(false))),
                (4, DeferValue::Ready(Value::SeqNil)),
            ]),
            value_dependents: IndexMap::new(),
            frames: IndexMap::from_iter([(
                5,
                Frame {
                    bindings: Default::default(),
                    parent: None,
                },
            )]),
            nil_value: 1,
            true_value: 2,
            false_value: 3,
            seq_nil_value: 4,
            global_frame: 5,
            next_id: 6,
            partial_cells: VecDeque::new(),
            compiled_cells: IndexMap::new(),
            compiled_cell_value_ids: HashMap::new(),
            compiled_cell_cache: HashMap::new(),
            seed_artifacts: artifacts
                .into_iter()
                .map(|artifact| {
                    let key = artifact
                        .cell_keys
                        .iter()
                        .find_map(|(key, cell)| (*cell == artifact.root).then(|| key.clone()))
                        .expect("cell artifacts contain their root key");
                    (key, artifact)
                })
                .collect(),
            seed_continuations: continuations
                .into_iter()
                .map(|continuation| (continuation.key.clone(), continuation))
                .collect(),
            artifacts: HashMap::new(),
            artifact_dependency_stack: Vec::new(),
            artifact_hits: 0,
            artifact_misses: 0,
            continuation: None,
            continuation_hits: 0,
            continuation_misses: 0,
            cancellation: cancellation.cloned(),
            cancelled: false,
            entry_cell_var: None,
            entry_cell: None,
            dependencies: IndexSet::new(),
            errors: Vec::new(),
        }
    }

    fn span(&self, loc: &DynLoc, span: cfgrammar::Span) -> Span {
        Span {
            path: self.cell_state(loc.cell).scopes[&loc.scope]
                .span
                .path
                .clone(),
            span,
        }
    }

    #[inline]
    fn cancellation_requested(&mut self) -> bool {
        if self
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            self.cancelled = true;
        }
        self.cancelled
    }

    pub(crate) fn lookup(&self, frame: FrameId, var: VarId) -> Option<ValueId> {
        let frame = self
            .frames
            .get(&frame)
            .expect("no frame found for frame ID");
        if let Some(val) = frame.bindings.get(&var) {
            Some(*val)
        } else {
            frame.parent.and_then(|frame| self.lookup(frame, var))
        }
    }

    pub(crate) fn execute_tracked(mut self, input: CompileInput<'a>) -> TrackedExecution {
        if self.cancellation_requested() {
            return self.complete(CompileOutput::FatalParseErrors);
        }
        self.declare_globals();
        if self.cancellation_requested() {
            return self.complete(CompileOutput::FatalParseErrors);
        }
        if input.cell.is_empty() {
            let output = CompileOutput::ExecErrors(ExecErrorCompileOutput {
                errors: vec![ExecError {
                    span: None,
                    cell: 0,
                    kind: ExecErrorKind::InvalidCell("<empty>".to_string()),
                }],
                output: None,
            });
            return self.complete(output);
        }
        let path = match input.cell[0] {
            "std" => {
                vec!["std".to_string()]
            }
            "lib" => input
                .cell
                .iter()
                .skip(1)
                .dropping_back(1)
                .map(|ident| ident.to_string())
                .collect_vec(),
            _ => input
                .cell
                .iter()
                .dropping_back(1)
                .map(|ident| ident.to_string())
                .collect_vec(),
        };
        if let Some((_, vid)) = self.ast.get(&path).and_then(|ast| {
            ast.ast.decls.iter().find_map(|d| match d {
                Decl::Cell(
                    v @ CellDecl {
                        name: Ident { name, .. },
                        ..
                    },
                ) if name == input.cell.last().unwrap() => Some(v.metadata.clone()),
                _ => None,
            })
        }) {
            let cell_id = match self.execute_cell(vid, input.args, None) {
                Ok(cell_id) => cell_id,
                Err(()) => {
                    let output = CompileOutput::ExecErrors(ExecErrorCompileOutput {
                        errors: std::mem::take(&mut self.errors),
                        output: None,
                    });
                    return self.complete(output);
                }
            };
            self.finish(cell_id)
        } else {
            let output = CompileOutput::ExecErrors(ExecErrorCompileOutput {
                errors: vec![ExecError {
                    span: None,
                    cell: 0, // TODO: don't use dummy cell ID
                    kind: ExecErrorKind::InvalidCell(input.cell.join("::")),
                }],
                output: None,
            });
            self.complete(output)
        }
    }

    /// Executes a cell invocation spliced into the root module by
    /// [`crate::parse::add_cell_invocation`]. The generated entry cell binds
    /// the invocation, so evaluating it runs the invocation through the
    /// ordinary expression evaluator; the cell it produced becomes the top.
    pub(crate) fn execute_invocation_tracked(
        mut self,
        invocation: &CellInvocation,
    ) -> TrackedExecution {
        if self.cancellation_requested() {
            return self.complete(CompileOutput::FatalParseErrors);
        }
        self.declare_globals();
        if self.cancellation_requested() {
            return self.complete(CompileOutput::FatalParseErrors);
        }
        let ast = self.ast;
        let root = &ast[&ModPath::new()];
        let entry = root
            .ast
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Cell(cell) if cell.name.name == invocation.entry_cell => Some(cell),
                _ => None,
            })
            .expect("generated entry cell should be declared");
        let value = entry
            .scope
            .stmts
            .iter()
            .find_map(|stmt| match stmt {
                Statement::LetBinding(binding) if binding.name.name == invocation.binding => {
                    Some(&binding.value)
                }
                _ => None,
            })
            .expect("generated entry cell should bind the invocation");
        // Reject a non-cell invocation statically, so the error lands on what
        // the caller wrote rather than surfacing from the executor.
        let ty = value.ty();
        if !matches!(ty, Ty::Cell(_) | Ty::Any) {
            let output = CompileOutput::StaticErrors(StaticErrorCompileOutput {
                errors: vec![StaticError {
                    span: Span {
                        path: root.path.clone(),
                        span: value.span(),
                    },
                    kind: StaticErrorKind::IncorrectTyCategory {
                        found: ty,
                        expected: "Cell".into(),
                    },
                }],
            });
            return self.complete(output);
        }

        self.entry_cell_var = Some(entry.metadata.1);
        let Ok(entry_id) = self.execute_cell(entry.metadata.1, Vec::new(), None) else {
            let output = CompileOutput::ExecErrors(ExecErrorCompileOutput {
                errors: std::mem::take(&mut self.errors),
                output: None,
            });
            return self.complete(output);
        };
        // Arguments are evaluated inside the generated cell, so an unsolvable
        // system there means an argument did not reduce to a constant.
        for error in &mut self.errors {
            if error.cell == entry_id
                && matches!(
                    error.kind,
                    ExecErrorKind::Underconstrained | ExecErrorKind::InconsistentConstraint(_)
                )
            {
                error.kind = ExecErrorKind::UnevaluatedCellArgument;
                error.span = Some(invocation.span());
            }
        }
        let value_id = self.cell_states[&entry_id].fields[&invocation.binding];
        let Some(DeferValue::Ready(Value::Cell(top))) = self.values.get(&value_id) else {
            self.errors.push(ExecError {
                span: Some(invocation.span()),
                cell: entry_id,
                kind: ExecErrorKind::NotACell,
            });
            let output = CompileOutput::ExecErrors(ExecErrorCompileOutput {
                errors: std::mem::take(&mut self.errors),
                output: None,
            });
            return self.complete(output);
        };
        let top = *top;
        // The entry cell is an implementation detail of the invocation.
        self.compiled_cells.shift_remove(&entry_id);
        self.finish(top)
    }

    /// Packages the executed cells into a compile output rooted at `top`.
    fn finish(mut self, top: CellId) -> TrackedExecution {
        let data = CompiledData {
            cells: std::mem::take(&mut self.compiled_cells),
            top,
            tech: self.tech.clone(),
        };
        let output = if self.errors.is_empty() {
            CompileOutput::Valid(data)
        } else {
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                errors: std::mem::take(&mut self.errors),
                output: Some(data),
            })
        };
        self.complete(output)
    }

    fn complete(mut self, output: CompileOutput) -> TrackedExecution {
        self.cancellation_requested();
        TrackedExecution {
            output,
            dependencies: std::mem::take(&mut self.dependencies),
            artifacts: std::mem::take(&mut self.artifacts).into_values().collect(),
            continuation: self.continuation.take(),
            artifact_hits: self.artifact_hits,
            artifact_misses: self.artifact_misses,
            continuation_hits: self.continuation_hits,
            continuation_misses: self.continuation_misses,
            cancelled: self.cancelled,
        }
    }

    fn observe_dependency(&mut self, dependency: VarId) {
        self.dependencies.insert(dependency);
        for dependencies in &mut self.artifact_dependency_stack {
            dependencies.insert(dependency);
        }
    }

    fn import_artifact(&mut self, artifact: &CellArtifact) -> Option<CellId> {
        if artifact.cells.values().any(|cell| {
            cell.objects.values().any(|object| {
                matches!(object, SolvedValue::Instance(instance)
                    if !artifact.cells.contains_key(&instance.cell))
            })
        }) {
            return None;
        }

        let cell_values = artifact
            .cells
            .keys()
            .copied()
            .map(|cell| {
                let value = self.new_ready_value(Value::Cell(cell));
                (cell, value)
            })
            .collect::<HashMap<_, _>>();
        for (cell, value) in &cell_values {
            self.compiled_cell_value_ids.insert(*cell, *value);
        }
        for (cell_id, cell) in &artifact.cells {
            if let Some(existing) = self.compiled_cells.get(cell_id)
                && !Arc::ptr_eq(existing, cell)
            {
                return None;
            }
            self.compiled_cells.insert(*cell_id, cell.clone());
        }
        for (key, cell) in &artifact.cell_keys {
            self.compiled_cell_cache.insert(key.clone(), *cell);
        }
        for dependency in &artifact.dependencies {
            self.observe_dependency(*dependency);
        }
        for error in &artifact.errors {
            self.errors.push(error.clone());
        }
        Some(artifact.root)
    }

    fn make_artifact(
        &self,
        root_key: &CellExecKey,
        root: CellId,
        dependencies: IndexSet<VarId>,
        error_start: usize,
    ) -> CellArtifact {
        let mut reachable = IndexSet::new();
        let mut pending = vec![root];
        while let Some(cell_id) = pending.pop() {
            if !reachable.insert(cell_id) {
                continue;
            }
            for object in self.compiled_cells[&cell_id].objects.values() {
                if let SolvedValue::Instance(instance) = object {
                    pending.push(instance.cell);
                }
            }
        }
        let cells = self
            .compiled_cells
            .iter()
            .filter(|(cell, _)| reachable.contains(*cell))
            .map(|(cell, value)| (*cell, value.clone()))
            .collect();
        let mut cell_keys = self
            .compiled_cell_cache
            .iter()
            .filter(|(_, cell)| reachable.contains(*cell))
            .map(|(key, cell)| (key.clone(), *cell))
            .collect::<HashMap<_, _>>();
        cell_keys.insert(root_key.clone(), root);
        CellArtifact {
            cell_keys,
            cells,
            root,
            dependencies,
            errors: self.errors[error_start..].to_vec(),
        }
    }

    fn snapshot_root_state(&self) -> RootExecutionState {
        RootExecutionState {
            cell_states: self.cell_states.clone(),
            values: self.values.clone(),
            value_dependents: self.value_dependents.clone(),
            frames: self.frames.clone(),
            next_id: self.next_id,
            partial_cells: self.partial_cells.clone(),
            compiled_cells: self.compiled_cells.clone(),
            compiled_cell_value_ids: self.compiled_cell_value_ids.clone(),
            compiled_cell_cache: self.compiled_cell_cache.clone(),
            artifacts: self.artifacts.clone(),
            artifact_dependency_stack: self.artifact_dependency_stack.clone(),
            dependencies: self.dependencies.clone(),
            errors: self.errors.clone(),
        }
    }

    fn restore_root_state(
        &mut self,
        state: RootExecutionState,
        global_value_limit: ValueId,
    ) -> bool {
        if self.next_id != global_value_limit {
            return false;
        }
        let current_globals = std::mem::take(&mut self.values);
        let current_global_frame = self
            .frames
            .get(&self.global_frame)
            .expect("global frame exists after declaration")
            .clone();

        self.cell_states = state.cell_states;
        self.values = state.values;
        self.value_dependents = state.value_dependents;
        self.frames = state.frames;
        self.next_id = state.next_id;
        self.partial_cells = state.partial_cells;
        self.compiled_cells = state.compiled_cells;
        self.compiled_cell_value_ids = state.compiled_cell_value_ids;
        self.compiled_cell_cache = state.compiled_cell_cache;
        self.artifacts = state.artifacts;
        self.artifact_dependency_stack = state.artifact_dependency_stack;
        self.dependencies = state.dependencies;
        self.errors = state.errors;

        // Function and cell declarations must come from the current typed AST.
        // Their runtime value IDs are deterministic while the declaration set
        // is unchanged, which is checked by `global_value_limit` above.
        for (id, value) in current_globals {
            if id < global_value_limit {
                self.values.insert(id, value);
            }
        }
        self.frames.insert(self.global_frame, current_global_frame);
        for state in self.cell_states.values_mut() {
            state
                .solver
                .replace_cancellation(self.cancellation.as_ref());
        }
        true
    }

    fn eval_top_level_statements(
        &mut self,
        cell_id: CellId,
        frame: FrameId,
        scope: ScopeId,
        statements: &[Statement<Substr, VarIdTyMetadata>],
        mut seq_num: SeqNum,
    ) -> Result<SeqNum, ()> {
        for statement in statements {
            if self.cancellation_requested() {
                return Err(());
            }
            let loc = DynLoc {
                cell: cell_id,
                frame,
                scope,
                seq_num,
            };
            match statement {
                Statement::LetBinding(binding) => {
                    let value = self.visit_expr(loc, &binding.value);
                    self.frames
                        .get_mut(&frame)
                        .expect("cell frame exists")
                        .bindings
                        .insert(binding.metadata, value);
                    self.cell_state_mut(cell_id)
                        .fields
                        .insert(binding.name.name.to_string(), value);
                    self.cell_state_mut(cell_id)
                        .scopes
                        .get_mut(&scope)
                        .expect("cell root scope exists")
                        .bindings
                        .insert(seq_num, (binding.name.name.to_string(), value));
                    seq_num = seq_num.next();
                }
                Statement::Expr { value, .. } => {
                    self.visit_expr(loc, value);
                }
                Statement::ForLoop(for_loop) => {
                    self.eval_for_loop(loc, for_loop);
                }
            }
        }
        Ok(seq_num)
    }

    fn settle_cell(
        &mut self,
        cell_id: CellId,
        mut prior_solution: Option<PriorSolutionSpace>,
    ) -> Result<(), ()> {
        while {
            if self.cancellation_requested() {
                return Err(());
            }
            let state = self.cell_state(cell_id);
            !state.deferred.is_empty() || !state.solver.fully_solved()
        } {
            let mut progress = false;
            while let Some(value) = {
                let state = self.cell_state_mut(cell_id);
                state.deferred.pop()
            } {
                if self.cancellation_requested() {
                    return Err(());
                }
                progress |= self.eval_partial(cell_id, value)?;
            }

            let state = self.cell_state_mut(cell_id);
            state.solve_iters += 1;
            state.solver.solve();
            if state.solver.was_cancelled() {
                return Err(());
            }
            progress = !state.solver.updated_vars().is_empty() || progress;
            let update_var_dependents = |state: &mut CellState| {
                for var in state.solver.updated_vars().clone() {
                    if let Some(dependents) = state.var_dependents.get(&var) {
                        for dependent in dependents.clone() {
                            state.deferred.insert(dependent);
                        }
                    }
                }
                state.solver.clear_updated_vars();
            };
            update_var_dependents(state);

            if !progress {
                let should_record_solution_space =
                    self.cell_state(cell_id).unsolved_vars.is_none() || prior_solution.is_some();
                if should_record_solution_space {
                    let state = self.cell_state_mut(cell_id);
                    let new_unsolved = state.solver.unsolved_vars().clone();
                    let (mut unsolved, basis, already_reported) =
                        if let Some(prior) = prior_solution.take() {
                            let already_reported = prior.unsolved_vars.is_some();
                            let mut unsolved = prior.unsolved_vars.unwrap_or_default();
                            unsolved.extend(new_unsolved);
                            let basis = match prior.basis {
                                SseBasis::Nullspace(mut previous) => {
                                    let Some(current) = state.solver.sparse_nullspace_vecs() else {
                                        return Err(());
                                    };
                                    previous.extend(current);
                                    SseBasis::Nullspace(previous)
                                }
                                SseBasis::Rowspace(mut previous) => {
                                    previous.extend(state.solver.rowspace_vecs());
                                    SseBasis::Rowspace(previous)
                                }
                            };
                            (unsolved, basis, already_reported)
                        } else {
                            let basis = match state.solver.sparse_nullspace_vecs() {
                                Some(vectors) => SseBasis::Nullspace(vectors),
                                None => SseBasis::Rowspace(state.solver.rowspace_vecs()),
                            };
                            (new_unsolved, basis, false)
                        };
                    let span = unsolved
                        .iter()
                        .find_map(|var| state.var_span_map.get(var).cloned());
                    state.unsolved_vars = Some(std::mem::take(&mut unsolved));
                    state.sse_basis = basis;
                    if !already_reported {
                        self.errors.push(ExecError {
                            span,
                            cell: cell_id,
                            kind: ExecErrorKind::Underconstrained,
                        });
                    }
                }
                let mut constraint_added = false;
                let state = self.cell_state_mut(cell_id);
                while let Some(FallbackConstraint {
                    constraint,
                    span,
                    initial_condition,
                    ..
                }) = state.fallback_constraints.pop()
                {
                    if constraint.coeffs.iter().any(|(coefficient, var)| {
                        coefficient.abs() > 1e-6 && !state.solver.is_solved(*var)
                    }) {
                        state.fallback_constraints_used.push(UsedFallback {
                            constraint: constraint.clone(),
                            span: span.clone(),
                            initial_condition,
                        });
                        let constraint_id = state.solver.constrain_eq0(constraint);
                        state.constraint_span_map.insert(constraint_id, span);
                        constraint_added = true;
                        break;
                    }
                }
                if !constraint_added {
                    state.solver.force_solution();
                    update_var_dependents(state);
                }
            }
        }

        // A resumed suffix may add new inconsistent or off-grid values. Rebuild
        // these diagnostics from the complete retained solver state so each
        // constraint/variable is reported exactly once.
        self.errors.retain(|error| {
            error.cell != cell_id
                || !matches!(
                    error.kind,
                    ExecErrorKind::InconsistentConstraint(_) | ExecErrorKind::OffGrid(_)
                )
        });
        for constraint in self
            .cell_state(cell_id)
            .solver
            .inconsistent_constraints()
            .clone()
        {
            let span = self
                .cell_state(cell_id)
                .constraint_span_map
                .get(&constraint)
                .cloned();
            self.errors.push(ExecError {
                span,
                cell: cell_id,
                kind: ExecErrorKind::InconsistentConstraint(constraint),
            });
        }
        for var in self.cell_state(cell_id).solver.off_grid_vars().clone() {
            let span = self.cell_state(cell_id).var_span_map.get(&var).cloned();
            self.errors.push(ExecError {
                span,
                cell: cell_id,
                kind: ExecErrorKind::OffGrid(var),
            });
        }
        Ok(())
    }

    pub(crate) fn execute_cell(
        &mut self,
        cell: VarId,
        args: Vec<CellArg>,
        scope_name: Option<String>,
    ) -> Result<CellId, ()> {
        if self.cancellation_requested() {
            return Err(());
        }
        if self.entry_cell_var != Some(cell) {
            self.observe_dependency(cell);
        }
        let cache_key = CellExecKey {
            cell,
            args: args.iter().map(CellArgKey::from).collect(),
            scope_name: scope_name.clone(),
        };
        if let Some(cell_id) = self.compiled_cell_cache.get(&cache_key) {
            return Ok(*cell_id);
        }
        if self.entry_cell_var != Some(cell)
            && let Some(artifact) = self.seed_artifacts.remove(&cache_key)
            && let Some(cell_id) = self.import_artifact(&artifact)
        {
            self.artifact_hits += 1;
            self.artifacts.insert(cache_key, artifact);
            return Ok(cell_id);
        }
        if self.entry_cell_var != Some(cell) {
            self.artifact_misses += 1;
        }
        if let Some((declared_name, path)) = self.gds_imports.get(&cell).cloned() {
            if !args.is_empty() {
                self.errors.push(ExecError {
                    span: None,
                    cell: 0,
                    kind: ExecErrorKind::InvalidCellArity {
                        expected: 0,
                        found: args.len(),
                    },
                });
                return Err(());
            }
            let error_start = self.errors.len();
            self.artifact_dependency_stack
                .push(IndexSet::from_iter([cell]));
            let cell_id = match self.execute_gds_cell(&cache_key, &declared_name, &path, scope_name)
            {
                Ok(cell_id) => cell_id,
                Err(()) => {
                    self.artifact_dependency_stack.pop();
                    return Err(());
                }
            };
            self.compiled_cell_cache.insert(cache_key.clone(), cell_id);
            let dependencies = self
                .artifact_dependency_stack
                .pop()
                .expect("GDS cell artifact dependency frame exists");
            let artifact = self.make_artifact(&cache_key, cell_id, dependencies, error_start);
            self.artifacts.insert(cache_key, artifact);
            return Ok(cell_id);
        }
        let mut frame = Frame {
            bindings: Default::default(),
            parent: Some(self.global_frame),
        };
        let cell_decl = self.values[&self.lookup(self.global_frame, cell).unwrap()]
            .as_ref()
            .unwrap_ready()
            .as_ref()
            .unwrap_cell_fn()
            .clone();
        if args.len() != cell_decl.args.len() {
            self.errors.push(ExecError {
                span: None,
                cell: 0,
                kind: ExecErrorKind::InvalidCellArity {
                    expected: cell_decl.args.len(),
                    found: args.len(),
                },
            });
            return Err(());
        }
        if let Some((index, (arg, decl))) = args
            .iter()
            .zip(&cell_decl.args)
            .enumerate()
            .find(|(_, (arg, decl))| !arg.matches_ty(&decl.metadata.1))
        {
            self.errors.push(ExecError {
                span: None,
                cell: 0,
                kind: ExecErrorKind::InvalidCellArgumentType {
                    index: index + 1,
                    expected: decl.metadata.1.clone(),
                    found: arg.ty_name().to_string(),
                },
            });
            return Err(());
        }
        let root_scope_name = scope_name.unwrap_or_else(|| format!("cell {}", cell_decl.name.name));
        let root_scope_id = ScopeId::semantic(None, &root_scope_name);
        let cell_id = stable_cell_id(&cache_key);
        let global_value_limit = self.next_id;
        let arguments = encoded_cell_arguments(&cell_decl);
        let statements = encoded_cell_statements(&cell_decl);
        let is_root = self.partial_cells.is_empty() && self.entry_cell_var.is_none();
        let candidate = is_root
            .then(|| self.seed_continuations.remove(&cache_key))
            .flatten();
        let reusable = candidate.filter(|continuation| {
            cell_decl.scope.tail.is_none()
                && continuation.arguments == arguments
                && statements.len() > continuation.statements.len()
                && statements
                    .iter()
                    .zip(&continuation.statements)
                    .all(|(current, previous)| current == previous)
                && cell_decl.scope.stmts[continuation.statements.len()..]
                    .iter()
                    .all(is_independent_emitted_rect)
        });

        let resumed = reusable.and_then(|continuation| {
            let prior_solution = PriorSolutionSpace {
                unsolved_vars: continuation
                    .state
                    .cell_states
                    .get(&continuation.cell_id)?
                    .unsolved_vars
                    .clone(),
                basis: continuation
                    .state
                    .cell_states
                    .get(&continuation.cell_id)?
                    .sse_basis
                    .clone(),
            };
            let old_statement_count = continuation.statements.len();
            let frame = continuation.frame;
            let next_seq_num = continuation.next_seq_num;
            let error_start = continuation.error_start;
            if continuation.cell_id != cell_id
                || continuation.root_scope != root_scope_id
                || !self.restore_root_state(continuation.state, continuation.global_value_limit)
            {
                return None;
            }
            self.cell_state_mut(cell_id)
                .scopes
                .get_mut(&root_scope_id)
                .expect("continued root scope exists")
                .span = Span {
                path: cell_decl.metadata.0.clone(),
                span: cell_decl.scope.span,
            };
            Some((
                frame,
                next_seq_num,
                error_start,
                old_statement_count,
                prior_solution,
            ))
        });

        let (frame_id, seq_num, error_start, prior_solution) = if let Some((
            frame_id,
            next_seq_num,
            error_start,
            old_statement_count,
            prior_solution,
        )) = resumed
        {
            self.continuation_hits += 1;
            let seq_num = self.eval_top_level_statements(
                cell_id,
                frame_id,
                root_scope_id,
                &cell_decl.scope.stmts[old_statement_count..],
                next_seq_num,
            )?;
            (frame_id, seq_num, error_start, Some(prior_solution))
        } else {
            if is_root {
                self.continuation_misses += 1;
            }
            let root_scope = ExecScope {
                parent: None,
                static_parent: None,
                span: Span {
                    path: cell_decl.metadata.0.clone(),
                    span: cell_decl.scope.span,
                },
                name: root_scope_name,
                bindings: Default::default(),
            };
            assert!(
                !self.compiled_cells.contains_key(&cell_id),
                "stable compiled-cell ID collision"
            );
            let error_start = self.errors.len();
            self.artifact_dependency_stack
                .push(IndexSet::from_iter([cell]));
            if self.entry_cell_var == Some(cell) {
                self.entry_cell = Some(cell_id);
            }
            self.partial_cells.push_back(cell_id);
            assert!(
                self.cell_states
                    .insert(
                        cell_id,
                        CellState {
                            solve_iters: 0,
                            solver: Solver::with_grid_cancellable(
                                self.tech.grid_step(),
                                self.cancellation.as_ref(),
                            ),
                            fields: Default::default(),
                            emit: Vec::new(),
                            object_emit: Vec::new(),
                            deferred: Default::default(),
                            scopes: IndexMap::from_iter([(root_scope_id, root_scope)]),
                            fallback_constraints: Default::default(),
                            fallback_constraints_used: Vec::new(),
                            sse_basis: SseBasis::Nullspace(Vec::new()),
                            root_scope: root_scope_id,
                            unsolved_vars: Default::default(),
                            objects: Default::default(),
                            constraint_span_map: IndexMap::new(),
                            var_span_map: IndexMap::new(),
                            var_dependents: IndexMap::new(),
                        }
                    )
                    .is_none()
            );
            for (value, declaration) in args.into_iter().zip(cell_decl.args.iter()) {
                let value_id = self.value_id();
                let value = Value::from_arg(&value);
                self.values.insert(value_id, DeferValue::Ready(value));
                frame.bindings.insert(declaration.metadata.0, value_id);
            }
            let frame_id = self.frame_id();
            self.frames.insert(frame_id, frame);
            let seq_num = self.eval_top_level_statements(
                cell_id,
                frame_id,
                root_scope_id,
                &cell_decl.scope.stmts,
                SeqNum::new(),
            )?;
            (frame_id, seq_num, error_start, None)
        };
        self.settle_cell(cell_id, prior_solution)?;

        if is_root && cell_decl.scope.tail.is_none() {
            let dependencies = self
                .artifact_dependency_stack
                .last()
                .cloned()
                .expect("root continuation has a dependency frame");
            self.continuation = Some(CellContinuation {
                key: cache_key.clone(),
                cell,
                arguments,
                statements,
                global_value_limit,
                cell_id,
                frame: frame_id,
                root_scope: root_scope_id,
                next_seq_num: seq_num,
                error_start,
                dependencies,
                state: self.snapshot_root_state(),
            });
        }

        self.partial_cells
            .pop_back()
            .expect("failed to pop cell id");

        let compiled_cell = Arc::new(self.emit(cell_id));
        assert!(self.compiled_cells.insert(cell_id, compiled_cell).is_none());
        self.compiled_cell_cache.insert(cache_key.clone(), cell_id);
        let dependencies = self
            .artifact_dependency_stack
            .pop()
            .expect("cell artifact dependency frame exists");
        if self.entry_cell_var != Some(cell) {
            let artifact = self.make_artifact(&cache_key, cell_id, dependencies, error_start);
            self.artifacts.insert(cache_key, artifact);
        }
        Ok(cell_id)
    }

    fn execute_gds_cell(
        &mut self,
        cache_key: &CellExecKey,
        declared_name: &str,
        path: &FsPath,
        scope_name: Option<String>,
    ) -> Result<CellId, ()> {
        let imported = match import_gds(path, declared_name, &self.tech) {
            Ok(imported) => imported,
            Err(error) => {
                self.errors.push(ExecError {
                    span: None,
                    cell: 0,
                    kind: ExecErrorKind::InvalidGds(error.to_string()),
                });
                return Err(());
            }
        };
        let cell_ids = (0..imported.structs.len())
            .map(|index| stable_cell_subgraph_id(cache_key, index as u64))
            .collect::<Vec<_>>();
        // Imported hierarchy has no source-level cell calls, but field access
        // through a child instance still needs a ready cell value.
        let cell_vids = cell_ids
            .iter()
            .map(|cell_id| {
                let vid = self.value_id();
                self.values
                    .insert(vid, DeferValue::Ready(Value::Cell(*cell_id)));
                self.compiled_cell_value_ids.insert(*cell_id, vid);
                vid
            })
            .collect::<Vec<_>>();
        let top_id = cell_ids[imported.top];
        for (structure_index, structure) in imported.structs.into_iter().enumerate() {
            let cell_id = cell_ids[structure_index];
            let root_name = if structure_index == imported.top {
                scope_name
                    .clone()
                    .unwrap_or_else(|| format!("cell {declared_name}"))
            } else {
                format!("cell {}", structure.name)
            };
            let root = ScopeId::semantic(None, &root_name);
            let span = Span {
                path: path.to_path_buf(),
                span: cfgrammar::Span::new(0, 0),
            };
            let mut objects = IndexMap::new();
            let mut emit = Vec::new();
            let mut named_objects: IndexMap<String, Vec<ObjectId>> = IndexMap::new();
            for (element_index, element) in structure.elements.into_iter().enumerate() {
                let id = self.object_id();
                let (value, field_name) = match element {
                    ImportedGdsElement::Rect {
                        layer,
                        name,
                        x0,
                        y0,
                        x1,
                        y1,
                    } => (
                        SolvedValue::Rect(Rect {
                            id,
                            layer: Some(layer),
                            x0: (x0, LinearExpr::from(x0)),
                            y0: (y0, LinearExpr::from(y0)),
                            x1: (x1, LinearExpr::from(x1)),
                            y1: (y1, LinearExpr::from(y1)),
                            construction: false,
                            span: None,
                        }),
                        Some(name.unwrap_or_else(|| format!("gds_rect_{element_index}"))),
                    ),
                    ImportedGdsElement::Polygon {
                        layer,
                        name,
                        points,
                    } => (
                        SolvedValue::Polygon(Polygon {
                            id,
                            layer,
                            points: points
                                .into_iter()
                                .map(|(x, y)| ((x, LinearExpr::from(x)), (y, LinearExpr::from(y))))
                                .collect(),
                            span: None,
                        }),
                        Some(name.unwrap_or_else(|| format!("gds_polygon_{element_index}"))),
                    ),
                    ImportedGdsElement::Path {
                        layer,
                        name,
                        width,
                        points,
                        begin_extension,
                        end_extension,
                    } => (
                        SolvedValue::Path(Path {
                            id,
                            layer,
                            width: (width, LinearExpr::from(width)),
                            points: points
                                .into_iter()
                                .map(|(x, y)| ((x, LinearExpr::from(x)), (y, LinearExpr::from(y))))
                                .collect(),
                            begin_extension: (begin_extension, LinearExpr::from(begin_extension)),
                            end_extension: (end_extension, LinearExpr::from(end_extension)),
                            span: None,
                        }),
                        Some(name.unwrap_or_else(|| format!("gds_path_{element_index}"))),
                    ),
                    ImportedGdsElement::Text { layer, text, x, y } => (
                        SolvedValue::Text(Text {
                            id,
                            layer,
                            text,
                            x,
                            y,
                            span: None,
                        }),
                        None,
                    ),
                    ImportedGdsElement::Instance {
                        cell,
                        x,
                        y,
                        angle,
                        reflect,
                    } => (
                        SolvedValue::Instance(SolvedInstance {
                            id,
                            x,
                            y,
                            x_expr: LinearExpr::from(x),
                            y_expr: LinearExpr::from(y),
                            angle: Rotation::try_from(angle).unwrap_or(Rotation::R0),
                            reflect,
                            construction: false,
                            cell: cell_ids[cell],
                            span: span.clone(),
                            cell_vid: cell_vids[cell],
                        }),
                        Some(format!("gds_inst_{element_index}")),
                    ),
                };
                objects.insert(id, value);
                emit.push((id, CompiledEmit { span: span.clone() }));
                if let Some(field_name) = field_name {
                    named_objects.entry(field_name).or_default().push(id);
                }
            }
            let fields = named_objects
                .into_iter()
                .map(|(name, ids)| {
                    let value = if ids.len() == 1 {
                        Arrayed::Elem(ids[0])
                    } else {
                        Arrayed::Array(ids.into_iter().map(Arrayed::Elem).collect())
                    };
                    (name, value)
                })
                .collect::<IndexMap<_, _>>();
            let bindings = fields
                .iter()
                .enumerate()
                .map(|(index, (name, value))| (SeqNum(index as u64), (name.clone(), value.clone())))
                .collect();
            let scopes = IndexMap::from_iter([(
                root,
                CompiledScope {
                    static_parent: None,
                    bindings,
                    children: IndexSet::new(),
                    name: root_name,
                    span: span.clone(),
                    emit,
                },
            )]);
            let cell = CompiledCell {
                scopes,
                root,
                fields,
                sse_basis: SseBasis::Nullspace(Vec::new()),
                objects,
                fallback_constraints_used: Vec::new(),
                unsolved_vars: IndexSet::new(),
                inconsistent_constraints: IndexSet::new(),
            };
            self.compiled_cells.insert(cell_id, Arc::new(cell));
        }
        Ok(top_id)
    }

    fn emit(&mut self, cell: CellId) -> CompiledCell {
        let state = self.cell_states.get(&cell).expect("cell not found");
        let mut emit_obj = |obj: &Object| -> SolvedValue {
            match obj {
                Object::Rect(rect) => {
                    let x0 = state
                        .solver
                        .eval_expr(&rect.x0)
                        .expect("rect x0 not solved");
                    let y0 = state
                        .solver
                        .eval_expr(&rect.y0)
                        .expect("rect y0 not solved");
                    let x1 = state
                        .solver
                        .eval_expr(&rect.x1)
                        .expect("rect x1 not solved");
                    let y1 = state
                        .solver
                        .eval_expr(&rect.y1)
                        .expect("rect y1 not solved");
                    if x0 > x1 {
                        self.errors.push(ExecError {
                            span: rect.span.clone(),
                            cell,
                            kind: ExecErrorKind::FlippedRect("x0 > x1".to_string()),
                        });
                    }
                    if y0 > y1 {
                        self.errors.push(ExecError {
                            span: rect.span.clone(),
                            cell,
                            kind: ExecErrorKind::FlippedRect("y0 > y1".to_string()),
                        });
                    }
                    SolvedValue::Rect(Rect {
                        id: rect.id,
                        layer: rect.layer.clone(),
                        x0: (x0, rect.x0.clone()),
                        y0: (y0, rect.y0.clone()),
                        x1: (x1, rect.x1.clone()),
                        y1: (y1, rect.y1.clone()),
                        construction: rect.construction,
                        span: rect.span.clone(),
                    })
                }
                Object::Polygon(polygon) => SolvedValue::Polygon(Polygon {
                    id: polygon.id,
                    layer: polygon.layer.clone(),
                    points: polygon
                        .points
                        .iter()
                        .map(|(x, y)| {
                            (
                                (
                                    state.solver.eval_expr(x).expect("polygon x not solved"),
                                    x.clone(),
                                ),
                                (
                                    state.solver.eval_expr(y).expect("polygon y not solved"),
                                    y.clone(),
                                ),
                            )
                        })
                        .collect(),
                    span: polygon.span.clone(),
                }),
                Object::Path(path) => SolvedValue::Path(Path {
                    id: path.id,
                    layer: path.layer.clone(),
                    width: (
                        state
                            .solver
                            .eval_expr(&path.width)
                            .expect("path width not solved"),
                        path.width.clone(),
                    ),
                    points: path
                        .points
                        .iter()
                        .map(|(x, y)| {
                            (
                                (
                                    state.solver.eval_expr(x).expect("path x not solved"),
                                    x.clone(),
                                ),
                                (
                                    state.solver.eval_expr(y).expect("path y not solved"),
                                    y.clone(),
                                ),
                            )
                        })
                        .collect(),
                    begin_extension: (
                        state
                            .solver
                            .eval_expr(&path.begin_extension)
                            .expect("path begin extension not solved"),
                        path.begin_extension.clone(),
                    ),
                    end_extension: (
                        state
                            .solver
                            .eval_expr(&path.end_extension)
                            .expect("path end extension not solved"),
                        path.end_extension.clone(),
                    ),
                    span: path.span.clone(),
                }),
                Object::Text(text) => SolvedValue::Text(Text {
                    id: text.id,
                    text: text.text.clone(),
                    layer: text.layer.clone(),
                    x: state.solver.eval_expr(&text.x).expect("text x not solved"),
                    y: state.solver.eval_expr(&text.y).expect("text x not solved"),
                    span: text.span.clone(),
                }),
                Object::Dimension(dim) => SolvedValue::Dimension(Dimension {
                    id: dim.id,
                    p: (
                        state.solver.eval_expr(&dim.p).expect("dim p not solved"),
                        dim.p.clone(),
                    ),
                    n: (
                        state.solver.eval_expr(&dim.n).expect("dim n not solved"),
                        dim.n.clone(),
                    ),
                    value: (
                        state
                            .solver
                            .eval_expr(&dim.value)
                            .expect("dim value not solved"),
                        dim.value.clone(),
                    ),
                    coord: (
                        state
                            .solver
                            .eval_expr(&dim.coord)
                            .expect("dim coord not solved"),
                        dim.coord.clone(),
                    ),
                    pstop: (
                        state
                            .solver
                            .eval_expr(&dim.pstop)
                            .expect("dim pstop not solved"),
                        dim.pstop.clone(),
                    ),
                    nstop: (
                        state
                            .solver
                            .eval_expr(&dim.nstop)
                            .expect("dim nstop not solved"),
                        dim.nstop.clone(),
                    ),
                    horiz: dim.horiz,
                    constraint: dim.constraint,
                    span: dim.span.clone(),
                }),
                Object::Inst(inst) => SolvedValue::Instance(SolvedInstance {
                    id: inst.id,
                    x: state.solver.eval_expr(&inst.x).expect("inst x not solved"),
                    y: state.solver.eval_expr(&inst.y).expect("inst y not solved"),
                    x_expr: inst.x.clone(),
                    y_expr: inst.y.clone(),
                    angle: inst.angle,
                    reflect: inst.reflect,
                    construction: inst.construction,
                    cell: *self.values[&inst.cell]
                        .as_ref()
                        .into_ready()
                        .expect("inst parent cell not ready")
                        .as_ref()
                        .into_cell()
                        .expect("inst parent not a cell"),
                    span: inst.span.clone(),
                    cell_vid: inst.cell,
                }),
            }
        };
        let emit_value = |vid: ValueId| -> Option<Arrayed<ObjectId>> {
            let value = &self.values[&vid];
            value
                .as_ref()
                .into_ready()
                .expect("emitted values must be ready")
                .obj_ids()
        };

        let mut ccell = CompiledCell {
            scopes: IndexMap::new(),
            root: state.root_scope,
            fields: IndexMap::new(),
            sse_basis: state.sse_basis.clone(),
            fallback_constraints_used: state.fallback_constraints_used.clone(),
            unsolved_vars: state.unsolved_vars.clone().unwrap_or_default(),
            inconsistent_constraints: state.solver.inconsistent_constraints().clone(),
            objects: IndexMap::new(),
        };
        for (id, scope) in state.scopes.iter() {
            add_scope(&mut ccell, state, *id, scope);
        }

        for (id, obj) in state.objects.iter() {
            ccell.objects.insert(*id, emit_obj(obj));
        }

        for emit in state.emit.iter() {
            let obj_id = emit_value(emit.value)
                .expect("failed to emit")
                .into_elem()
                .expect("emitted non-element object");
            ccell
                .scopes
                .get_mut(&emit.scope)
                .expect("cell scope not found for element emission")
                .emit
                .push((
                    obj_id,
                    CompiledEmit {
                        span: emit.span.clone(),
                    },
                ));
        }

        for emit in state.object_emit.iter() {
            ccell
                .scopes
                .get_mut(&emit.scope)
                .expect("cell scope not found for object emission")
                .emit
                .push((
                    emit.object,
                    CompiledEmit {
                        span: emit.span.clone(),
                    },
                ));
        }

        for (id, scope) in state.scopes.iter() {
            for (seq_num, (name, value)) in scope.bindings.iter() {
                if let Some(obj_id) = emit_value(*value) {
                    let scope = ccell.scopes.get_mut(id).expect("scope not found");
                    scope
                        .bindings
                        .insert(*seq_num, (name.clone(), obj_id.clone()));
                    if *id == ccell.root {
                        ccell.fields.insert(name.clone(), obj_id);
                    }
                }
            }
        }

        ccell
    }

    fn value_id(&mut self) -> ValueId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn frame_id(&mut self) -> FrameId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn object_id(&mut self) -> ObjectId {
        ObjectId(self.alloc_id())
    }

    fn cell_state(&self, cell_id: CellId) -> &CellState {
        self.cell_states
            .get(&cell_id)
            .expect("no cell state found for cell ID")
    }

    fn cell_state_mut(&mut self, cell_id: CellId) -> &mut CellState {
        self.cell_states
            .get_mut(&cell_id)
            .expect("no cell state found for cell ID")
    }

    fn declare_globals(&mut self) {
        for ast in self.ast.values() {
            for decl in &ast.ast.decls {
                if self.cancellation_requested() {
                    return;
                }
                match decl {
                    Decl::Fn(f) => {
                        let vid = self.value_id();
                        assert!(
                            self.values
                                .insert(vid, DeferValue::Ready(Value::Fn(f.clone())))
                                .is_none()
                        );
                        assert!(
                            self.frames
                                .get_mut(&self.global_frame)
                                .unwrap()
                                .bindings
                                .insert(f.metadata.1, vid)
                                .is_none()
                        );
                    }
                    Decl::Cell(c) => {
                        let vid = self.value_id();
                        assert!(
                            self.values
                                .insert(vid, DeferValue::Ready(Value::CellFn(c.clone())))
                                .is_none()
                        );
                        assert!(
                            self.frames
                                .get_mut(&self.global_frame)
                                .unwrap()
                                .bindings
                                .insert(c.metadata.1, vid)
                                .is_none()
                        );
                    }
                    _ => (),
                }
            }
        }
    }

    fn eval_for_loop(&mut self, loc: DynLoc, f: &ForLoop<Substr, VarIdTyMetadata>) {
        let seq = self.visit_expr(loc, &f.seq);
        self.new_deferred_value(loc, |_| {
            PartialEvalState::ForLoop(Box::new(PartialForLoop {
                for_loop: f.clone(),
                seq,
            }))
        });
    }

    fn eval_stmt(&mut self, loc: DynLoc, stmt: &Statement<Substr, VarIdTyMetadata>) {
        match stmt {
            Statement::LetBinding(binding) => {
                let value = self.visit_expr(loc, &binding.value);
                self.frames
                    .get_mut(&loc.frame)
                    .unwrap()
                    .bindings
                    .insert(binding.metadata, value);
                self.cell_state_mut(loc.cell)
                    .scopes
                    .get_mut(&loc.scope)
                    .unwrap()
                    .bindings
                    .insert(loc.seq_num, (binding.name.name.to_string(), value));
            }
            Statement::Expr { value, .. } => {
                self.visit_expr(loc, value);
            }
            Statement::ForLoop(f) => {
                self.eval_for_loop(loc, f);
            }
        }
    }

    /// Create a new execution scope.
    ///
    /// parent is the dynamic parent scope.
    fn create_exec_scope(
        &mut self,
        cell_id: CellId,
        parent: ScopeId,
        static_parent: Option<(ScopeId, SeqNum)>,
        name: String,
        span: Span,
    ) -> ScopeId {
        let id = ScopeId::semantic(Some(parent), &name);
        assert!(
            !self.cell_state(cell_id).scopes.contains_key(&id),
            "duplicate semantic scope ID for {name}"
        );
        self.cell_state_mut(cell_id).scopes.insert(
            id,
            ExecScope {
                parent: Some(parent),
                static_parent,
                name,
                span,
                bindings: Default::default(),
            },
        );
        id
    }

    /// Create a new execution scope.
    ///
    /// The scope is inserted in the execution trace at the location specified by `loc`.
    /// The static and dynamic parents of the new scope both point to `loc`.
    fn create_exec_scope_at_loc(&mut self, loc: DynLoc, name: String, span: Span) -> ScopeId {
        self.create_exec_scope(
            loc.cell,
            loc.scope,
            Some((loc.scope, loc.seq_num)),
            name,
            span,
        )
    }

    fn visit_scope_expr_inner(
        &mut self,
        cell_id: CellId,
        frame: FrameId,
        scope: ScopeId,
        s: &Scope<Substr, VarIdTyMetadata>,
    ) -> ValueId {
        let mut seq_num = SeqNum::new();
        for stmt in &s.stmts {
            if self.cancellation_requested() {
                return self.nil_value;
            }
            let loc = DynLoc {
                cell: cell_id,
                frame,
                scope,
                seq_num,
            };
            self.eval_stmt(loc, stmt);
            if matches!(stmt, Statement::LetBinding(_)) {
                seq_num = seq_num.next();
            }
        }

        let loc = DynLoc {
            cell: cell_id,
            frame,
            scope,
            seq_num,
        };
        s.tail
            .as_ref()
            .map(|tail| self.visit_expr(loc, tail))
            .unwrap_or(self.nil_value)
    }

    fn new_ready_value(&mut self, val: Value) -> ValueId {
        let vid = self.value_id();
        self.values.insert(vid, Defer::Ready(val));
        vid
    }

    // Takes in a closure so that the parent can be added to the stack first.
    fn new_deferred_value(
        &mut self,
        loc: DynLoc,
        state: impl FnOnce(&mut Self) -> PartialEvalState<VarIdTyMetadata>,
    ) -> ValueId {
        let vid = self.value_id();
        self.cell_state_mut(loc.cell).deferred.insert(vid);
        let state = state(self);
        self.values
            .insert(vid, Defer::Deferred(PartialEval { state, loc }));
        vid
    }

    fn visit_expr(&mut self, loc: DynLoc, expr: &Expr<Substr, VarIdTyMetadata>) -> ValueId {
        match expr {
            Expr::Nil(_) => self.nil_value,
            Expr::SeqNil(_) => self.seq_nil_value,
            Expr::FloatLiteral(f) => self.new_ready_value(Value::Linear(LinearExpr::from(f.value))),
            Expr::IntLiteral(i) => self.new_ready_value(Value::Int(i.value)),
            Expr::BoolLiteral(b) => {
                if b.value {
                    self.true_value
                } else {
                    self.false_value
                }
            }
            Expr::StringLiteral(s) => self.new_ready_value(Value::String(s.value.to_string())),
            Expr::IdentPath(path) => {
                if let Some(var_id) = path.metadata.0 {
                    self.lookup(loc.frame, var_id).unwrap()
                } else {
                    // must be an enum value
                    assert!(path.path.len() >= 2);
                    self.new_ready_value(Value::EnumValue(
                        path.path.last().unwrap().name.to_string(),
                    ))
                }
            }
            Expr::Emit(e) => {
                let value = self.visit_expr(loc, &e.value);
                let span = self.span(&loc, e.span);
                self.cell_state_mut(loc.cell).emit.push(Emit {
                    scope: loc.scope,
                    value,
                    span,
                });
                value
            }
            Expr::Call(c) => {
                if BUILTINS.contains(&c.func.path.last().unwrap().name.as_str()) {
                    self.new_deferred_value(loc, |this| {
                        PartialEvalState::Call(Box::new(PartialCallExpr {
                            expr: c.clone(),
                            state: CallExprState {
                                posargs: c
                                    .args
                                    .posargs
                                    .iter()
                                    .map(|arg| this.visit_expr(loc, arg))
                                    .collect(),
                                kwargs: c
                                    .args
                                    .kwargs
                                    .iter()
                                    .map(|arg| this.visit_expr(loc, &arg.value))
                                    .collect(),
                            },
                        }))
                    })
                } else {
                    self.observe_dependency(
                        c.metadata
                            .0
                            .expect("no var ID assigned to function being called"),
                    );
                    let arg_vals = c
                        .args
                        .posargs
                        .iter()
                        .map(|arg| self.visit_expr(loc, arg))
                        .collect_vec();
                    let val = &self.values[&self
                        .lookup(
                            loc.frame,
                            c.metadata
                                .0
                                .expect("no var ID assigned to function being called"),
                        )
                        .unwrap()]
                        .as_ref()
                        .unwrap_ready()
                        .as_ref();
                    match val {
                        ValueRef::Fn(val) => {
                            let mut call_frame = Frame {
                                bindings: Default::default(),
                                parent: Some(self.global_frame),
                            };
                            for (arg_val, arg_decl) in arg_vals.iter().zip(&val.args) {
                                call_frame.bindings.insert(arg_decl.metadata.0, *arg_val);
                            }
                            let new_scope = val.scope.clone();
                            let scope = self.create_exec_scope(
                                loc.cell,
                                loc.scope,
                                None,
                                format!(
                                    "{} fn {}",
                                    c.scope_order,
                                    c.func.path.iter().map(|ident| &ident.name).join("::")
                                ),
                                Span {
                                    path: val.metadata.0.clone(),
                                    span: val.scope.span,
                                },
                            );
                            let fid = self.frame_id();
                            self.frames.insert(fid, call_frame);
                            self.visit_scope_expr_inner(loc.cell, fid, scope, &new_scope)
                        }
                        ValueRef::CellFn(_) => self.new_deferred_value(loc, |this| {
                            PartialEvalState::Call(Box::new(PartialCallExpr {
                                expr: c.clone(),
                                state: CallExprState {
                                    posargs: arg_vals,
                                    kwargs: c
                                        .args
                                        .kwargs
                                        .iter()
                                        .map(|arg| this.visit_expr(loc, &arg.value))
                                        .collect(),
                                },
                            }))
                        }),
                        _ => {
                            self.errors.push(ExecError {
                                span: Some(self.span(&loc, c.span)),
                                cell: loc.cell,
                                kind: ExecErrorKind::InvalidType,
                            });
                            self.nil_value
                        }
                    }
                }
            }
            Expr::If(if_expr) => {
                let cond = self.visit_expr(loc, &if_expr.cond);
                self.new_deferred_value(loc, |_| {
                    PartialEvalState::If(Box::new(PartialIfExpr {
                        expr: (**if_expr).clone(),
                        state: IfExprState::Cond(cond),
                    }))
                })
            }
            Expr::Match(match_expr) => self.new_deferred_value(loc, |this| {
                let scrutinee = this.visit_expr(loc, &match_expr.scrutinee);
                PartialEvalState::Match(Box::new(PartialMatchExpr {
                    expr: (**match_expr).clone(),
                    state: MatchExprState::Scrutinee(scrutinee),
                }))
            }),
            Expr::Comparison(comparison_expr) => self.new_deferred_value(loc, |this| {
                let left = this.visit_expr(loc, &comparison_expr.left);
                let right = this.visit_expr(loc, &comparison_expr.right);
                PartialEvalState::Comparison(Box::new(PartialComparisonExpr {
                    expr: (**comparison_expr).clone(),
                    state: ComparisonExprState { left, right },
                }))
            }),
            Expr::Scope(s) => {
                let scope = self.create_exec_scope_at_loc(
                    loc,
                    format!("{} block", s.scope_order),
                    self.span(&loc, s.span),
                );
                self.visit_scope_expr_inner(loc.cell, loc.frame, scope, s)
            }
            Expr::FieldAccess(f) => self.new_deferred_value(loc, |this| {
                let base = this.visit_expr(loc, &f.base);
                PartialEvalState::FieldAccess(Box::new(PartialFieldAccessExpr {
                    expr: (**f).clone(),
                    state: FieldAccessExprState { base },
                }))
            }),
            Expr::IndexFieldAccess(f) => self.new_deferred_value(loc, |this| {
                let base = this.visit_expr(loc, &f.base);
                PartialEvalState::IndexFieldAccess(Box::new(PartialIndexFieldAccessExpr {
                    expr: (**f).clone(),
                    state: IndexFieldAccessExprState { base },
                }))
            }),
            Expr::Index(i) => self.new_deferred_value(loc, |this| {
                let base = this.visit_expr(loc, &i.base);
                let index = this.visit_expr(loc, &i.index);
                PartialEvalState::Index(Box::new(PartialIndexExpr {
                    expr: (**i).clone(),
                    state: IndexExprState { base, index },
                }))
            }),
            Expr::BinOp(b) => self.new_deferred_value(loc, |this| {
                let lhs = this.visit_expr(loc, &b.left);
                let rhs = this.visit_expr(loc, &b.right);
                PartialEvalState::BinOp(PartialBinOp {
                    lhs,
                    rhs,
                    op: b.op,
                    expr: b.clone(),
                })
            }),
            Expr::UnaryOp(u) => self.new_deferred_value(loc, |this| {
                let operand = this.visit_expr(loc, &u.operand);
                PartialEvalState::UnaryOp(PartialUnaryOp {
                    operand,
                    op: u.op,
                    expr: u.clone(),
                })
            }),
            Expr::Cast(cast) => self.new_deferred_value(loc, |this| {
                let value = this.visit_expr(loc, &cast.value);
                PartialEvalState::Cast(Box::new(PartialCastExpr {
                    expr: (**cast).clone(),
                    state: PartialCastState {
                        value,
                        ty: cast.metadata.clone(),
                    },
                }))
            }),
            Expr::Tuple(tuple) => self.new_deferred_value(loc, |this| {
                PartialEvalState::Tuple(PartialTupleExpr {
                    items: tuple
                        .items
                        .iter()
                        .map(|i| this.visit_expr(loc, i))
                        .collect(),
                })
            }),
        }
    }

    fn add_value_dependent(&mut self, vid: ValueId, dependent: ValueId) {
        self.value_dependents
            .entry(vid)
            .or_default()
            .insert(dependent);
    }

    fn add_var_dependent(&mut self, cell_id: CellId, var: Var, dependent: ValueId) {
        self.cell_state_mut(cell_id)
            .var_dependents
            .entry(var)
            .or_default()
            .insert(dependent);
    }

    /// Converts an evaluated value into a cell argument. `Ok(None)` means the
    /// value depends on solver variables that are not resolved yet, so the
    /// conversion should be retried; `Err` means the value can never be passed
    /// to a cell and an error has been recorded.
    pub fn cell_arg_from_value(
        &mut self,
        cell_id: CellId,
        dependent_vid: ValueId,
        val: &Value,
    ) -> Result<Option<CellArg>, ()> {
        Ok(match val {
            Value::Linear(v) => {
                if let Some(f) = self.cell_state_mut(cell_id).solver.eval_expr(v) {
                    Some(CellArg::Float(f))
                } else {
                    for (_, var) in v.coeffs.clone() {
                        self.add_var_dependent(cell_id, var, dependent_vid);
                    }
                    None
                }
            }
            Value::Int(i) => Some(CellArg::Int(*i)),
            Value::Bool(b) => Some(CellArg::Bool(*b)),
            Value::String(s) => Some(CellArg::String(s.clone())),
            Value::EnumValue(v) => Some(CellArg::Enum(v.clone())),
            Value::SeqNil => Some(CellArg::Seq(Vec::new())),
            Value::Seq(s) => {
                let mut args = Vec::with_capacity(s.len());
                for v in s.iter() {
                    match self.cell_arg_from_value(cell_id, dependent_vid, v)? {
                        Some(arg) => args.push(arg),
                        None => return Ok(None),
                    }
                }
                Some(CellArg::Seq(args))
            }
            v => {
                self.errors.push(ExecError {
                    span: None,
                    cell: cell_id,
                    kind: ExecErrorKind::UnsupportedCellArgument(v.kind_name().to_owned()),
                });
                return Err(());
            }
        })
    }

    fn eval_partial(&mut self, cell_id: CellId, vid: ValueId) -> Result<bool, ()> {
        if self.cancellation_requested() {
            return Err(());
        }
        let v = self.values.get(&vid);
        if v.is_none() {
            return Ok(false);
        }
        let vref = v.as_ref().unwrap();
        let mut vref = match &vref {
            Defer::Ready(_) => {
                if let Some(deps) = self.value_dependents.get(&vid) {
                    for dep_vid in deps.clone() {
                        self.cell_state_mut(cell_id).deferred.insert(dep_vid);
                    }
                }
                return Ok(true);
            }
            Defer::Deferred(v) => v.clone(),
        };
        let cell_id = vref.loc.cell;
        let state = self.cell_states.get_mut(&cell_id).unwrap();
        let progress = match &mut vref.state {
            PartialEvalState::Call(c) => match c.expr.func.path.last().unwrap().name.as_str() {
                f @ "crect" | f @ "rect" => {
                    let layer = if f == "crect" {
                        c.expr
                            .args
                            .kwargs
                            .iter()
                            .zip(c.state.kwargs.iter())
                            .find(|(k, _)| k.name.name == "layer")
                            .map(|(_, arg_vid)| {
                                if let Defer::Ready(layer) = &self.values[arg_vid] {
                                    Some(layer.as_ref().unwrap_string().clone())
                                } else {
                                    self.add_value_dependent(*arg_vid, vid);
                                    None
                                }
                            })
                    } else {
                        c.state.posargs.first().map(|arg_vid| {
                            if let Defer::Ready(layer) = &self.values[arg_vid] {
                                Some(layer.as_ref().unwrap_string().clone())
                            } else {
                                self.add_value_dependent(*arg_vid, vid);
                                None
                            }
                        })
                    };
                    let layer = match layer {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(l)) => Some(Some(l)),
                    };
                    if let Some(layer) = layer {
                        let id = self.object_id();
                        let span = self.span(&vref.loc, c.expr.span);
                        let state = self.cell_state_mut(cell_id);
                        let rect = Rect {
                            id,
                            layer,
                            x0: state.new_solver_var(&span).into(),
                            y0: state.new_solver_var(&span).into(),
                            x1: state.new_solver_var(&span).into(),
                            y1: state.new_solver_var(&span).into(),
                            construction: f == "crect",
                            span: Some(span.clone()),
                        };
                        state.objects.insert(rect.id, rect.clone().into());
                        state.emit.push(Emit {
                            scope: vref.loc.scope,
                            value: vid,
                            span,
                        });
                        self.values
                            .insert(vid, Defer::Ready(Value::Rect(rect.clone())));
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let lhs = self.value_id();
                            let (priority, initial_condition) = match kwarg.name.name.as_str() {
                                "x0" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.x0.clone())));
                                    (6, None)
                                }
                                "x0i" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.x0.clone())));
                                    (6, Some(RectInitialCondition::X0(rect.id)))
                                }
                                "x1" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.x1.clone())));
                                    (5, None)
                                }
                                "x1i" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.x1.clone())));
                                    (5, Some(RectInitialCondition::X1(rect.id)))
                                }
                                "y0" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.y0.clone())));
                                    (4, None)
                                }
                                "y0i" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.y0.clone())));
                                    (4, Some(RectInitialCondition::Y0(rect.id)))
                                }
                                "y1" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.y1.clone())));
                                    (3, None)
                                }
                                "y1i" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.y1.clone())));
                                    (3, Some(RectInitialCondition::Y1(rect.id)))
                                }
                                "w" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(
                                            rect.x1.clone() - rect.x0.clone(),
                                        )),
                                    );
                                    (2, None)
                                }
                                "h" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(
                                            rect.y1.clone() - rect.y0.clone(),
                                        )),
                                    );
                                    (1, None)
                                }
                                "layer" => {
                                    continue;
                                }
                                x => unreachable!("unsupported kwarg `{x}`"),
                            };
                            // Use the value expression's span (e.g. `100.` in
                            // `x1i=100.`) rather than the whole kwarg, so the GUI
                            // can rewrite just the value when persisting a
                            // solution-space-exploration drag.
                            let span = self.span(&vref.loc, kwarg.value.span());
                            self.new_deferred_value(vref.loc, |_| {
                                PartialEvalState::Constraint(PartialConstraint {
                                    lhs,
                                    rhs: *rhs,
                                    fallback: kwarg.name.name.ends_with('i'),
                                    priority,
                                    span,
                                    initial_condition,
                                })
                            });
                        }
                        true
                    } else {
                        false
                    }
                }
                "polygon" => {
                    if let (Defer::Ready(layer), Defer::Ready(point_spec)) = (
                        &self.values[&c.state.posargs[0]],
                        &self.values[&c.state.posargs[1]],
                    ) {
                        let layer = layer.as_ref().unwrap_string().clone();
                        let point_spec = point_spec.clone();
                        let span = self.span(&vref.loc, c.expr.span);
                        let points: Vec<(LinearExpr, LinearExpr)> = match point_spec {
                            Value::Int(count) => {
                                let Ok(count) = usize::try_from(count) else {
                                    self.errors.push(ExecError {
                                        span: Some(self.span(&vref.loc, c.expr.span)),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidPolygon,
                                    });
                                    return Err(());
                                };
                                if count < 3 {
                                    self.errors.push(ExecError {
                                        span: Some(self.span(&vref.loc, c.expr.span)),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidPolygon,
                                    });
                                    return Err(());
                                }
                                let state = self.cell_state_mut(cell_id);
                                (0..count)
                                    .map(|_| {
                                        (
                                            state.new_solver_var(&span).into(),
                                            state.new_solver_var(&span).into(),
                                        )
                                    })
                                    .collect()
                            }
                            _ => {
                                self.errors.push(ExecError {
                                    span: Some(self.span(&vref.loc, c.expr.span)),
                                    cell: cell_id,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        if points.len() < 3 {
                            self.errors.push(ExecError {
                                span: Some(self.span(&vref.loc, c.expr.span)),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidPolygon,
                            });
                            return Err(());
                        }
                        for kwarg in &c.expr.args.kwargs {
                            let coordinate = polygon_coordinate(kwarg.name.name.as_str())
                                .expect("polygon kwargs were statically validated");
                            if coordinate.index >= points.len() {
                                self.errors.push(ExecError {
                                    span: Some(self.span(&vref.loc, kwarg.name.span)),
                                    cell: cell_id,
                                    kind: ExecErrorKind::IndexOutOfBounds,
                                });
                                return Err(());
                            }
                        }

                        let id = self.object_id();
                        let polygon = Polygon {
                            id,
                            layer,
                            points,
                            span: Some(span.clone()),
                        };
                        let state = self.cell_state_mut(cell_id);
                        state.objects.insert(id, polygon.clone().into());
                        state.emit.push(Emit {
                            scope: vref.loc.scope,
                            value: vid,
                            span,
                        });
                        self.values
                            .insert(vid, Defer::Ready(Value::Polygon(polygon.clone())));
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let coordinate = polygon_coordinate(kwarg.name.name.as_str())
                                .expect("polygon kwargs were statically validated");
                            let expr = match coordinate.axis {
                                PolygonAxis::X => polygon.points[coordinate.index].0.clone(),
                                PolygonAxis::Y => polygon.points[coordinate.index].1.clone(),
                            };
                            let lhs = self.value_id();
                            self.values.insert(lhs, Defer::Ready(Value::Linear(expr)));
                            let span = self.span(&vref.loc, kwarg.value.span());
                            self.new_deferred_value(vref.loc, |_| {
                                PartialEvalState::Constraint(PartialConstraint {
                                    lhs,
                                    rhs: *rhs,
                                    fallback: coordinate.initial,
                                    priority: i32::MAX
                                        - i32::try_from(coordinate.index.saturating_mul(2))
                                            .unwrap_or(i32::MAX)
                                        - i32::from(matches!(coordinate.axis, PolygonAxis::Y)),
                                    span,
                                    initial_condition: coordinate.initial.then_some(
                                        match coordinate.axis {
                                            PolygonAxis::X => RectInitialCondition::PolygonX(
                                                polygon.id,
                                                coordinate.index,
                                            ),
                                            PolygonAxis::Y => RectInitialCondition::PolygonY(
                                                polygon.id,
                                                coordinate.index,
                                            ),
                                        },
                                    ),
                                })
                            });
                        }
                        true
                    } else {
                        for arg in &c.state.posargs {
                            if !self.values[arg].is_ready() {
                                self.add_value_dependent(*arg, vid);
                            }
                        }
                        false
                    }
                }
                "path" => {
                    if let (Defer::Ready(layer), Defer::Ready(point_spec)) = (
                        &self.values[&c.state.posargs[0]],
                        &self.values[&c.state.posargs[1]],
                    ) {
                        let layer = layer.as_ref().unwrap_string().clone();
                        let count = match point_spec.as_ref() {
                            ValueRef::Int(count) => usize::try_from(*count).ok(),
                            _ => {
                                self.errors.push(ExecError {
                                    span: Some(self.span(&vref.loc, c.expr.span)),
                                    cell: cell_id,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        let Some(count) = count.filter(|count| *count >= 2) else {
                            self.errors.push(ExecError {
                                span: Some(self.span(&vref.loc, c.expr.span)),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidPath,
                            });
                            return Err(());
                        };
                        for kwarg in &c.expr.args.kwargs {
                            let name = kwarg.name.name.as_str();
                            if let Some(coordinate) = polygon_coordinate(name)
                                && coordinate.index >= count
                            {
                                self.errors.push(ExecError {
                                    span: Some(self.span(&vref.loc, kwarg.name.span)),
                                    cell: cell_id,
                                    kind: ExecErrorKind::IndexOutOfBounds,
                                });
                                return Err(());
                            }
                        }

                        let id = self.object_id();
                        let span = self.span(&vref.loc, c.expr.span);
                        let has_begin_extension = c.expr.args.kwargs.iter().any(|kwarg| {
                            matches!(
                                kwarg.name.name.as_str(),
                                "begin_extension" | "begin_extensioni"
                            )
                        });
                        let has_end_extension = c.expr.args.kwargs.iter().any(|kwarg| {
                            matches!(kwarg.name.name.as_str(), "end_extension" | "end_extensioni")
                        });
                        let state = self.cell_state_mut(cell_id);
                        let width = state.new_solver_var(&span).into();
                        let begin_extension = if has_begin_extension {
                            state.new_solver_var(&span).into()
                        } else {
                            LinearExpr::from(0.)
                        };
                        let end_extension = if has_end_extension {
                            state.new_solver_var(&span).into()
                        } else {
                            LinearExpr::from(0.)
                        };
                        let points = (0..count)
                            .map(|_| {
                                (
                                    state.new_solver_var(&span).into(),
                                    state.new_solver_var(&span).into(),
                                )
                            })
                            .collect();
                        let path = Path {
                            id,
                            layer,
                            width,
                            points,
                            begin_extension,
                            end_extension,
                            span: Some(span.clone()),
                        };
                        state.objects.insert(id, path.clone().into());
                        state.emit.push(Emit {
                            scope: vref.loc.scope,
                            value: vid,
                            span,
                        });
                        self.values
                            .insert(vid, Defer::Ready(Value::Path(path.clone())));
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let name = kwarg.name.name.as_str();
                            let (expr, fallback, priority, initial_condition) = match name {
                                "width" | "widthi" => (
                                    path.width.clone(),
                                    name == "widthi",
                                    i32::MAX,
                                    (name == "widthi")
                                        .then_some(RectInitialCondition::PathWidth(path.id)),
                                ),
                                "begin_extension" | "begin_extensioni" => (
                                    path.begin_extension.clone(),
                                    name == "begin_extensioni",
                                    i32::MAX - 1,
                                    (name == "begin_extensioni").then_some(
                                        RectInitialCondition::PathBeginExtension(path.id),
                                    ),
                                ),
                                "end_extension" | "end_extensioni" => (
                                    path.end_extension.clone(),
                                    name == "end_extensioni",
                                    i32::MAX - 2,
                                    (name == "end_extensioni")
                                        .then_some(RectInitialCondition::PathEndExtension(path.id)),
                                ),
                                _ => {
                                    let coordinate = polygon_coordinate(name)
                                        .expect("path kwargs were statically validated");
                                    let expr = match coordinate.axis {
                                        PolygonAxis::X => path.points[coordinate.index].0.clone(),
                                        PolygonAxis::Y => path.points[coordinate.index].1.clone(),
                                    };
                                    let initial_condition =
                                        coordinate.initial.then_some(match coordinate.axis {
                                            PolygonAxis::X => RectInitialCondition::PathX(
                                                path.id,
                                                coordinate.index,
                                            ),
                                            PolygonAxis::Y => RectInitialCondition::PathY(
                                                path.id,
                                                coordinate.index,
                                            ),
                                        });
                                    (
                                        expr,
                                        coordinate.initial,
                                        i32::MAX
                                            - 3
                                            - i32::try_from(coordinate.index.saturating_mul(2))
                                                .unwrap_or(i32::MAX)
                                            - i32::from(matches!(coordinate.axis, PolygonAxis::Y)),
                                        initial_condition,
                                    )
                                }
                            };
                            let lhs = self.value_id();
                            self.values.insert(lhs, Defer::Ready(Value::Linear(expr)));
                            let span = self.span(&vref.loc, kwarg.value.span());
                            self.new_deferred_value(vref.loc, |_| {
                                PartialEvalState::Constraint(PartialConstraint {
                                    lhs,
                                    rhs: *rhs,
                                    fallback,
                                    priority,
                                    span,
                                    initial_condition,
                                })
                            });
                        }
                        true
                    } else {
                        for arg in &c.state.posargs {
                            if !self.values[arg].is_ready() {
                                self.add_value_dependent(*arg, vid);
                            }
                        }
                        false
                    }
                }
                "text" => {
                    let (mut args, unready): (Vec<_>, Vec<_>) =
                        c.state.posargs.iter().partition_map(|v| {
                            if let Defer::Ready(v) = &self.values[v] {
                                Either::Left(v)
                            } else {
                                Either::Right(*v)
                            }
                        });
                    if unready.is_empty() {
                        assert_eq!(args.len(), 4);
                        let id = object_id(&mut self.next_id);
                        let span = self.span(&vref.loc, c.expr.span);
                        let state = self.cell_states.get_mut(&cell_id).unwrap();
                        let y = args.pop().unwrap().as_ref().unwrap_linear().clone();
                        let x = args.pop().unwrap().as_ref().unwrap_linear().clone();
                        let layer = args.pop().unwrap().as_ref().unwrap_string().clone();
                        let text_val = args.pop().unwrap().as_ref().unwrap_string().clone();
                        let text = Text {
                            id,
                            text: text_val,
                            layer,
                            x,
                            y,
                            span: Some(span.clone()),
                        };
                        state.object_emit.push(ObjectEmit {
                            scope: vref.loc.scope,
                            object: text.id,
                            span,
                        });
                        state.objects.insert(text.id, text.clone().into());
                        self.values.insert(vid, Defer::Ready(Value::Nil));
                        true
                    } else {
                        for arg_vid in unready {
                            self.add_value_dependent(arg_vid, vid);
                        }
                        false
                    }
                }
                "bbox" => {
                    let arg = &self.values[&c.state.posargs[0]];
                    if let Some(val) = arg.get_ready() {
                        let span = self.span(&vref.loc, c.expr.span);
                        let r = match val {
                            Value::Inst(i) => {
                                if let Defer::Ready(cell) = &self.values[&i.cell] {
                                    let cell_id = cell.as_ref().unwrap_cell();
                                    Some(self.bbox(*cell_id).map(|r| {
                                        let r = r.transform(i.reflect, i.angle);
                                        Rect {
                                            id: r.id,
                                            layer: r.layer,
                                            x0: LinearExpr::from(r.x0) + i.x.clone(),
                                            y0: LinearExpr::from(r.y0) + i.y.clone(),
                                            x1: LinearExpr::from(r.x1) + i.x.clone(),
                                            y1: LinearExpr::from(r.y1) + i.y.clone(),
                                            construction: true,
                                            span: None,
                                        }
                                    }))
                                } else {
                                    self.add_value_dependent(i.cell, vid);
                                    None
                                }
                            }
                            Value::Cell(c) => Some(self.bbox(*c).map(|r| Rect {
                                id: r.id,
                                layer: r.layer,
                                x0: r.x0.into(),
                                y0: r.y0.into(),
                                x1: r.x1.into(),
                                y1: r.y1.into(),
                                construction: true,
                                span: None,
                            })),
                            _ => {
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: cell_id,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        if let Some(r) = r {
                            if let Some(r) = r {
                                let id = object_id(&mut self.next_id);
                                let state = self.cell_states.get_mut(&cell_id).unwrap();
                                let orect = Rect {
                                    id,
                                    layer: None,
                                    x0: r.x0,
                                    y0: r.y0,
                                    x1: r.x1,
                                    y1: r.y1,
                                    construction: true,
                                    span: Some(span.clone()),
                                };
                                state.objects.insert(orect.id, orect.clone().into());
                                state.emit.push(Emit {
                                    scope: vref.loc.scope,
                                    value: vid,
                                    span,
                                });
                                self.values.insert(vid, Defer::Ready(Value::Rect(orect)));
                                true
                            } else {
                                // default to a zero rectangle
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: cell_id,
                                    kind: ExecErrorKind::EmptyBbox,
                                });
                                let id = object_id(&mut self.next_id);
                                let state = self.cell_states.get_mut(&cell_id).unwrap();
                                let orect = Rect {
                                    id,
                                    layer: None,
                                    x0: 0.0.into(),
                                    y0: 0.0.into(),
                                    x1: 0.0.into(),
                                    y1: 0.0.into(),
                                    construction: true,
                                    span: Some(span),
                                };
                                state.objects.insert(orect.id, orect.clone().into());
                                self.values.insert(vid, Defer::Ready(Value::Rect(orect)));
                                true
                            }
                        } else {
                            false
                        }
                    } else {
                        self.add_value_dependent(c.state.posargs[0], vid);
                        false
                    }
                }
                "float" => {
                    let span = Span {
                        path: state.scopes[&vref.loc.scope].span.path.clone(),
                        span: c.expr.span,
                    };
                    let var = state.new_solver_var(&span);
                    self.values
                        .insert(vid, Defer::Ready(Value::Linear(LinearExpr::from(var))));
                    true
                }
                "eq" => {
                    if let (Defer::Ready(vl), Defer::Ready(vr)) = (
                        &self.values[&c.state.posargs[0]],
                        &self.values[&c.state.posargs[1]],
                    ) {
                        let expr = vl.as_ref().unwrap_linear().clone()
                            - vr.as_ref().unwrap_linear().clone();
                        let constraint = state.solver.constrain_eq0(expr);

                        state.constraint_span_map.insert(
                            constraint,
                            Span {
                                path: state.scopes[&vref.loc.scope].span.path.clone(),
                                span: c.expr.span,
                            },
                        );
                        self.values.insert(vid, Defer::Ready(Value::Nil));
                        true
                    } else {
                        self.add_value_dependent(c.state.posargs[0], vid);
                        self.add_value_dependent(c.state.posargs[1], vid);
                        false
                    }
                }
                "cons" => {
                    if let (Defer::Ready(head), Defer::Ready(tail)) = (
                        &self.values[&c.state.posargs[0]],
                        &self.values[&c.state.posargs[1]],
                    ) {
                        let val = match tail {
                            Value::SeqNil => {
                                let mut s = Seq::new();
                                s.push_back(head.clone());
                                s
                            }
                            Value::Seq(s) => {
                                // O(1) structural clone + O(log n) prepend (was O(n) deep
                                // clone + O(n) front-insert, making `range` O(n^2)).
                                let mut s = s.clone();
                                s.push_front(head.clone());
                                s
                            }
                            _ => {
                                let span = self.span(&vref.loc, c.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: cell_id,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        self.values.insert(vid, Defer::Ready(Value::Seq(val)));
                        true
                    } else {
                        self.add_value_dependent(c.state.posargs[0], vid);
                        self.add_value_dependent(c.state.posargs[1], vid);
                        false
                    }
                }
                "list" => {
                    let (ready, unready): (Vec<_>, Vec<_>) =
                        c.state.posargs.iter().partition_map(|v| {
                            if let Defer::Ready(v) = &self.values[v] {
                                Either::Left(v)
                            } else {
                                Either::Right(*v)
                            }
                        });
                    if unready.is_empty() {
                        self.values.insert(
                            vid,
                            Defer::Ready(Value::Seq(ready.iter().map(|v| (*v).clone()).collect())),
                        );
                        true
                    } else {
                        for arg_vid in unready {
                            self.add_value_dependent(arg_vid, vid);
                        }
                        false
                    }
                }
                "range_full" => {
                    if let (Defer::Ready(start), Defer::Ready(stop), Defer::Ready(step)) = (
                        &self.values[&c.state.posargs[0]],
                        &self.values[&c.state.posargs[1]],
                        &self.values[&c.state.posargs[2]],
                    ) {
                        if let (Value::Int(start), Value::Int(stop), Value::Int(step)) =
                            (start, stop, step)
                        {
                            let (start, stop, step) = (*start, *stop, *step);
                            // Build the whole `[Int]` in one O(n) pass (O(log n) pushes),
                            // avoiding the per-element interpreter overhead (frame, scope,
                            // deferred value) of the old recursive `cons` definition.
                            let mut seq = Seq::new();
                            if step > 0 {
                                let mut i = start;
                                while i < stop {
                                    if self.cancellation_requested() {
                                        return Err(());
                                    }
                                    seq.push_back(Value::Int(i));
                                    i += step;
                                }
                            }
                            self.values.insert(vid, Defer::Ready(Value::Seq(seq)));
                            true
                        } else {
                            let span = self.span(&vref.loc, c.expr.span);
                            self.errors.push(ExecError {
                                span: Some(span),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidType,
                            });
                            return Err(());
                        }
                    } else {
                        self.add_value_dependent(c.state.posargs[0], vid);
                        self.add_value_dependent(c.state.posargs[1], vid);
                        self.add_value_dependent(c.state.posargs[2], vid);
                        false
                    }
                }
                "head" => {
                    if let Defer::Ready(head) = &self.values[&c.state.posargs[0]] {
                        let val = match head {
                            Value::SeqNil => {
                                let span = self.span(&vref.loc, c.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: cell_id,
                                    kind: ExecErrorKind::HeadEmptyList,
                                });
                                return Err(());
                            }
                            Value::Seq(s) => {
                                if let Some(s) = s.front() {
                                    s.clone()
                                } else {
                                    let span = self.span(&vref.loc, c.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: cell_id,
                                        kind: ExecErrorKind::HeadEmptyList,
                                    });
                                    return Err(());
                                }
                            }
                            _ => {
                                let span = self.span(&vref.loc, c.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: cell_id,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        self.values.insert(vid, Defer::Ready(val));
                        true
                    } else {
                        self.add_value_dependent(c.state.posargs[0], vid);
                        false
                    }
                }
                "tail" => {
                    if let Defer::Ready(lst) = &self.values[&c.state.posargs[0]] {
                        let val = match lst {
                            Value::SeqNil => {
                                let span = self.span(&vref.loc, c.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: cell_id,
                                    kind: ExecErrorKind::TailEmptyList,
                                });
                                return Err(());
                            }
                            Value::Seq(s) => {
                                if !s.is_empty() {
                                    // Drop the head: O(1) structural clone + O(log n)
                                    // pop_front (was O(n) `s[1..].to_vec()`, which made
                                    // `tail`-recursion such as `std::last` O(n^2)).
                                    let mut s = s.clone();
                                    s.pop_front();
                                    Value::Seq(s)
                                } else {
                                    let span = self.span(&vref.loc, c.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: cell_id,
                                        kind: ExecErrorKind::TailEmptyList,
                                    });
                                    return Err(());
                                }
                            }
                            _ => {
                                let span = self.span(&vref.loc, c.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: cell_id,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        self.values.insert(vid, Defer::Ready(val));
                        true
                    } else {
                        self.add_value_dependent(c.state.posargs[0], vid);
                        false
                    }
                }
                "dimension" => {
                    let (mut args, unready): (Vec<_>, Vec<_>) =
                        c.state.posargs.iter().partition_map(|v| {
                            if let Defer::Ready(v) = &self.values[v] {
                                Either::Left(v)
                            } else {
                                Either::Right(*v)
                            }
                        });
                    if unready.is_empty() {
                        assert_eq!(args.len(), 7);
                        let id = object_id(&mut self.next_id);
                        let span = self.span(&vref.loc, c.expr.span);
                        let state = self.cell_states.get_mut(&cell_id).unwrap();
                        let horiz = *args.pop().unwrap().as_ref().unwrap_bool();
                        let mut arg = || args.pop().unwrap().as_ref().unwrap_linear().clone();
                        let (nstop, pstop, coord, value, n, p) =
                            (arg(), arg(), arg(), arg(), arg(), arg());
                        let expr = p.clone() - n.clone() - value.clone();
                        let constraint = state.solver.constrain_eq0(expr);
                        let dim = Dimension {
                            id,
                            horiz,
                            nstop,
                            pstop,
                            coord,
                            value,
                            n,
                            p,
                            constraint,
                            span: Some(span.clone()),
                        };
                        state.constraint_span_map.insert(constraint, span.clone());
                        state.object_emit.push(ObjectEmit {
                            scope: vref.loc.scope,
                            object: dim.id,
                            span,
                        });
                        state.objects.insert(dim.id, dim.clone().into());
                        self.values.insert(vid, Defer::Ready(Value::Nil));
                        true
                    } else {
                        for arg_vid in unready {
                            self.add_value_dependent(arg_vid, vid);
                        }
                        false
                    }
                }
                "inst" => {
                    let refl = c
                        .expr
                        .args
                        .kwargs
                        .iter()
                        .zip(c.state.kwargs.iter())
                        .find_map(|(kwarg, arg_vid)| {
                            if kwarg.name.name == "reflect" {
                                Some(if let Defer::Ready(refl) = &self.values[arg_vid] {
                                    Some(*refl.as_ref().unwrap_bool())
                                } else {
                                    self.add_value_dependent(*arg_vid, vid);
                                    None
                                })
                            } else {
                                None
                            }
                        });
                    let refl = match refl {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(l)) => Some(Some(l)),
                    };
                    let angle = c
                        .expr
                        .args
                        .kwargs
                        .iter()
                        .zip(c.state.kwargs.iter())
                        .find_map(|(kwarg, arg_vid)| {
                            if kwarg.name.name == "angle" {
                                let span = self.span(&vref.loc, kwarg.value.span());
                                Some(if let Defer::Ready(refl) = &self.values[arg_vid] {
                                    Some(match ((*refl.as_ref().unwrap_int() % 360) + 360) % 360 {
                                        0 => Rotation::R0,
                                        90 => Rotation::R90,
                                        180 => Rotation::R180,
                                        270 => Rotation::R270,
                                        _ => {
                                            self.errors.push(ExecError {
                                                span: Some(span),
                                                cell: cell_id,
                                                kind: ExecErrorKind::InvalidRotation,
                                            });
                                            Rotation::R0
                                        }
                                    })
                                } else {
                                    self.add_value_dependent(*arg_vid, vid);
                                    None
                                })
                            } else {
                                None
                            }
                        });
                    let angle = match angle {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(l)) => Some(Some(l)),
                    };
                    let construction = c
                        .expr
                        .args
                        .kwargs
                        .iter()
                        .zip(c.state.kwargs.iter())
                        .find_map(|(kwarg, arg_vid)| {
                            if kwarg.name.name == "construction" {
                                Some(if let Defer::Ready(v) = &self.values[arg_vid] {
                                    Some(*v.as_ref().unwrap_bool())
                                } else {
                                    self.add_value_dependent(*arg_vid, vid);
                                    None
                                })
                            } else {
                                None
                            }
                        });
                    let construction = match construction {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(v)) => Some(Some(v)),
                    };
                    if let (Some(refl), Some(angle), Some(construction)) =
                        (refl, angle, construction)
                    {
                        let id = object_id(&mut self.next_id);
                        let span = self.span(&vref.loc, c.expr.span);
                        let state = self.cell_states.get_mut(&cell_id).unwrap();
                        let inst = Instance {
                            id,
                            x: state.new_solver_var(&span).into(),
                            y: state.new_solver_var(&span).into(),
                            cell: *c.state.posargs.first().unwrap(),
                            reflect: refl.unwrap_or_default(),
                            angle: angle.unwrap_or_default(),
                            construction: construction.unwrap_or_default(),
                            span: span.clone(),
                        };
                        state.emit.push(Emit {
                            scope: vref.loc.scope,
                            value: vid,
                            span,
                        });
                        state.objects.insert(inst.id, inst.clone().into());
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let lhs = self.value_id();
                            let (priority, initial_condition) = match kwarg.name.name.as_str() {
                                "x" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(inst.x.clone())));
                                    (2, None)
                                }
                                "xi" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(inst.x.clone())));
                                    (2, Some(RectInitialCondition::InstanceX(inst.id)))
                                }
                                "y" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(inst.y.clone())));
                                    (1, None)
                                }
                                "yi" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(inst.y.clone())));
                                    (1, Some(RectInitialCondition::InstanceY(inst.id)))
                                }
                                _ => continue,
                            };
                            // Use the value expression's span (e.g. `100.` in
                            // `x1i=100.`) rather than the whole kwarg, so the GUI
                            // can rewrite just the value when persisting a
                            // solution-space-exploration drag.
                            let span = self.span(&vref.loc, kwarg.value.span());
                            self.new_deferred_value(vref.loc, |_| {
                                PartialEvalState::Constraint(PartialConstraint {
                                    lhs,
                                    rhs: *rhs,
                                    fallback: kwarg.name.name.ends_with('i'),
                                    priority,
                                    span,
                                    initial_condition,
                                })
                            });
                        }
                        self.values.insert(vid, Defer::Ready(Value::Inst(inst)));
                        true
                    } else {
                        false
                    }
                }
                _ => {
                    // Must be calling a cell generator.
                    // User functions are never deferred.
                    let mut arg_vals = Vec::with_capacity(c.state.posargs.len());
                    let mut unready = Vec::new();
                    for arg_vid in c.state.posargs.iter() {
                        match self.values[arg_vid].clone() {
                            Defer::Ready(v) => {
                                match self.cell_arg_from_value(cell_id, *arg_vid, &v)? {
                                    Some(arg) => arg_vals.push(arg),
                                    None => unready.push(*arg_vid),
                                }
                            }
                            _ => unready.push(*arg_vid),
                        }
                    }
                    if unready.is_empty() {
                        let scope_name = (self.entry_cell != Some(cell_id)).then(|| {
                            format!(
                                "{} cell {}",
                                c.expr.scope_order,
                                c.expr.func.path.iter().map(|ident| &ident.name).join("::")
                            )
                        });
                        let cell =
                            self.execute_cell(c.expr.metadata.0.unwrap(), arg_vals, scope_name)?;
                        self.compiled_cell_value_ids.entry(cell).or_insert(vid);
                        self.values.insert(vid, Defer::Ready(Value::Cell(cell)));
                        true
                    } else {
                        for arg_vid in unready {
                            self.add_value_dependent(arg_vid, vid);
                        }
                        false
                    }
                }
            },
            PartialEvalState::BinOp(bin_op) => {
                if let (Defer::Ready(vl), Defer::Ready(vr)) =
                    (&self.values[&bin_op.lhs], &self.values[&bin_op.rhs])
                {
                    match (vl, vr) {
                        (Value::Linear(vl), Value::Linear(vr)) => {
                            let res = match bin_op.op {
                                BinOp::Add => Some(vl.clone() + vr.clone()),
                                BinOp::Sub => Some(vl.clone() - vr.clone()),
                                BinOp::Mul => {
                                    let res = match (
                                        state.solver.eval_expr(vl),
                                        state.solver.eval_expr(vr),
                                    ) {
                                        (Some(vl), Some(vr)) => Some((vl * vr).into()),
                                        (Some(vl), None) => Some(vr.clone() * vl),
                                        (None, Some(vr)) => Some(vl.clone() * vr),
                                        (None, None) => None,
                                    };
                                    if res.is_none() {
                                        for (_, var) in
                                            vl.coeffs.clone().into_iter().chain(vr.coeffs.clone())
                                        {
                                            self.add_var_dependent(cell_id, var, vid);
                                        }
                                    }
                                    res
                                }
                                BinOp::Div => {
                                    let res =
                                        state.solver.eval_expr(vr).map(|rhs| vl.clone() / rhs);
                                    if res.is_none() {
                                        for (_, var) in vr.coeffs.clone() {
                                            self.add_var_dependent(cell_id, var, vid);
                                        }
                                    }
                                    res
                                }
                                _ => {
                                    let span = self.span(&vref.loc, bin_op.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            if let Some(res) = res {
                                self.values
                                    .insert(vid, DeferValue::Ready(Value::Linear(res)));
                                true
                            } else {
                                false
                            }
                        }
                        (Value::Int(vl), Value::Int(vr)) => {
                            let res = match bin_op.op {
                                BinOp::Add => vl + vr,
                                BinOp::Sub => vl - vr,
                                BinOp::Mul => vl * vr,
                                BinOp::Div => vl / vr,
                                BinOp::Rem => vl % vr,
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Int(res)));
                            true
                        }
                        _ => {
                            let span = self.span(&vref.loc, bin_op.expr.span);
                            self.errors.push(ExecError {
                                span: Some(span.clone()),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidType,
                            });
                            return Err(());
                        }
                    }
                } else {
                    self.add_value_dependent(bin_op.lhs, vid);
                    self.add_value_dependent(bin_op.rhs, vid);
                    false
                }
            }
            PartialEvalState::UnaryOp(unary_op) => {
                if let Defer::Ready(v) = &self.values[&unary_op.operand] {
                    match v {
                        Value::Linear(v) => {
                            let res = match unary_op.op {
                                UnaryOp::Neg => LinearExpr {
                                    coeffs: v
                                        .coeffs
                                        .iter()
                                        .map(|(coeff, var)| (-coeff, *var))
                                        .collect(),
                                    constant: -v.constant,
                                },
                                _ => {
                                    let span = self.span(&vref.loc, unary_op.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            self.values
                                .insert(vid, DeferValue::Ready(Value::Linear(res)));
                            true
                        }
                        Value::Int(v) => {
                            let res = match unary_op.op {
                                UnaryOp::Neg => -v,
                                _ => {
                                    let span = self.span(&vref.loc, unary_op.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Int(res)));
                            true
                        }
                        _ => {
                            let span = self.span(&vref.loc, unary_op.expr.span);
                            self.errors.push(ExecError {
                                span: Some(span.clone()),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidType,
                            });
                            return Err(());
                        }
                    }
                } else {
                    self.add_value_dependent(unary_op.operand, vid);
                    false
                }
            }
            PartialEvalState::If(if_) => match if_.state {
                IfExprState::Cond(cond) => {
                    if let Defer::Ready(val) = &self.values[&cond] {
                        if *val.as_ref().unwrap_bool() {
                            let scope = self.create_exec_scope_at_loc(
                                vref.loc,
                                format!("{} if", if_.expr.scope_order),
                                self.span(&vref.loc, if_.expr.then.span),
                            );
                            let then = self.visit_scope_expr_inner(
                                cell_id,
                                vref.loc.frame,
                                scope,
                                &if_.expr.then,
                            );
                            if_.state = IfExprState::Then(then);
                        } else {
                            let scope = self.create_exec_scope_at_loc(
                                vref.loc,
                                format!("{} else", if_.expr.scope_order),
                                self.span(&vref.loc, if_.expr.else_.span),
                            );
                            let else_ = self.visit_scope_expr_inner(
                                cell_id,
                                vref.loc.frame,
                                scope,
                                &if_.expr.else_,
                            );
                            if_.state = IfExprState::Else(else_);
                        }
                        self.values.insert(vid, Defer::Deferred(vref));
                        self.cell_state_mut(cell_id).deferred.insert(vid);
                        true
                    } else {
                        self.add_value_dependent(cond, vid);
                        false
                    }
                }
                IfExprState::Then(then) => {
                    if let Defer::Ready(val) = &self.values[&then] {
                        self.values.insert(vid, Defer::Ready(val.clone()));
                        true
                    } else {
                        self.add_value_dependent(then, vid);
                        false
                    }
                }
                IfExprState::Else(else_) => {
                    if let Defer::Ready(val) = &self.values[&else_] {
                        self.values.insert(vid, Defer::Ready(val.clone()));
                        true
                    } else {
                        self.add_value_dependent(else_, vid);
                        false
                    }
                }
            },
            PartialEvalState::Match(match_) => match match_.state {
                MatchExprState::Scrutinee(scrutinee) => {
                    if let Defer::Ready(val) = &self.values[&scrutinee] {
                        let variant = val.as_ref().unwrap_enum_value();
                        let arm = match_
                            .expr
                            .arms
                            .iter()
                            .find(|arm| *variant == arm.pattern.path.last().unwrap().name)
                            .unwrap();
                        let value = self.visit_expr(vref.loc, &arm.expr);
                        match_.state = MatchExprState::Value(value);
                        self.values.insert(vid, Defer::Deferred(vref));
                        self.cell_state_mut(cell_id).deferred.insert(vid);
                        true
                    } else {
                        self.add_value_dependent(scrutinee, vid);
                        false
                    }
                }
                MatchExprState::Value(value) => {
                    if let Defer::Ready(val) = &self.values[&value] {
                        self.values.insert(vid, Defer::Ready(val.clone()));
                        true
                    } else {
                        self.add_value_dependent(value, vid);
                        false
                    }
                }
            },
            PartialEvalState::Comparison(comparison_expr) => {
                if let (Defer::Ready(vl), Defer::Ready(vr)) = (
                    &self.values[&comparison_expr.state.left],
                    &self.values[&comparison_expr.state.right],
                ) {
                    match (vl, vr) {
                        (Value::Linear(vl), Value::Linear(vr)) => {
                            if let (Some(vl), Some(vr)) =
                                (state.solver.eval_expr(vl), state.solver.eval_expr(vr))
                            {
                                let res = match comparison_expr.expr.op {
                                    crate::ast::ComparisonOp::Eq => {
                                        unreachable!("cannot check equality between floats")
                                    }
                                    crate::ast::ComparisonOp::Ne => {
                                        unreachable!("cannot check inequality between floats")
                                    }
                                    crate::ast::ComparisonOp::Geq => vl >= vr,
                                    crate::ast::ComparisonOp::Gt => vl > vr,
                                    crate::ast::ComparisonOp::Leq => vl <= vr,
                                    crate::ast::ComparisonOp::Lt => vl < vr,
                                };
                                self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                                true
                            } else {
                                for (_, var) in
                                    vl.coeffs.clone().into_iter().chain(vr.coeffs.clone())
                                {
                                    self.add_var_dependent(cell_id, var, vid);
                                }
                                false
                            }
                        }
                        (Value::Int(vl), Value::Int(vr)) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => vl == vr,
                                crate::ast::ComparisonOp::Ne => vl != vr,
                                crate::ast::ComparisonOp::Geq => vl >= vr,
                                crate::ast::ComparisonOp::Gt => vl > vr,
                                crate::ast::ComparisonOp::Leq => vl <= vr,
                                crate::ast::ComparisonOp::Lt => vl < vr,
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        (Value::EnumValue(vl), Value::EnumValue(vr)) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => vl == vr,
                                crate::ast::ComparisonOp::Ne => vl != vr,
                                _ => unreachable!(),
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        (Value::Nil, Value::Nil) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => true,
                                crate::ast::ComparisonOp::Ne => false,
                                _ => unreachable!(),
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        (Value::SeqNil, Value::SeqNil) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => true,
                                crate::ast::ComparisonOp::Ne => false,
                                _ => unreachable!(),
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        (Value::Seq(x), Value::SeqNil) | (Value::SeqNil, Value::Seq(x)) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => x.is_empty(),
                                crate::ast::ComparisonOp::Ne => !x.is_empty(),
                                _ => unreachable!(),
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        _ => unreachable!(),
                    }
                } else {
                    self.add_value_dependent(comparison_expr.state.left, vid);
                    self.add_value_dependent(comparison_expr.state.right, vid);
                    false
                }
            }
            PartialEvalState::FieldAccess(field_access_expr) => {
                if let Defer::Ready(base) = &self.values[&field_access_expr.state.base] {
                    match base.as_ref() {
                        ValueRef::Rect(rect) => {
                            let val = match field_access_expr.expr.field.name.as_str() {
                                "x0" => Value::Linear(rect.x0.clone()),
                                "x1" => Value::Linear(rect.x1.clone()),
                                "y0" => Value::Linear(rect.y0.clone()),
                                "y1" => Value::Linear(rect.y1.clone()),
                                "w" => Value::Linear(rect.x1.clone() - rect.x0.clone()),
                                "h" => Value::Linear(rect.y1.clone() - rect.y0.clone()),
                                "layer" => {
                                    if let Some(layer) = rect.layer.clone() {
                                        Value::String(layer)
                                    } else {
                                        let span =
                                            self.span(&vref.loc, field_access_expr.expr.span);
                                        self.errors.push(ExecError {
                                            span: Some(span),
                                            cell: cell_id,
                                            kind: ExecErrorKind::InvalidRotation,
                                        });
                                        Value::String("".to_string())
                                    }
                                }
                                _ => {
                                    let span = self.span(&vref.loc, field_access_expr.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            self.values.insert(vid, DeferValue::Ready(val));
                            true
                        }
                        ValueRef::Polygon(polygon) => {
                            let field = field_access_expr.expr.field.name.as_str();
                            let val = match field {
                                "points" => Value::Seq(
                                    polygon
                                        .points
                                        .iter()
                                        .map(|(x, y)| Value::Point((x.clone(), y.clone())))
                                        .collect(),
                                ),
                                "layer" => Value::String(polygon.layer.clone()),
                                name if polygon_coordinate(name)
                                    .is_some_and(|coordinate| !coordinate.initial) =>
                                {
                                    let coordinate = polygon_coordinate(name).unwrap();
                                    let Some(point) = polygon.points.get(coordinate.index) else {
                                        self.errors.push(ExecError {
                                            span: Some(self.span(
                                                &vref.loc,
                                                field_access_expr.expr.field.span,
                                            )),
                                            cell: cell_id,
                                            kind: ExecErrorKind::IndexOutOfBounds,
                                        });
                                        return Err(());
                                    };
                                    Value::Linear(match coordinate.axis {
                                        PolygonAxis::X => point.0.clone(),
                                        PolygonAxis::Y => point.1.clone(),
                                    })
                                }
                                _ => {
                                    self.errors.push(ExecError {
                                        span: Some(
                                            self.span(&vref.loc, field_access_expr.expr.span),
                                        ),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            self.values.insert(vid, DeferValue::Ready(val));
                            true
                        }
                        ValueRef::Path(path) => {
                            let field = field_access_expr.expr.field.name.as_str();
                            let val = match field {
                                "points" => Value::Seq(
                                    path.points
                                        .iter()
                                        .map(|(x, y)| Value::Point((x.clone(), y.clone())))
                                        .collect(),
                                ),
                                "layer" => Value::String(path.layer.clone()),
                                "width" => Value::Linear(path.width.clone()),
                                "begin_extension" => Value::Linear(path.begin_extension.clone()),
                                "end_extension" => Value::Linear(path.end_extension.clone()),
                                name if polygon_coordinate(name)
                                    .is_some_and(|coordinate| !coordinate.initial) =>
                                {
                                    let coordinate = polygon_coordinate(name).unwrap();
                                    let Some(point) = path.points.get(coordinate.index) else {
                                        self.errors.push(ExecError {
                                            span: Some(self.span(
                                                &vref.loc,
                                                field_access_expr.expr.field.span,
                                            )),
                                            cell: cell_id,
                                            kind: ExecErrorKind::IndexOutOfBounds,
                                        });
                                        return Err(());
                                    };
                                    Value::Linear(match coordinate.axis {
                                        PolygonAxis::X => point.0.clone(),
                                        PolygonAxis::Y => point.1.clone(),
                                    })
                                }
                                _ => {
                                    self.errors.push(ExecError {
                                        span: Some(
                                            self.span(&vref.loc, field_access_expr.expr.span),
                                        ),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            self.values.insert(vid, DeferValue::Ready(val));
                            true
                        }
                        ValueRef::Point(point) => {
                            let val = match field_access_expr.expr.field.name.as_str() {
                                "x" => Value::Linear(point.0.clone()),
                                "y" => Value::Linear(point.1.clone()),
                                _ => {
                                    self.errors.push(ExecError {
                                        span: Some(
                                            self.span(&vref.loc, field_access_expr.expr.span),
                                        ),
                                        cell: cell_id,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            self.values.insert(vid, DeferValue::Ready(val));
                            true
                        }
                        ValueRef::Inst(inst) => {
                            let val = match field_access_expr.expr.field.name.as_str() {
                                "x" => Some(Value::Linear(inst.x.clone())),
                                "y" => Some(Value::Linear(inst.y.clone())),
                                field => {
                                    if let Defer::Ready(cell) = &self.values[&inst.cell] {
                                        let inst_cell_id = *cell.as_ref().unwrap_cell();
                                        // When a cell is ready, it must have been fully
                                        // solved/compiled, and therefore it will be in the
                                        // compiled cell map.
                                        let cell = &self.compiled_cells[&inst_cell_id];
                                        let field_value =
                                            if let Some(field_value) = cell.field(field) {
                                                field_value
                                            } else {
                                                self.errors.push(ExecError {
                                                    span: Some(self.span(
                                                        &vref.loc,
                                                        field_access_expr.expr.span,
                                                    )),
                                                    cell: cell_id,
                                                    // TODO: More descriptive error
                                                    kind: ExecErrorKind::EmptyBbox,
                                                });
                                                return Err(());
                                            };
                                        let compiled_cell_value_ids = &self.compiled_cell_value_ids;
                                        let obj_id = &mut self.next_id;
                                        let objects = &mut self
                                            .cell_states
                                            .get_mut(&cell_id)
                                            .unwrap()
                                            .objects;
                                        let transformed = Value::from_array(field_value.map(
                                            &mut move |v| match v {
                                                SolvedValue::Rect(rect) => {
                                                    let id = object_id(obj_id);
                                                    let rect = rect
                                                        .to_float()
                                                        .transform(inst.reflect, inst.angle);
                                                    let xrect = Rect {
                                                        id,
                                                        layer: rect.layer.clone(),
                                                        x0: LinearExpr::add(
                                                            rect.x0,
                                                            inst.x.clone(),
                                                        ),
                                                        y0: LinearExpr::add(
                                                            rect.y0,
                                                            inst.y.clone(),
                                                        ),
                                                        x1: LinearExpr::add(
                                                            rect.x1,
                                                            inst.x.clone(),
                                                        ),
                                                        y1: LinearExpr::add(
                                                            rect.y1,
                                                            inst.y.clone(),
                                                        ),
                                                        construction: rect.construction,
                                                        span: rect.span.clone(),
                                                    };
                                                    objects.insert(xrect.id, xrect.clone().into());
                                                    Value::Rect(xrect)
                                                }
                                                SolvedValue::Polygon(polygon) => {
                                                    let id = object_id(obj_id);
                                                    let mat = tmat(inst.angle, inst.reflect);
                                                    let polygon = Polygon {
                                                        id,
                                                        layer: polygon.layer.clone(),
                                                        points: polygon
                                                            .points
                                                            .iter()
                                                            .map(|(x, y)| {
                                                                let (x, y) =
                                                                    ifmatvec(mat, (x.0, y.0));
                                                                (
                                                                    LinearExpr::add(
                                                                        x,
                                                                        inst.x.clone(),
                                                                    ),
                                                                    LinearExpr::add(
                                                                        y,
                                                                        inst.y.clone(),
                                                                    ),
                                                                )
                                                            })
                                                            .collect(),
                                                        span: polygon.span.clone(),
                                                    };
                                                    objects
                                                        .insert(polygon.id, polygon.clone().into());
                                                    Value::Polygon(polygon)
                                                }
                                                SolvedValue::Path(path) => {
                                                    let id = object_id(obj_id);
                                                    let mat = tmat(inst.angle, inst.reflect);
                                                    let path = Path {
                                                        id,
                                                        layer: path.layer.clone(),
                                                        width: LinearExpr::from(path.width.0),
                                                        points: path
                                                            .points
                                                            .iter()
                                                            .map(|(x, y)| {
                                                                let (x, y) =
                                                                    ifmatvec(mat, (x.0, y.0));
                                                                (
                                                                    LinearExpr::add(
                                                                        x,
                                                                        inst.x.clone(),
                                                                    ),
                                                                    LinearExpr::add(
                                                                        y,
                                                                        inst.y.clone(),
                                                                    ),
                                                                )
                                                            })
                                                            .collect(),
                                                        begin_extension: LinearExpr::from(
                                                            path.begin_extension.0,
                                                        ),
                                                        end_extension: LinearExpr::from(
                                                            path.end_extension.0,
                                                        ),
                                                        span: path.span.clone(),
                                                    };
                                                    objects.insert(path.id, path.clone().into());
                                                    Value::Path(path)
                                                }
                                                SolvedValue::Instance(cinst) => {
                                                    let (angle, reflect, cx, cy) = cascade(
                                                        inst.angle,
                                                        inst.reflect,
                                                        cinst.angle,
                                                        cinst.reflect,
                                                        cinst.x,
                                                        cinst.y,
                                                    );
                                                    let id = object_id(obj_id);
                                                    let oinst = Instance {
                                                        id,
                                                        cell: compiled_cell_value_ids[&cinst.cell],
                                                        x: LinearExpr::add(inst.x.clone(), cx),
                                                        y: LinearExpr::add(inst.y.clone(), cy),
                                                        angle,
                                                        reflect,
                                                        construction: cinst.construction,
                                                        span: cinst.span.clone(),
                                                    };
                                                    objects.insert(oinst.id, oinst.clone().into());
                                                    Value::Inst(oinst)
                                                }
                                                _ => unreachable!(),
                                            },
                                        ));
                                        Some(transformed)
                                    } else {
                                        None
                                    }
                                }
                            };
                            if let Some(val) = val {
                                self.values.insert(vid, DeferValue::Ready(val));
                                true
                            } else {
                                self.add_value_dependent(inst.cell, vid);
                                false
                            }
                        }
                        _ => {
                            let span = self.span(&vref.loc, field_access_expr.expr.span);
                            self.errors.push(ExecError {
                                span: Some(span),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidType,
                            });
                            return Err(());
                        }
                    }
                } else {
                    self.add_value_dependent(field_access_expr.state.base, vid);
                    false
                }
            }
            PartialEvalState::IndexFieldAccess(field_access_expr) => {
                if let Defer::Ready(base) = &self.values[&field_access_expr.state.base] {
                    match base.as_ref() {
                        ValueRef::Tuple(t) => {
                            if let Some(v) = usize::try_from(field_access_expr.expr.field.value)
                                .ok()
                                .and_then(|i| t.get(i))
                            {
                                self.values.insert(vid, DeferValue::Ready(v.clone()));
                                true
                            } else {
                                let span = self.span(&vref.loc, field_access_expr.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span),
                                    cell: cell_id,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        }
                        _ => {
                            let span = self.span(&vref.loc, field_access_expr.expr.span);
                            self.errors.push(ExecError {
                                span: Some(span),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidType,
                            });
                            return Err(());
                        }
                    }
                } else {
                    self.add_value_dependent(field_access_expr.state.base, vid);
                    false
                }
            }
            PartialEvalState::Index(index_expr) => {
                if let Defer::Ready(base) = &self.values[&index_expr.state.base]
                    && let Defer::Ready(index) = &self.values[&index_expr.state.index]
                {
                    if let ValueRef::Seq(s) = base.as_ref() {
                        if let ValueRef::Int(i) = index.as_ref() {
                            if let Some(v) = usize::try_from(*i).ok().and_then(|i| s.get(i)) {
                                self.values.insert(vid, DeferValue::Ready(v.clone()));
                                true
                            } else {
                                let span = self.span(&vref.loc, index_expr.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span),
                                    cell: cell_id,
                                    kind: ExecErrorKind::IndexOutOfBounds,
                                });
                                return Err(());
                            }
                        } else {
                            let span = self.span(&vref.loc, index_expr.expr.index.span());
                            self.errors.push(ExecError {
                                span: Some(span),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidType,
                            });
                            return Err(());
                        }
                    } else {
                        let span = self.span(&vref.loc, index_expr.expr.base.span());
                        self.errors.push(ExecError {
                            span: Some(span),
                            cell: cell_id,
                            kind: ExecErrorKind::InvalidType,
                        });
                        return Err(());
                    }
                } else {
                    self.add_value_dependent(index_expr.state.base, vid);
                    self.add_value_dependent(index_expr.state.index, vid);
                    false
                }
            }
            PartialEvalState::Constraint(c) => {
                if let (Defer::Ready(vl), Defer::Ready(vr)) =
                    (&self.values[&c.lhs], &self.values[&c.rhs])
                {
                    let lhs = vl.as_ref().unwrap_linear();
                    let rhs = vr.as_ref().unwrap_linear();
                    let expr = lhs.clone() - rhs.clone();
                    if c.fallback {
                        state.fallback_constraints.push(FallbackConstraint {
                            priority: c.priority,
                            constraint: expr,
                            span: c.span.clone(),
                            initial_condition: c.initial_condition,
                        });
                    } else {
                        let constraint = state.solver.constrain_eq0(expr);
                        state.constraint_span_map.insert(constraint, c.span.clone());
                    }
                    self.values.insert(vid, DeferValue::Ready(Value::Nil));
                    true
                } else {
                    self.add_value_dependent(c.lhs, vid);
                    self.add_value_dependent(c.rhs, vid);
                    false
                }
            }
            PartialEvalState::Cast(c) => {
                if let Defer::Ready(val) = &self.values[&c.state.value] {
                    let value = match (val, &c.state.ty) {
                        (Value::Int(x), Ty::Float) => {
                            Some(Value::Linear(LinearExpr::from(*x as f64)))
                        }
                        (x @ Value::Int(_), Ty::Int) => Some(x.clone()),
                        (Value::Linear(expr), Ty::Int) => {
                            let res = state
                                .solver
                                .eval_expr(expr)
                                .map(|val| Value::Int(val as i64));
                            if res.is_none() {
                                for (_, var) in expr.coeffs.clone() {
                                    self.add_var_dependent(cell_id, var, vid);
                                }
                            }
                            res
                        }
                        (expr @ Value::Linear(_), Ty::Float) => Some(expr.clone()),
                        _ => {
                            let span = self.span(&vref.loc, c.expr.span);
                            self.errors.push(ExecError {
                                span: Some(span),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidCast,
                            });
                            return Err(());
                        }
                    };
                    if let Some(value) = value {
                        self.values.insert(vid, DeferValue::Ready(value));
                        true
                    } else {
                        false
                    }
                } else {
                    self.add_value_dependent(c.state.value, vid);
                    false
                }
            }
            PartialEvalState::Tuple(tuple) => {
                let items = tuple
                    .items
                    .iter()
                    .map(|i| self.values[i].get_ready().cloned())
                    .collect::<Option<Vec<_>>>();
                if let Some(items) = items {
                    self.values
                        .insert(vid, DeferValue::Ready(Value::Tuple(items)));
                    true
                } else {
                    let dep = tuple
                        .items
                        .iter()
                        .find(|&i| !self.values[i].is_ready())
                        .unwrap();
                    self.add_value_dependent(*dep, vid);
                    false
                }
            }
            PartialEvalState::ForLoop(f) => {
                if let Defer::Ready(val) = &self.values[&f.seq] {
                    let seq = match val.as_ref() {
                        // `s.clone()` is now an O(1) refcount bump (was an O(n) deep copy).
                        ValueRef::Seq(s) => s.clone(),
                        ValueRef::SeqNil => Seq::new(),
                        _ => {
                            let span = self.span(&vref.loc, f.for_loop.seq.span());
                            self.errors.push(ExecError {
                                span: Some(span),
                                cell: cell_id,
                                kind: ExecErrorKind::InvalidType,
                            });
                            return Err(());
                        }
                    };
                    for (i, elem) in seq.iter().enumerate() {
                        if self.cancellation_requested() {
                            return Err(());
                        }
                        let mut frame = Frame {
                            bindings: Default::default(),
                            parent: Some(vref.loc.frame),
                        };

                        let elem_vid = self.value_id();
                        self.values
                            .insert(elem_vid, DeferValue::Ready(elem.clone()));
                        frame.bindings.insert(f.for_loop.metadata, elem_vid);
                        let scope = self.create_exec_scope_at_loc(
                            vref.loc,
                            format!(
                                "{} for {}[{i}]",
                                f.for_loop.scope_order, f.for_loop.var.name
                            ),
                            self.span(&vref.loc, f.for_loop.body.span),
                        );
                        let fid = self.frame_id();
                        self.frames.insert(fid, frame);
                        self.visit_scope_expr_inner(vref.loc.cell, fid, scope, &f.for_loop.body);
                    }
                    self.values.insert(vid, Defer::Ready(Value::Nil));
                    true
                } else {
                    self.add_value_dependent(f.seq, vid);
                    false
                }
            }
        };

        if self.values[&vid].is_ready()
            && let Some(deps) = self.value_dependents.get(&vid)
        {
            for dep_vid in deps.clone() {
                self.cell_state_mut(cell_id).deferred.insert(dep_vid);
            }
        }
        Ok(progress)
    }

    /// The bounding box of `cell`'s geometry, in `cell`'s own coordinate frame.
    pub fn bbox(&self, cell: CellId) -> Option<Rect<f64>> {
        let mut bbox = None;
        let cell = &self.compiled_cells[&cell];
        for (_, o) in cell.objects.iter() {
            match o {
                SolvedValue::Rect(r) => bbox = bbox_union(bbox, Some(r.to_float())),
                SolvedValue::Polygon(p) => bbox = bbox_union(bbox, p.bbox()),
                SolvedValue::Path(p) => bbox = bbox_union(bbox, p.bbox()),
                SolvedValue::Instance(i) => {
                    let cell_bbox = self
                        .bbox(i.cell)
                        .map(|r| r.transform(i.reflect, i.angle).translate(i.x, i.y));
                    bbox = bbox_union(bbox, cell_bbox);
                }
                _ => (),
            }
        }
        bbox
    }
}

/// Persistent immutable sequence backing `Value::Seq`.
///
/// Backed by an RRB-tree (`im::Vector`): O(1) clone (structural sharing) and
/// O(log n) `push_front`/`get`/`pop_front`. This keeps `cons` (used to build
/// `range`) at O(log n) instead of the O(n) clone+prepend a `Vec` requires, so
/// building `range(n)` is O(n log n) rather than O(n^2), while random indexing
/// (`arr[i]`) stays O(log n). `im::Vector` is `Arc`-backed, so `Seq` is `Send`
/// exactly when `Value` is — no regression for the (tokio) language server.
type Seq = im::Vector<Value>;

#[enumify]
#[derive(Debug, Clone)]
pub enum Value {
    EnumValue(String),
    String(String),
    Linear(LinearExpr),
    Int(i64),
    Rect(Rect<LinearExpr>),
    Polygon(Polygon<LinearExpr>),
    Path(Path<LinearExpr>),
    Point((LinearExpr, LinearExpr)),
    Bool(bool),
    Fn(FnDecl<Substr, VarIdTyMetadata>),
    /// A cell generator.
    ///
    /// Example:
    /// ```argon
    /// cell mycell() {
    ///   // ...
    /// }
    /// ```
    ///
    /// `mycell` is a value of type `CellFn`.
    CellFn(CellDecl<Substr, VarIdTyMetadata>),
    /// A particular parameterization of a cell.
    ///
    /// Example:
    /// ```argon
    /// cell mycell() {
    ///   // ...
    /// }
    ///
    /// let val = mycell();
    /// ```
    ///
    /// `val` is a value of type `Cell`.
    Cell(CellId),
    /// An instantiation of a cell value.
    ///
    /// Example:
    /// ```argon
    /// cell mycell() {
    ///   // ...
    /// }
    ///
    /// let mycell_inst = inst(mycell(), x=0, y=0);
    /// ```
    ///
    /// `mycell_inst` is a value of type `Inst`.
    Inst(Instance),
    Seq(Seq),
    Tuple(Vec<Value>),
    SeqNil,
    Nil,
}

impl Value {
    pub fn to_obj(&self) -> Option<Object> {
        match self {
            Self::Rect(r) => Some(Object::Rect(r.clone())),
            Self::Polygon(p) => Some(Object::Polygon(p.clone())),
            Self::Path(p) => Some(Object::Path(p.clone())),
            Self::Inst(i) => Some(Object::Inst(i.clone())),
            _ => None,
        }
    }

    pub fn from_arg(arg: &CellArg) -> Self {
        match arg {
            CellArg::Int(i) => Value::Int(*i),
            CellArg::Bool(b) => Value::Bool(*b),
            CellArg::Float(f) => Value::Linear(LinearExpr::from(*f)),
            CellArg::String(s) => Value::String(s.clone()),
            CellArg::Enum(v) => Value::EnumValue(v.clone()),
            CellArg::Seq(v) => Value::Seq(v.iter().map(Self::from_arg).collect()),
        }
    }

    /// Name of this value's kind, for diagnostics.
    fn kind_name(&self) -> &'static str {
        match self {
            Self::EnumValue(_) => "enum variant",
            Self::String(_) => "String",
            Self::Linear(_) => "Float",
            Self::Int(_) => "Int",
            Self::Rect(_) => "Rect",
            Self::Polygon(_) => "Polygon",
            Self::Path(_) => "Path",
            Self::Point(_) => "Point",
            Self::Bool(_) => "Bool",
            Self::Fn(_) => "function",
            Self::CellFn(_) => "cell generator",
            Self::Cell(_) => "cell",
            Self::Inst(_) => "instance",
            Self::Seq(_) | Self::SeqNil => "sequence",
            Self::Tuple(_) => "tuple",
            Self::Nil => "nil",
        }
    }

    fn obj_ids(&self) -> Option<Arrayed<ObjectId>> {
        match self {
            Value::Rect(r) => Some(Arrayed::Elem(r.id)),
            Value::Polygon(p) => Some(Arrayed::Elem(p.id)),
            Value::Path(p) => Some(Arrayed::Elem(p.id)),
            Value::Inst(i) => Some(Arrayed::Elem(i.id)),
            Value::Seq(s) => Some(Arrayed::Array(
                s.iter().map(|v| v.obj_ids()).collect::<Option<Vec<_>>>()?,
            )),
            _ => None,
        }
    }

    fn from_array(arr: Arrayed<Value>) -> Self {
        match arr {
            Arrayed::Elem(v) => v,
            Arrayed::Array(s) => Self::Seq(s.into_iter().map(Value::from_array).collect()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: ObjectId,
    pub x: LinearExpr,
    pub y: LinearExpr,
    pub cell: ValueId,
    pub reflect: bool,
    pub angle: Rotation,
    pub construction: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolvedInstance {
    pub id: ObjectId,
    pub x: f64,
    pub y: f64,
    /// Solver expressions retained for solution-space movement in the GUI.
    pub x_expr: LinearExpr,
    pub y_expr: LinearExpr,
    pub angle: Rotation,
    pub reflect: bool,
    pub construction: bool,
    pub cell: CellId,
    pub span: Span,
    /// The value ID of the cell being instantiated.
    ///
    /// For compiler internal use only.
    cell_vid: ValueId,
}

#[enumify]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SolvedValue {
    Rect(Rect<(f64, LinearExpr)>),
    Polygon(Polygon<(f64, LinearExpr)>),
    Path(Path<(f64, LinearExpr)>),
    Text(Text<f64>),
    Dimension(Dimension<(f64, LinearExpr)>),
    Instance(SolvedInstance),
}

#[enumify]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Object {
    Rect(Rect<LinearExpr>),
    Polygon(Polygon<LinearExpr>),
    Path(Path<LinearExpr>),
    Text(Text<LinearExpr>),
    Dimension(Dimension<LinearExpr>),
    Inst(Instance),
}

impl From<Rect<LinearExpr>> for Object {
    fn from(value: Rect<LinearExpr>) -> Self {
        Self::Rect(value)
    }
}

impl From<Polygon<LinearExpr>> for Object {
    fn from(value: Polygon<LinearExpr>) -> Self {
        Self::Polygon(value)
    }
}

impl From<Path<LinearExpr>> for Object {
    fn from(value: Path<LinearExpr>) -> Self {
        Self::Path(value)
    }
}

impl From<Text<LinearExpr>> for Object {
    fn from(value: Text<LinearExpr>) -> Self {
        Self::Text(value)
    }
}

impl From<Dimension<LinearExpr>> for Object {
    fn from(value: Dimension<LinearExpr>) -> Self {
        Self::Dimension(value)
    }
}

impl From<Instance> for Object {
    fn from(value: Instance) -> Self {
        Self::Inst(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledScope {
    pub static_parent: Option<(ScopeId, SeqNum)>,
    pub bindings: IndexMap<SeqNum, (String, Arrayed<ObjectId>)>,
    /// Dynamic children.
    pub children: IndexSet<ScopeId>,
    pub name: String,
    pub span: Span,
    /// Objects emitted in this scope.
    pub emit: Vec<(ObjectId, CompiledEmit)>,
}

/// A fallback (initial-condition) constraint that was actually applied while
/// solving a cell. Used by the GUI to persist solution-space-exploration drags:
/// after a drag, the value text at `span` is rewritten so the new layout sticks
/// across recompilation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsedFallback {
    /// The applied constraint, of the form `expr - value` (so the pinned value
    /// is `-constraint.constant` when `expr` has no constant term).
    pub constraint: LinearExpr,
    /// Source span of the initial-condition value expression (e.g. the `100.`
    /// in `x1i=100.`), so the GUI can rewrite just that value.
    pub span: Span,
    /// Geometry coordinate initialized by this fallback. Rectangle metadata
    /// lets the GUI keep `x0 <= x1` and `y0 <= y1` when edges cross.
    pub initial_condition: Option<RectInitialCondition>,
}

/// The geometry coordinate associated with a user-written initial condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RectInitialCondition {
    X0(ObjectId),
    X1(ObjectId),
    Y0(ObjectId),
    Y1(ObjectId),
    PolygonX(ObjectId, usize),
    PolygonY(ObjectId, usize),
    PathX(ObjectId, usize),
    PathY(ObjectId, usize),
    PathWidth(ObjectId),
    PathBeginExtension(ObjectId),
    PathEndExtension(ObjectId),
    InstanceX(ObjectId),
    InstanceY(ObjectId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SseBasis {
    /// An orthonormal constraint row-space basis. SSE obtains allowed motion
    /// by subtracting the projection onto these vectors.
    Rowspace(Vec<Vec<(f64, Var)>>),
    /// An orthonormal constraint null-space basis. SSE projects allowed motion
    /// directly onto these vectors.
    Nullspace(Vec<Vec<(f64, Var)>>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledCell {
    pub scopes: IndexMap<ScopeId, CompiledScope>,
    pub root: ScopeId,
    pub fields: IndexMap<String, Arrayed<ObjectId>>,
    pub sse_basis: SseBasis,
    pub objects: IndexMap<ObjectId, SolvedValue>,
    pub fallback_constraints_used: Vec<UsedFallback>,
    pub unsolved_vars: IndexSet<Var>,
    pub inconsistent_constraints: IndexSet<ConstraintId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[enumify]
pub enum Arrayed<T> {
    Elem(T),
    Array(Vec<Arrayed<T>>),
}

impl<T> Arrayed<T> {
    pub fn map<U, F>(&self, f: &mut F) -> Arrayed<U>
    where
        F: FnMut(&T) -> U,
    {
        match self {
            Self::Elem(x) => Arrayed::Elem(f(x)),
            Self::Array(x) => Arrayed::Array(x.iter().map(|x| x.map(f)).collect()),
        }
    }

    pub fn for_each<F>(&self, f: &mut F)
    where
        F: FnMut(&T),
    {
        match self {
            Self::Elem(x) => f(x),
            Self::Array(x) => x.iter().for_each(|x| x.for_each(f)),
        }
    }
}

impl CompiledCell {
    pub fn field(&self, name: &str) -> Option<Arrayed<&SolvedValue>> {
        self.fields
            .get(name)
            .map(|o| o.map(&mut |id| &self.objects[id]))
    }
}

pub fn bbox_union(b1: Option<Rect<f64>>, b2: Option<Rect<f64>>) -> Option<Rect<f64>> {
    match (b1, b2) {
        (Some(r1), Some(r2)) => Some(Rect {
            layer: None,
            x0: r1.x0.min(r2.x0),
            y0: r1.y0.min(r2.y0),
            x1: r1.x1.max(r2.x1),
            y1: r1.y1.max(r2.y1),
            id: r1.id,
            construction: true,
            span: None,
        }),
        (Some(r), None) | (None, Some(r)) => Some(r),
        (None, None) => None,
    }
}

pub fn bbox_text_union(b: Option<Rect<f64>>, t: &Text<f64>) -> Option<Rect<f64>> {
    match b {
        Some(r) => Some(Rect {
            layer: None,
            x0: r.x0.min(t.x),
            y0: r.y0.min(t.y),
            x1: r.x1.max(t.x),
            y1: r.y1.max(t.y),
            id: r.id,
            construction: true,
            span: None,
        }),
        None => Some(Rect {
            layer: None,
            x0: t.x,
            y0: t.y,
            x1: t.x,
            y1: t.y,
            id: t.id,
            construction: true,
            span: None,
        }),
    }
}

pub fn bbox_dim_union(
    bbox: Option<Rect<f64>>,
    dim: &Dimension<(f64, LinearExpr)>,
) -> Option<Rect<f64>> {
    let perp_max = dim.coord.0.max(dim.pstop.0).max(dim.nstop.0);
    let perp_min = dim.coord.0.min(dim.pstop.0).min(dim.nstop.0);
    let par_max = dim.n.0.max(dim.p.0);
    let par_min = dim.n.0.min(dim.p.0);
    let (xmin, xmax, ymin, ymax) = if dim.horiz {
        (par_min, par_max, perp_min, perp_max)
    } else {
        (perp_min, perp_max, par_min, par_max)
    };
    match bbox {
        Some(r) => Some(Rect {
            layer: None,
            x0: r.x0.min(xmin),
            y0: r.y0.min(ymin),
            x1: r.x1.max(xmax),
            y1: r.y1.max(ymax),
            id: r.id,
            construction: true,
            span: None,
        }),
        None => Some(Rect {
            layer: None,
            x0: xmin,
            y0: ymin,
            x1: xmax,
            y1: ymax,
            id: ObjectId(0), // FIXME: should not need to allocate an object ID
            construction: true,
            span: None,
        }),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledData {
    /// Immutable compiled cells are structurally shared by incremental cache
    /// entries and successive public snapshots. Serde's `rc` support keeps the
    /// binary artifact representation transparent while in-process clones are
    /// O(number of cells), not O(number of geometry objects).
    pub cells: IndexMap<CellId, Arc<CompiledCell>>,
    pub top: CellId,
    pub tech: crate::tech::Technology,
}

#[enumify(generics_only)]
#[derive(Clone, Debug)]
enum Defer<R, D> {
    Ready(R),
    Deferred(D),
}

type DeferValue<T> = Defer<Value, PartialEval<T>>;

#[derive(Debug, Clone)]
struct PartialEval<T: AstMetadata> {
    state: PartialEvalState<T>,
    loc: DynLoc,
}

#[derive(Debug, Clone)]
enum PartialEvalState<T: AstMetadata> {
    If(Box<PartialIfExpr<T>>),
    Match(Box<PartialMatchExpr<T>>),
    Comparison(Box<PartialComparisonExpr<T>>),
    BinOp(PartialBinOp<T>),
    UnaryOp(PartialUnaryOp<T>),
    Call(Box<PartialCallExpr<T>>),
    FieldAccess(Box<PartialFieldAccessExpr<T>>),
    IndexFieldAccess(Box<PartialIndexFieldAccessExpr<T>>),
    Index(Box<PartialIndexExpr<T>>),
    Constraint(PartialConstraint),
    Cast(Box<PartialCastExpr<T>>),
    Tuple(PartialTupleExpr),
    ForLoop(Box<PartialForLoop<T>>),
}

#[derive(Debug, Clone)]
struct PartialCastState {
    value: ValueId,
    ty: Ty,
}

#[derive(Debug, Clone)]
struct PartialConstraint {
    lhs: ValueId,
    rhs: ValueId,
    fallback: bool,
    priority: i32,
    span: Span,
    initial_condition: Option<RectInitialCondition>,
}

#[derive(Debug, Clone)]
struct PartialBinOp<T: AstMetadata> {
    lhs: ValueId,
    rhs: ValueId,
    op: BinOp,
    expr: Box<BinOpExpr<Substr, T>>,
}

#[derive(Debug, Clone)]
struct PartialUnaryOp<T: AstMetadata> {
    operand: ValueId,
    op: UnaryOp,
    expr: Box<UnaryOpExpr<Substr, T>>,
}

#[derive(Debug, Clone)]
struct PartialIfExpr<T: AstMetadata> {
    expr: IfExpr<Substr, T>,
    state: IfExprState,
}

#[derive(Debug, Clone)]
struct PartialMatchExpr<T: AstMetadata> {
    expr: MatchExpr<Substr, T>,
    state: MatchExprState,
}

#[derive(Debug, Clone)]
pub enum IfExprState {
    Cond(ValueId),
    Then(ValueId),
    Else(ValueId),
}

#[derive(Debug, Clone)]
pub enum MatchExprState {
    Scrutinee(ValueId),
    Value(ValueId),
}

#[derive(Debug, Clone)]
struct PartialCallExpr<T: AstMetadata> {
    expr: CallExpr<Substr, T>,
    state: CallExprState,
}

#[derive(Debug, Clone)]
pub struct CallExprState {
    posargs: Vec<ValueId>,
    kwargs: Vec<ValueId>,
}

#[derive(Debug, Clone)]
struct PartialComparisonExpr<T: AstMetadata> {
    expr: ComparisonExpr<Substr, T>,
    state: ComparisonExprState,
}

#[derive(Debug, Clone)]
pub struct ComparisonExprState {
    left: ValueId,
    right: ValueId,
}

#[derive(Debug, Clone)]
struct PartialFieldAccessExpr<T: AstMetadata> {
    expr: FieldAccessExpr<Substr, T>,
    state: FieldAccessExprState,
}

#[derive(Debug, Clone)]
struct PartialIndexFieldAccessExpr<T: AstMetadata> {
    expr: IndexFieldAccessExpr<Substr, T>,
    state: IndexFieldAccessExprState,
}

#[derive(Debug, Clone)]
struct PartialIndexExpr<T: AstMetadata> {
    expr: IndexExpr<Substr, T>,
    state: IndexExprState,
}

#[derive(Debug, Clone)]
struct PartialCastExpr<T: AstMetadata> {
    expr: CastExpr<Substr, T>,
    state: PartialCastState,
}

#[derive(Debug, Clone)]
struct PartialTupleExpr {
    items: Vec<ValueId>,
}

#[derive(Debug, Clone)]
struct PartialForLoop<T: AstMetadata> {
    for_loop: ForLoop<Substr, T>,
    seq: ValueId,
}

#[derive(Debug, Clone)]
pub struct FieldAccessExprState {
    base: ValueId,
}

#[derive(Debug, Clone)]
pub struct IndexFieldAccessExprState {
    base: ValueId,
}

#[derive(Debug, Clone)]
pub struct IndexExprState {
    base: ValueId,
    index: ValueId,
}

pub fn ifmatvec(mat: TransformationMatrix, pt: (f64, f64)) -> (f64, f64) {
    (
        mat[0][0] as f64 * pt.0 + mat[0][1] as f64 * pt.1,
        mat[1][0] as f64 * pt.0 + mat[1][1] as f64 * pt.1,
    )
}

fn tmat(rot: Rotation, refv: bool) -> TransformationMatrix {
    let mut mat = TransformationMatrix::identity();
    if refv {
        mat = mat.reflect_vert()
    }
    mat = mat.rotate(rot);
    mat
}

fn imat(mat: TransformationMatrix) -> (Rotation, bool) {
    let refv = mat[1][0] == mat[0][1] && mat[0][0] == -mat[1][1];
    let rot = match (mat[0][0], mat[1][0]) {
        (1, 0) => Rotation::R0,
        (0, 1) => Rotation::R90,
        (-1, 0) => Rotation::R180,
        (0, -1) => Rotation::R270,
        // `mat` is formed exclusively from Manhattan rotation matrices, so
        // this is an internal fallback rather than a user-visible crash path.
        _ => Rotation::R0,
    };
    (rot, refv)
}

impl<T> Rect<(f64, T)> {
    pub fn to_float(&self) -> Rect<f64> {
        Rect {
            id: self.id,
            layer: self.layer.clone(),
            x0: self.x0.0,
            y0: self.y0.0,
            x1: self.x1.0,
            y1: self.y1.0,
            construction: self.construction,
            span: self.span.clone(),
        }
    }
}

impl<T> Polygon<(f64, T)> {
    pub fn bbox(&self) -> Option<Rect<f64>> {
        let mut points = self.points.iter();
        let ((x, _), (y, _)) = points.next()?;
        let (mut x0, mut y0, mut x1, mut y1) = (*x, *y, *x, *y);
        for ((x, _), (y, _)) in points {
            x0 = x0.min(*x);
            y0 = y0.min(*y);
            x1 = x1.max(*x);
            y1 = y1.max(*y);
        }
        Some(Rect {
            id: self.id,
            layer: None,
            x0,
            y0,
            x1,
            y1,
            construction: true,
            span: self.span.clone(),
        })
    }
}

const PATH_OUTLINE_EPSILON: f64 = 1e-12;

fn path_real_points(points: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut result = Vec::<(f64, f64)>::with_capacity(points.len());
    for point in points {
        if result.last() == Some(&point) {
            continue;
        }
        while result.len() >= 2 {
            let a = result[result.len() - 2];
            let b = result[result.len() - 1];
            let ab = (b.0 - a.0, b.1 - a.1);
            let bc = (point.0 - b.0, point.1 - b.1);
            if path_cross(ab, bc).abs() <= PATH_OUTLINE_EPSILON && path_dot(ab, bc) >= 0. {
                result.pop();
            } else {
                break;
            }
        }
        result.push(point);
    }
    result
}

fn path_shifted_side(
    points: &[(f64, f64)],
    half_width: f64,
    begin_extension: f64,
    end_extension: f64,
) -> Option<Vec<(f64, f64)>> {
    let directions = points
        .windows(2)
        .map(|points| path_unit(points[0], points[1]))
        .collect::<Option<Vec<_>>>()?;
    let mut shifted = Vec::with_capacity(points.len() * 2);

    let first_direction = directions[0];
    let first_normal = path_scale(path_normal(first_direction), half_width);
    shifted.push(path_add(
        points[0],
        path_add(path_scale(first_direction, -begin_extension), first_normal),
    ));

    for index in 1..points.len() - 1 {
        let point = points[index];
        let previous_direction = directions[index - 1];
        let next_direction = directions[index];
        let previous_normal = path_scale(path_normal(previous_direction), half_width);
        let next_normal = path_scale(path_normal(next_direction), half_width);
        let turn = path_cross(previous_direction, next_direction);

        if turn.abs() > PATH_OUTLINE_EPSILON {
            let normal_delta = (
                next_normal.0 - previous_normal.0,
                next_normal.1 - previous_normal.1,
            );
            let previous_length = path_distance(points[index - 1], point);
            let next_length = path_distance(points[index + 1], point);
            let previous_offset = path_cross(normal_delta, next_direction) / turn;
            let next_offset = path_cross(
                (
                    previous_normal.0 - next_normal.0,
                    previous_normal.1 - next_normal.1,
                ),
                previous_direction,
            ) / turn;
            let previous_min = -previous_length - half_width;
            let next_min = -next_length - half_width;

            if previous_offset < previous_min - PATH_OUTLINE_EPSILON
                || next_offset < next_min - PATH_OUTLINE_EPSILON
            {
                shifted.push(path_add(point, previous_normal));
                shifted.push(point);
                shifted.push(path_add(point, next_normal));
            } else if previous_offset <= half_width + PATH_OUTLINE_EPSILON
                && next_offset <= half_width + PATH_OUTLINE_EPSILON
            {
                shifted.push(path_add(
                    point,
                    path_add(
                        previous_normal,
                        path_scale(previous_direction, previous_offset),
                    ),
                ));
            } else {
                shifted.push(path_add(
                    point,
                    path_add(
                        previous_normal,
                        path_scale(previous_direction, previous_offset.min(half_width)),
                    ),
                ));
                shifted.push(path_add(
                    point,
                    path_add(
                        next_normal,
                        path_scale(next_direction, -next_offset.min(half_width)),
                    ),
                ));
            }
        } else if path_dot(previous_direction, next_direction) < -PATH_OUTLINE_EPSILON {
            shifted.push(path_add(
                point,
                path_add(previous_normal, path_scale(previous_direction, half_width)),
            ));
            shifted.push(path_add(
                point,
                path_add(next_normal, path_scale(next_direction, -half_width)),
            ));
        }
    }

    let last_direction = directions[directions.len() - 1];
    let last_normal = path_scale(path_normal(last_direction), half_width);
    shifted.push(path_add(
        points[points.len() - 1],
        path_add(path_scale(last_direction, end_extension), last_normal),
    ));
    Some(shifted)
}

fn path_unit(from: (f64, f64), to: (f64, f64)) -> Option<(f64, f64)> {
    let vector = (to.0 - from.0, to.1 - from.1);
    let length = vector.0.hypot(vector.1);
    (length.is_finite() && length > 0.).then_some((vector.0 / length, vector.1 / length))
}

fn path_add(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    (a.0 + b.0, a.1 + b.1)
}

fn path_scale(vector: (f64, f64), scale: f64) -> (f64, f64) {
    (vector.0 * scale, vector.1 * scale)
}

fn path_normal(vector: (f64, f64)) -> (f64, f64) {
    (-vector.1, vector.0)
}

fn path_cross(a: (f64, f64), b: (f64, f64)) -> f64 {
    a.0 * b.1 - a.1 * b.0
}

fn path_dot(a: (f64, f64), b: (f64, f64)) -> f64 {
    a.0 * b.0 + a.1 * b.1
}

fn path_distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

impl<T> Path<(f64, T)> {
    /// Returns the non-rounded outline of this path, using bounded mitered
    /// joins and the path's begin/end extensions.
    pub fn outline(&self) -> Option<Vec<(f64, f64)>> {
        let points = self
            .points
            .iter()
            .map(|(x, y)| (x.0, y.0))
            .collect::<Vec<_>>();
        path_outline(
            &points,
            self.width.0,
            self.begin_extension.0,
            self.end_extension.0,
        )
    }

    pub fn bbox(&self) -> Option<Rect<f64>> {
        let outline = self.outline()?;
        let mut points = outline.into_iter();
        let (x, y) = points.next()?;
        let (mut x0, mut y0, mut x1, mut y1) = (x, y, x, y);
        for (x, y) in points {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        Some(Rect {
            id: self.id,
            layer: None,
            x0,
            y0,
            x1,
            y1,
            construction: true,
            span: self.span.clone(),
        })
    }
}

/// Returns the non-rounded outline for a path centerline and extensions.
pub fn path_outline(
    points: &[(f64, f64)],
    width: f64,
    begin_extension: f64,
    end_extension: f64,
) -> Option<Vec<(f64, f64)>> {
    let points = path_real_points(points.to_vec());
    if points.len() < 2 {
        return None;
    }
    let half_width = width.abs() / 2.;
    if !half_width.is_finite() || !begin_extension.is_finite() || !end_extension.is_finite() {
        return None;
    }

    let mut outline = path_shifted_side(&points, half_width, begin_extension, end_extension)?;
    let reversed = points.iter().rev().copied().collect::<Vec<_>>();
    outline.extend(path_shifted_side(
        &reversed,
        half_width,
        end_extension,
        begin_extension,
    )?);
    outline
        .iter()
        .all(|(x, y)| x.is_finite() && y.is_finite())
        .then_some(outline)
}

impl Rect<f64> {
    fn transform(&self, reflect_vert: bool, angle: Rotation) -> Self {
        let mat = tmat(angle, reflect_vert);
        let p0p = ifmatvec(mat, (self.x0, self.y0));
        let p1p = ifmatvec(mat, (self.x1, self.y1));
        Self {
            id: self.id,
            layer: self.layer.clone(),
            x0: p0p.0.min(p1p.0),
            y0: p0p.1.min(p1p.1),
            x1: p0p.0.max(p1p.0),
            y1: p0p.1.max(p1p.1),
            construction: self.construction,
            span: None,
        }
    }

    /// Translates the rect by `(dx, dy)`.
    fn translate(&self, dx: f64, dy: f64) -> Self {
        Self {
            x0: self.x0 + dx,
            y0: self.y0 + dy,
            x1: self.x1 + dx,
            y1: self.y1 + dy,
            ..self.clone()
        }
    }
}

fn cascade(
    rot: Rotation,
    refv: bool,
    crot: Rotation,
    crefv: bool,
    cx: f64,
    cy: f64,
) -> (Rotation, bool, f64, f64) {
    let mat = tmat(rot, refv);
    let cmat = tmat(crot, crefv);
    let (x, y) = ifmatvec(mat, (cx, cy));
    let (rot, refv) = imat(mat * cmat);
    (rot, refv, x, y)
}

impl SeqNum {
    #[inline]
    fn new() -> Self {
        Self(0)
    }

    #[inline]
    fn next(&self) -> Self {
        Self(self.0 + 1)
    }

    /// The sequence number corresponding to the end of a scope.
    ///
    /// Currently implemented as [`u64::MAX`].
    #[inline]
    fn end() -> Self {
        Self(u64::MAX)
    }
}

fn object_id(id: &mut u64) -> ObjectId {
    let next_id = *id;
    *id += 1;
    ObjectId(next_id)
}

impl CompiledData {
    pub fn reachable_objs(&self, cell: CellId, scope: ScopeId) -> IndexMap<ObjectId, String> {
        let mut set = Default::default();
        self.reachable_objs_inner(cell, scope, SeqNum::end(), "", &mut set);
        set
    }

    fn reachable_objs_inner(
        &self,
        cell_id: CellId,
        scope_id: ScopeId,
        seq_num: SeqNum,
        name_prefix: &str,
        set: &mut IndexMap<ObjectId, String>,
    ) {
        let cell = &self.cells[&cell_id];
        let scope = &cell.scopes[&scope_id];
        if let Some((parent, seq_num)) = scope.static_parent {
            self.reachable_objs_inner(cell_id, parent, seq_num, name_prefix, set);
        }
        for (item_num, (name, obj)) in scope.bindings.iter() {
            if *item_num < seq_num {
                Self::insert_reachable_obj(obj, cell, &format!("{}{}", name_prefix, name), set);
            }
        }
    }

    fn insert_reachable_obj(
        value: &Arrayed<ObjectId>,
        cell: &CompiledCell,
        name: &str,
        set: &mut IndexMap<ObjectId, String>,
    ) {
        match value {
            Arrayed::Elem(obj) => match &cell.objects[obj] {
                SolvedValue::Rect(r) => {
                    set.insert(r.id, name.to_owned());
                }
                SolvedValue::Polygon(p) => {
                    set.insert(p.id, name.to_owned());
                }
                SolvedValue::Path(p) => {
                    set.insert(p.id, name.to_owned());
                }
                SolvedValue::Instance(inst) => {
                    set.insert(inst.id, name.to_owned());
                }
                _ => (),
            },
            Arrayed::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    Self::insert_reachable_obj(value, cell, &format!("{name}[{index}]"), set);
                }
            }
        }
    }
}
