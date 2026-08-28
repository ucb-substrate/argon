//! Definition and reference lookup over the type-checked AST.
//!
//! Name resolution is not a separate pass in this compiler: [`VarIdTyPass`]
//! resolves and type checks together, stamping a [`VarId`] onto every
//! declaration and onto every reference that binds to one. This module turns
//! that into something an editor can query by cursor position.
//!
//! A [`NavIndex`] holds, per file, the span of every identifier token together
//! with what it refers to, plus the reverse mapping from a definition to all
//! of its uses. Offsets are byte offsets into a file's editor-visible source,
//! which is what the language server converts client positions into.
//!
//! [`VarIdTyPass`]: crate::compile
//! [`VarId`]: crate::compile::VarId

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use indexmap::IndexMap;

use crate::{
    ast::{
        ArgDecl, CellDecl, Decl, EnumDecl, Expr, FnDecl, Ident, IdentPath, ModPath, Scope,
        Statement, TySpec, TySpecKind, UseDecl, WorkspaceAst,
    },
    compile::{BUILTINS, EnumId, Ty, VarId, VarIdTyMetadata, module_prefix},
    parse::CELL_PATH,
};

/// Identity of something that can be navigated to.
///
/// A [`VarId`] already distinguishes every `fn`, `cell`, `let`, parameter,
/// loop variable, and enum *name*, so it does most of the work. Enum variants
/// and modules are the two things it does not cover.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DefKey {
    Var(VarId),
    /// A variant, keyed by the `VarId` of its enum's name and its own name.
    Variant(VarId, String),
    Module(ModPath),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Cell,
    Local,
    Parameter,
    LoopVar,
    Enum,
    Variant,
    Module,
}

/// Where a definition can be shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefLocation {
    /// The identifier token that declares the symbol.
    ///
    /// The path may be a compiler-internal virtual path, most notably the
    /// embedded standard library; see [`crate::parse::virtual_source`].
    Source(crate::ast::Span),
    /// The file a module lives in. Modules have no declaring token.
    File(PathBuf),
    /// A declaration the compiler generated rather than one the user wrote —
    /// the cell signature synthesized for a GDS import. Recorded so that
    /// references to it still resolve, but there is nowhere to jump to.
    Generated,
}

#[derive(Debug, Clone)]
pub struct Definition {
    pub kind: SymbolKind,
    pub name: String,
    pub location: DefLocation,
}

/// An identifier that resolves to something the compiler provides rather than
/// to a declaration in source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Builtin {
    /// One of [`BUILTINS`].
    Function(&'static str),
    /// A primitive type name such as `Float` or `Rect`.
    Type(&'static str),
    /// A field of a primitive type, such as `Rect::x0`.
    Field(String),
    /// A keyword argument. Argon only permits these on builtin calls, so a
    /// keyword argument never names a parameter declared in source.
    KwArg(String),
}

/// What the identifier at some position refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Def(DefKey),
    Builtin(Builtin),
    /// A known identifier whose resolution failed. Type checking will have
    /// already reported an error here; recording it lets navigation answer
    /// "nothing to jump to" rather than "nothing is here".
    Unresolved,
}

/// Position-indexed navigation data for one workspace.
#[derive(Debug, Clone, Default)]
pub struct NavIndex {
    /// Identifier tokens per file, sorted by start offset. Entries are single
    /// tokens and never overlap.
    refs: HashMap<PathBuf, Vec<(cfgrammar::Span, Target)>>,
    defs: HashMap<DefKey, Definition>,
    usages: HashMap<DefKey, Vec<crate::ast::Span>>,
}

impl NavIndex {
    pub fn build(ast: &WorkspaceAst<VarIdTyMetadata>) -> Self {
        Builder::new(ast).run()
    }

    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// The files this index has entries for.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.refs.keys().map(PathBuf::as_path)
    }

    /// Whether this index still covers every file `previous` did.
    ///
    /// A file drops out of the index when it stops parsing, because the parser
    /// reports a failure rather than a partial tree and the module is left
    /// empty. That is a transient state while someone is typing, so it is a
    /// signal to keep serving the previous index rather than to replace it.
    /// `tracked` is the set of files the workspace still contains, so that a
    /// file which was genuinely removed does not pin the index forever.
    pub fn covers(&self, previous: &NavIndex, tracked: &HashSet<&Path>) -> bool {
        previous
            .files()
            .all(|path| self.refs.contains_key(path) || !tracked.contains(path))
    }

    /// The identifier token covering `offset` in `file`, and what it refers to.
    ///
    /// The offset range is inclusive of the token's end so that a cursor
    /// resting just after a name still resolves it, which is where editors
    /// leave it after `e` or `w`.
    pub fn target_at(&self, file: &Path, offset: usize) -> Option<(cfgrammar::Span, &Target)> {
        let entries = self.refs.get(file)?;
        let end = entries.partition_point(|(span, _)| span.start() <= offset);
        let (span, target) = entries[..end].last()?;
        (offset <= span.end()).then_some((*span, target))
    }

    pub fn definition(&self, key: &DefKey) -> Option<&Definition> {
        self.defs.get(key)
    }

    pub fn definition_at(&self, file: &Path, offset: usize) -> Option<&Definition> {
        match self.target_at(file, offset)? {
            (_, Target::Def(key)) => self.definition(key),
            _ => None,
        }
    }

    pub fn references(&self, key: &DefKey, include_declaration: bool) -> Vec<&crate::ast::Span> {
        let all = self.usages.get(key).map(Vec::as_slice).unwrap_or_default();
        if include_declaration {
            return all.iter().collect();
        }
        let declaration = match self.defs.get(key).map(|def| &def.location) {
            Some(DefLocation::Source(span)) => Some(span),
            _ => None,
        };
        all.iter()
            .filter(|span| Some(*span) != declaration)
            .collect()
    }

    pub fn references_at(
        &self,
        file: &Path,
        offset: usize,
        include_declaration: bool,
    ) -> Vec<&crate::ast::Span> {
        match self.target_at(file, offset) {
            Some((_, Target::Def(key))) => self.references(key, include_declaration),
            _ => Vec::new(),
        }
    }
}

/// Primitive type names, kept as `'static` so a reference to one costs nothing.
const BUILTIN_TYPES: [&str; 9] = [
    "Bool", "Int", "Float", "Rect", "Polygon", "Path", "Point", "Any", "String",
];

fn builtin_type(name: &str) -> Option<&'static str> {
    BUILTIN_TYPES.into_iter().find(|builtin| *builtin == name)
}

fn builtin_function(name: &str) -> Option<&'static str> {
    BUILTINS.into_iter().find(|builtin| *builtin == name)
}

struct Builder<'a> {
    ast: &'a WorkspaceAst<VarIdTyMetadata>,
    /// Which file each module lives in.
    files: IndexMap<&'a ModPath, &'a Path>,
    /// The `VarId` an enum's name is bound to, by the id inside its [`Ty`].
    enums: HashMap<EnumId, VarId>,
    /// Cell `VarId` to field name to the `VarId` of the `let` declaring it.
    cell_fields: HashMap<VarId, HashMap<String, VarId>>,
    /// Module currently being walked.
    current: &'a ModPath,
    path: &'a Path,
    /// Length of the file's editor-visible source. Generated declarations are
    /// appended past this point.
    visible: usize,
    index: NavIndex,
}

/// Whether a module is real source rather than a compiler-internal splice.
fn is_navigable(path: &Path) -> bool {
    path != Path::new(CELL_PATH)
}

impl<'a> Builder<'a> {
    fn new(ast: &'a WorkspaceAst<VarIdTyMetadata>) -> Self {
        let mut builder = Self {
            ast,
            files: IndexMap::new(),
            enums: HashMap::new(),
            cell_fields: HashMap::new(),
            current: const { &Vec::new() },
            path: Path::new(""),
            visible: 0,
            index: NavIndex::default(),
        };
        builder.collect_declarations();
        builder
    }

    /// Records what the reference walk needs to have seen already: which file
    /// each module is in, which `VarId` each enum's name holds, and which
    /// `let` declares each of a cell's fields. All three can be referred to
    /// from a module that is walked earlier.
    fn collect_declarations(&mut self) {
        for (module, ast) in self.ast.iter() {
            if !is_navigable(&ast.path) {
                continue;
            }
            self.files.insert(module, &ast.path);
            self.index.defs.insert(
                DefKey::Module(module.clone()),
                Definition {
                    kind: SymbolKind::Module,
                    name: module.last().cloned().unwrap_or_default(),
                    location: DefLocation::File(ast.path.clone()),
                },
            );
            for decl in &ast.ast.decls {
                match decl {
                    Decl::Enum(decl) => {
                        self.enums.insert(decl.metadata.1, decl.metadata.0);
                    }
                    Decl::Cell(decl) => {
                        let fields = decl
                            .scope
                            .stmts
                            .iter()
                            .filter_map(|stmt| match stmt {
                                Statement::LetBinding(binding) => {
                                    Some((binding.name.name.to_string(), binding.metadata))
                                }
                                _ => None,
                            })
                            .collect();
                        self.cell_fields.insert(decl.metadata.1, fields);
                    }
                    _ => {}
                }
            }
        }
    }

    fn run(mut self) -> NavIndex {
        for (module, ast) in self.ast.iter() {
            if !is_navigable(&ast.path) {
                continue;
            }
            self.current = module;
            self.path = &ast.path;
            self.visible = ast.source_text.len();
            for decl in &ast.ast.decls {
                self.decl(decl);
            }
        }
        for entries in self.index.refs.values_mut() {
            entries.sort_by_key(|(span, _)| (span.start(), std::cmp::Reverse(span.end())));
        }
        self.index
    }

    /// True when `span` lies in text the user wrote rather than in a generated
    /// declaration appended after it.
    fn visible(&self, span: cfgrammar::Span) -> bool {
        span.end() <= self.visible
    }

    fn span(&self, span: cfgrammar::Span) -> crate::ast::Span {
        crate::ast::Span {
            path: self.path.to_path_buf(),
            span,
        }
    }

    fn record(&mut self, span: cfgrammar::Span, target: Target) {
        if !self.visible(span) {
            return;
        }
        if let Target::Def(key) = &target {
            let location = self.span(span);
            self.index
                .usages
                .entry(key.clone())
                .or_default()
                .push(location);
        }
        self.index
            .refs
            .entry(self.path.to_path_buf())
            .or_default()
            .push((span, target));
    }

    fn define(
        &mut self,
        key: DefKey,
        kind: SymbolKind,
        name: &Ident<arcstr::Substr, VarIdTyMetadata>,
    ) {
        let location = if self.visible(name.span) {
            DefLocation::Source(self.span(name.span))
        } else {
            DefLocation::Generated
        };
        self.index.defs.insert(
            key.clone(),
            Definition {
                kind,
                name: name.name.to_string(),
                location,
            },
        );
        // A declaration is also a reference to itself, so that a cursor on the
        // name navigates and `references` from the declaration finds the uses.
        self.record(name.span, Target::Def(key));
    }

    // ---------------------------------------------------------------- decls

    fn decl(&mut self, decl: &'a Decl<arcstr::Substr, VarIdTyMetadata>) {
        match decl {
            Decl::Enum(decl) => self.enum_decl(decl),
            Decl::Cell(decl) => self.cell_decl(decl),
            Decl::Fn(decl) => self.fn_decl(decl),
            Decl::Mod(decl) => {
                let mut target = self.current.clone();
                target.push(decl.ident.name.to_string());
                let key = DefKey::Module(target);
                let target = if self.index.defs.contains_key(&key) {
                    Target::Def(key)
                } else {
                    Target::Unresolved
                };
                self.record(decl.ident.span, target);
            }
            Decl::Use(decl) => self.use_decl(decl),
            // Rejected by `parse_ast` before this pass ever runs.
            Decl::Struct(_) | Decl::Constant(_) => {}
        }
    }

    fn enum_decl(&mut self, decl: &'a EnumDecl<arcstr::Substr, VarIdTyMetadata>) {
        let name_id = decl.metadata.0;
        self.define(DefKey::Var(name_id), SymbolKind::Enum, &decl.name);
        for variant in &decl.variants {
            self.define(
                DefKey::Variant(name_id, variant.name.to_string()),
                SymbolKind::Variant,
                variant,
            );
        }
    }

    fn cell_decl(&mut self, decl: &'a CellDecl<arcstr::Substr, VarIdTyMetadata>) {
        self.define(DefKey::Var(decl.metadata.1), SymbolKind::Cell, &decl.name);
        for arg in &decl.args {
            self.arg_decl(arg);
        }
        self.scope(&decl.scope);
    }

    fn fn_decl(&mut self, decl: &'a FnDecl<arcstr::Substr, VarIdTyMetadata>) {
        self.define(
            DefKey::Var(decl.metadata.1),
            SymbolKind::Function,
            &decl.name,
        );
        for arg in &decl.args {
            self.arg_decl(arg);
        }
        if let Some(spec) = &decl.return_ty {
            // The declared return type, recovered from the function's own type.
            let ret = match &decl.metadata.2 {
                Ty::Fn(fn_ty) => fn_ty.ret.clone(),
                _ => Ty::Unknown,
            };
            self.ty_spec(spec, &ret);
        }
        self.scope(&decl.scope);
    }

    fn use_decl(&mut self, decl: &'a UseDecl<arcstr::Substr, VarIdTyMetadata>) {
        let Some((item, prefix)) = decl.path.split_last() else {
            return;
        };
        self.module_path(prefix);
        // `use` has no metadata of its own; the imported binding reuses the
        // original declaration's `VarId`, so resolve it structurally against
        // the exporting module's declarations.
        let module = module_prefix(self.current, prefix.iter().map(|ident| ident.name.as_str()));
        let target = self
            .exported(&module, &item.name)
            .map_or(Target::Unresolved, |id| Target::Def(DefKey::Var(id)));
        self.record(item.span, target.clone());
        // An alias is another name for the same binding, not a new definition,
        // so navigating from it lands on the original declaration.
        if let Some(alias) = &decl.alias {
            self.record(alias.span, target);
        }
    }

    /// The `VarId` a module exports under `name`, if any.
    fn exported(&self, module: &ModPath, name: &str) -> Option<VarId> {
        self.ast
            .get(module)?
            .ast
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Fn(decl) if decl.name.name == name => Some(decl.metadata.1),
                Decl::Cell(decl) if decl.name.name == name => Some(decl.metadata.1),
                Decl::Enum(decl) if decl.name.name == name => Some(decl.metadata.0),
                _ => None,
            })
    }

    fn arg_decl(&mut self, arg: &'a ArgDecl<arcstr::Substr, VarIdTyMetadata>) {
        self.define(
            DefKey::Var(arg.metadata.0),
            SymbolKind::Parameter,
            &arg.name,
        );
        let ty = arg.metadata.1.clone();
        self.ty_spec(&arg.ty, &ty);
    }

    // ------------------------------------------------------------ type specs

    /// Walks a type annotation alongside the [`Ty`] it resolved to.
    ///
    /// `ty_from_spec` maps the two structures onto each other one-for-one, so
    /// zipping them recovers what each leaf name resolved to without the AST
    /// having to carry it. A structural mismatch means the annotation failed
    /// to resolve, and its names are reported as unresolved.
    fn ty_spec(&mut self, spec: &'a TySpec<arcstr::Substr, VarIdTyMetadata>, ty: &Ty) {
        match (&spec.kind, ty) {
            (TySpecKind::Ident(name), Ty::Enum(enum_ty)) => {
                let target = self
                    .enums
                    .get(&enum_ty.id)
                    .map_or(Target::Unresolved, |id| Target::Def(DefKey::Var(*id)));
                self.record(name.span, target);
            }
            (TySpecKind::Ident(name), _) => {
                let target = builtin_type(&name.name).map_or(Target::Unresolved, |name| {
                    Target::Builtin(Builtin::Type(name))
                });
                self.record(name.span, target);
            }
            (TySpecKind::Seq(inner), Ty::Seq(element)) => self.ty_spec(inner, element),
            (TySpecKind::Tuple(items), Ty::Tuple(types)) if items.len() == types.len() => {
                for (item, ty) in items.iter().zip(types) {
                    self.ty_spec(item, ty);
                }
            }
            (TySpecKind::Seq(inner), _) => self.ty_spec(inner, &Ty::Unknown),
            (TySpecKind::Tuple(items), _) => {
                for item in items {
                    self.ty_spec(item, &Ty::Unknown);
                }
            }
        }
    }

    // ----------------------------------------------------------- statements

    fn scope(&mut self, scope: &'a Scope<arcstr::Substr, VarIdTyMetadata>) {
        for stmt in &scope.stmts {
            match stmt {
                Statement::Expr { value, .. } => self.expr(value),
                Statement::LetBinding(binding) => {
                    // The initializer is evaluated before the name is bound.
                    self.expr(&binding.value);
                    self.define(
                        DefKey::Var(binding.metadata),
                        SymbolKind::Local,
                        &binding.name,
                    );
                }
                Statement::ForLoop(loop_) => {
                    self.expr(&loop_.seq);
                    self.define(DefKey::Var(loop_.metadata), SymbolKind::LoopVar, &loop_.var);
                    self.scope(&loop_.body);
                }
            }
        }
        if let Some(tail) = &scope.tail {
            self.expr(tail);
        }
    }

    // ---------------------------------------------------------- expressions

    fn expr(&mut self, expr: &'a Expr<arcstr::Substr, VarIdTyMetadata>) {
        match expr {
            Expr::IdentPath(path) => self.ident_path(path),
            Expr::Call(call) => {
                let Some((callee, prefix)) = call.func.path.split_last() else {
                    return;
                };
                self.module_path(prefix);
                let target = match call.metadata.0 {
                    Some(id) => Target::Def(DefKey::Var(id)),
                    // Builtins are matched by name and never bound, so an
                    // unbound single-segment call is one of them.
                    None if prefix.is_empty() => builtin_function(&callee.name)
                        .map_or(Target::Unresolved, |name| {
                            Target::Builtin(Builtin::Function(name))
                        }),
                    None => Target::Unresolved,
                };
                self.record(callee.span, target);
                for arg in &call.args.posargs {
                    self.expr(arg);
                }
                for kwarg in &call.args.kwargs {
                    self.record(
                        kwarg.name.span,
                        Target::Builtin(Builtin::KwArg(kwarg.name.name.to_string())),
                    );
                    self.expr(&kwarg.value);
                }
            }
            Expr::FieldAccess(access) => {
                let base = access.base.ty();
                self.expr(&access.base);
                let target = self.field_target(&base, &access.field.name);
                self.record(access.field.span, target);
            }
            Expr::Cast(cast) => {
                self.expr(&cast.value);
                let ty = cast.metadata.clone();
                self.ty_spec(&cast.ty, &ty);
            }
            Expr::Match(match_) => {
                self.expr(&match_.scrutinee);
                for arm in &match_.arms {
                    self.ident_path(&arm.pattern);
                    self.expr(&arm.expr);
                }
            }
            Expr::If(if_) => {
                self.expr(&if_.cond);
                self.scope(&if_.then);
                self.scope(&if_.else_);
            }
            Expr::Comparison(cmp) => {
                self.expr(&cmp.left);
                self.expr(&cmp.right);
            }
            Expr::BinOp(op) => {
                self.expr(&op.left);
                self.expr(&op.right);
            }
            Expr::UnaryOp(op) => self.expr(&op.operand),
            Expr::Emit(emit) => self.expr(&emit.value),
            Expr::IndexFieldAccess(access) => self.expr(&access.base),
            Expr::Index(index) => {
                self.expr(&index.base);
                self.expr(&index.index);
            }
            Expr::Scope(scope) => self.scope(scope),
            Expr::Tuple(tuple) => {
                for item in &tuple.items {
                    self.expr(item);
                }
            }
            Expr::Nil(_)
            | Expr::SeqNil(_)
            | Expr::FloatLiteral(_)
            | Expr::IntLiteral(_)
            | Expr::StringLiteral(_)
            | Expr::BoolLiteral(_) => {}
        }
    }

    fn ident_path(&mut self, path: &'a IdentPath<arcstr::Substr, VarIdTyMetadata>) {
        match path.path.as_slice() {
            [] => {}
            [name] => {
                let target = path
                    .metadata
                    .0
                    .map_or(Target::Unresolved, |id| Target::Def(DefKey::Var(id)));
                self.record(name.span, target);
            }
            // A multi-segment path is an enum variant, optionally qualified by
            // the module the enum lives in.
            segments => {
                let (variant, rest) = segments.split_last().expect("non-empty");
                let (enum_name, prefix) = rest.split_last().expect("at least two segments");
                self.module_path(prefix);
                let enum_id = match &path.metadata.1 {
                    Ty::Enum(enum_ty) => self.enums.get(&enum_ty.id).copied(),
                    _ => None,
                };
                let (enum_target, variant_target) = match enum_id {
                    Some(id) => (
                        Target::Def(DefKey::Var(id)),
                        Target::Def(DefKey::Variant(id, variant.name.to_string())),
                    ),
                    None => (Target::Unresolved, Target::Unresolved),
                };
                self.record(enum_name.span, enum_target);
                self.record(variant.span, variant_target);
            }
        }
    }

    /// Records each leading segment of a qualified path as the module it names.
    fn module_path(&mut self, prefix: &'a [Ident<arcstr::Substr, VarIdTyMetadata>]) {
        for length in 1..=prefix.len() {
            let module = module_prefix(
                self.current,
                prefix[..length].iter().map(|ident| ident.name.as_str()),
            );
            let key = DefKey::Module(module);
            let target = if self.index.defs.contains_key(&key) {
                Target::Def(key)
            } else {
                Target::Unresolved
            };
            self.record(prefix[length - 1].span, target);
        }
    }

    /// What `name` refers to when read off a value of type `base`.
    fn field_target(&self, base: &Ty, name: &str) -> Target {
        match base {
            // An instance's fields are the cell's top-level `let` bindings.
            // `x` and `y` are the instance's own placement, checked first by
            // the type pass and shadowing any binding of the same name.
            Ty::Inst(cell) if name != "x" && name != "y" => cell
                .def
                .and_then(|cell_id| self.cell_fields.get(&cell_id))
                .and_then(|fields| fields.get(name))
                .map_or(Target::Unresolved, |id| Target::Def(DefKey::Var(*id))),
            Ty::Inst(_) | Ty::Rect | Ty::Polygon | Ty::Path | Ty::Point => {
                Target::Builtin(Builtin::Field(name.to_string()))
            }
            _ => Target::Unresolved,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use indexmap::IndexMap;

    use super::*;
    use crate::{
        compile::static_compile,
        parse::{STD_PATH, STD_SOURCE, parse_source_text, parse_workspace_with_std},
    };

    const ROOT: &str = "/virtual/lib.ar";
    /// Marks the cursor position in a fixture.
    const CURSOR: &str = "$0";

    /// Strips every [`CURSOR`] marker, returning the clean source and the byte
    /// offsets the markers stood at.
    fn cursors(source: &str) -> (String, Vec<usize>) {
        let mut clean = String::with_capacity(source.len());
        let mut offsets = Vec::new();
        let mut rest = source;
        while let Some(at) = rest.find(CURSOR) {
            clean.push_str(&rest[..at]);
            offsets.push(clean.len());
            rest = &rest[at + CURSOR.len()..];
        }
        clean.push_str(rest);
        (clean, offsets)
    }

    /// Builds a one-file workspace (plus the standard library) and its index.
    fn index(source: &str) -> (String, NavIndex, Vec<usize>) {
        let (source, offsets) = cursors(source);
        let root = parse_source_text(source.clone(), PathBuf::from(ROOT)).unwrap();
        let std = parse_source_text(STD_SOURCE, PathBuf::from(STD_PATH)).unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (typed, _) = static_compile(&ast).unwrap();
        (source, NavIndex::build(&typed), offsets)
    }

    /// The text a definition points at, as `name#occurrence`, so an assertion
    /// pins down not just the name but which occurrence of it was reached.
    fn at(source: &str, span: &crate::ast::Span) -> String {
        let text = &source[span.span.start()..span.span.end()];
        let occurrence = source
            .match_indices(text)
            .position(|(start, _)| start == span.span.start())
            .expect("the span covers its own text");
        format!("{text}#{occurrence}")
    }

    /// Asserts that the definition found at each cursor renders as expected.
    #[track_caller]
    fn check(source: &str, expected: &[&str]) {
        let (source, index, offsets) = index(source);
        assert_eq!(offsets.len(), expected.len(), "cursor/expectation count");
        let found: Vec<String> = offsets
            .iter()
            .map(
                |offset| match index.definition_at(Path::new(ROOT), *offset) {
                    Some(Definition {
                        location: DefLocation::Source(span),
                        ..
                    }) => at(&source, span),
                    Some(Definition {
                        location: DefLocation::File(path),
                        ..
                    }) => format!("file:{}", path.display()),
                    Some(Definition {
                        location: DefLocation::Generated,
                        ..
                    }) => "generated".to_owned(),
                    None => match index.target_at(Path::new(ROOT), *offset) {
                        Some((_, target)) => format!("{target:?}"),
                        None => "none".to_owned(),
                    },
                },
            )
            .collect();
        let expected: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn locals_parameters_and_loop_variables() {
        check(
            r#"
cell top(wid$0th: Float) {
    let base = 1.;
    let scaled = ba$0se + wid$0th;
    for step in [] {
        eq(st$0ep, scal$0ed);
    }
}
"#,
            &["width#0", "base#0", "width#0", "step#0", "scaled#0"],
        );
    }

    #[test]
    fn an_inner_binding_shadows_an_outer_one() {
        check(
            r#"
cell top() {
    let x = 1.;
    let y = {
        let x = 2.;
        x$0
    };
    eq(x$0, y);
}
"#,
            // The tail `x` reaches the inner `let`; the `eq` argument the outer.
            &["x#1", "x#0"],
        );
    }

    #[test]
    fn functions_and_cells_resolve_from_their_call_sites() {
        check(
            r#"
fn dou$0ble(v: Float) -> Float { v * 2. }

cell inner() {
    let r = rect("met1");
}

cell top() {
    let a = dou$0ble(1.);
    let c = inn$0er();
    let i = inst(c);
}
"#,
            &["double#0", "double#0", "inner#0"],
        );
    }

    /// Cells are not hoisted: `VarIdTyPass` binds a cell only after walking its
    /// body, so using one before its declaration is a `UseBeforeDeclaration`
    /// error (see `examples/cell_out_of_order`). Navigation reports what the
    /// compiler resolved, so there is deliberately nothing to jump to.
    #[test]
    fn a_cell_used_before_its_declaration_is_unresolved() {
        check(
            r#"
cell top() {
    let c = lat$0er();
    let i = inst(c);
}

cell later() {
    let r = rect("met1");
}
"#,
            &["Unresolved"],
        );
    }

    #[test]
    fn enums_variants_annotations_and_match_arms() {
        check(
            r#"
enum Mo$0de { Fast, Slow, }

fn pick(mode: Mo$0de) -> Float {
    match mode {
        Mode::Fa$0st => 1.,
        Mod$0e::Slow => 2.,
    }
}

cell top() {
    let v = Mode::Sl$0ow;
    eq(rect("met1").w, pick(v));
}
"#,
            &["Mode#0", "Mode#0", "Fast#0", "Mode#0", "Slow#0"],
        );
    }

    #[test]
    fn a_cell_field_resolves_to_the_let_that_declares_it() {
        check(
            r#"
cell inner() {
    let met = rect("met1");
}

cell top() {
    let c = inner();
    let i = inst(c);
    eq(i.me$0t.x0, 0.);
}
"#,
            &["met#0"],
        );
    }

    #[test]
    fn builtins_are_classified_but_have_no_definition() {
        check(
            r#"
cell top(w: Flo$0at) {
    let r = re$0ct("met1", x$00=0., y0=0., x1=w, y1=w);
    eq(r.x$00, 0.);
}
"#,
            &[
                r#"Builtin(Type("Float"))"#,
                r#"Builtin(Function("rect"))"#,
                r#"Builtin(KwArg("x0"))"#,
                r#"Builtin(Field("x0"))"#,
            ],
        );
    }

    #[test]
    fn the_standard_library_is_navigable() {
        let (_, index, offsets) = index(
            r#"
cell top() {
    let r = rect("met1");
    eq(r.w, std::ma$0x(1., 2.));
}
"#,
        );
        let definition = index
            .definition_at(Path::new(ROOT), offsets[0])
            .expect("std::max resolves");
        let DefLocation::Source(span) = &definition.location else {
            panic!("expected a source location, got {:?}", definition.location);
        };
        assert_eq!(span.path, Path::new(STD_PATH));
        assert_eq!(
            &STD_SOURCE[span.span.start()..span.span.end()],
            "max",
            "the span should cover the declaration's name"
        );
        assert_eq!(definition.kind, SymbolKind::Function);
    }

    #[test]
    fn references_cover_every_use_and_can_exclude_the_declaration() {
        let (source, index, offsets) = index(
            r#"
cell top() {
    let wid$0th = 1.;
    let a = width + 1.;
    let b = width * 2.;
    eq(rect("met1").w, a + b);
}
"#,
        );
        let (_, Target::Def(key)) = index
            .target_at(Path::new(ROOT), offsets[0])
            .expect("cursor is on a name")
        else {
            panic!("expected a definition target");
        };
        let with: Vec<String> = index
            .references(key, true)
            .iter()
            .map(|span| at(&source, span))
            .collect();
        assert_eq!(with, ["width#0", "width#1", "width#2"]);
        let without: Vec<String> = index
            .references(key, false)
            .iter()
            .map(|span| at(&source, span))
            .collect();
        assert_eq!(without, ["width#1", "width#2"]);
    }

    #[test]
    fn navigation_survives_a_type_error_elsewhere() {
        check(
            r#"
cell top() {
    let good = 1.;
    let bad = "text" + 1;
    eq(rect("met1").w, go$0od);
}
"#,
            &["good#0"],
        );
    }

    // ------------------------------------------------------------ workspaces

    fn examples(name: &str) -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples")).join(name)
    }

    /// Index for a whole on-disk example library.
    fn workspace(root_lib: &Path) -> NavIndex {
        let output = parse_workspace_with_std(root_lib);
        let (typed, _) = static_compile(&output.ast()).expect("a root module");
        NavIndex::build(&typed)
    }

    #[test]
    fn modules_resolve_to_their_files() {
        let root = examples("argon_library/lib.ar");
        let index = workspace(&root);
        let source = std::fs::read_to_string(&root).unwrap();
        let expect_file = |needle: &str, occurrence: usize, file: &str| {
            let offset = source
                .match_indices(needle)
                .nth(occurrence)
                .unwrap_or_else(|| panic!("no occurrence {occurrence} of {needle}"))
                .0;
            let definition = index
                .definition_at(&root, offset)
                .unwrap_or_else(|| panic!("{needle} at {offset} resolves"));
            let DefLocation::File(path) = &definition.location else {
                panic!(
                    "expected a file location for {needle}, got {:?}",
                    definition.location
                );
            };
            assert!(
                path.ends_with(file),
                "{needle} resolved to {} not {file}",
                path.display()
            );
        };
        // `mod utils;` and `mod nested;`
        expect_file("utils", 0, "utils.ar");
        expect_file("nested", 0, "nested/mod.ar");
        // The `nested::` and `lib::utils::` prefixes of the call paths.
        expect_file("nested", 1, "nested/mod.ar");
        expect_file("nested", 2, "nested/nested.ar");
        expect_file("utils", 1, "utils.ar");
    }

    #[test]
    fn a_dependency_librarys_items_resolve_across_the_workspace() {
        let root = examples("path_dependencies/root_library/lib.ar");
        let library = arc_config(&root);
        let output = crate::parse::parse_workspace_with_config(&library);
        let (typed, _) = static_compile(&output.ast()).expect("a root module");
        let index = NavIndex::build(&typed);
        let source = std::fs::read_to_string(&root).unwrap();
        let offset = source.find("x1()").unwrap();
        let definition = index.definition_at(&root, offset).expect("x1 resolves");
        let DefLocation::Source(span) = &definition.location else {
            panic!("expected a source location, got {:?}", definition.location);
        };
        assert!(
            span.path.ends_with("dependency_library/lib.ar"),
            "resolved to {}",
            span.path.display()
        );
        assert_eq!(definition.kind, SymbolKind::Function);
    }

    fn arc_config(root_lib: &Path) -> crate::WorkspaceConfig {
        let directory = root_lib.parent().unwrap();
        crate::WorkspaceConfig::new(root_lib).with_dependencies([(
            "dependency".to_owned(),
            directory.join("../dependency_library"),
        )])
    }

    /// Structural invariants that must hold for every example in the corpus.
    #[test]
    fn corpus_indexes_are_well_formed() {
        let examples_dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples"));
        let mut checked = 0;
        for entry in std::fs::read_dir(&examples_dir).unwrap() {
            let root = entry.unwrap().path().join("lib.ar");
            if !root.is_file() {
                continue;
            }
            let output = parse_workspace_with_std(&root);
            let parsed = output.ast();
            let Some((typed, _)) = static_compile(&parsed) else {
                continue;
            };
            if typed.is_empty() {
                continue;
            }
            checked += 1;
            let index = NavIndex::build(&typed);
            let visible: IndexMap<&Path, usize> = typed
                .values()
                .map(|module| (module.path.as_path(), module.source_text.len()))
                .collect();

            for (path, entries) in &index.refs {
                assert_ne!(path, Path::new(crate::parse::CELL_PATH));
                let limit = visible[path.as_path()];
                let mut previous: Option<cfgrammar::Span> = None;
                for (span, target) in entries {
                    assert!(
                        span.end() <= limit,
                        "{}: {span:?} is past the editor-visible source",
                        path.display()
                    );
                    if let Some(previous) = previous {
                        assert!(
                            previous.end() <= span.start(),
                            "{}: {previous:?} overlaps {span:?}",
                            path.display()
                        );
                    }
                    previous = Some(*span);
                    if let Target::Def(key) = target {
                        assert!(
                            index.defs.contains_key(key),
                            "{}: {key:?} has no definition",
                            path.display()
                        );
                        let location = crate::ast::Span {
                            path: path.clone(),
                            span: *span,
                        };
                        assert!(
                            index.usages[key].contains(&location),
                            "{}: {span:?} missing from the usages of {key:?}",
                            path.display()
                        );
                    }
                }
            }

            // Every recorded declaration names a real identifier.
            for (key, definition) in &index.defs {
                let DefLocation::Source(span) = &definition.location else {
                    continue;
                };
                let module = typed
                    .values()
                    .find(|module| module.path == span.path)
                    .unwrap_or_else(|| panic!("{key:?} points outside the workspace"));
                assert_eq!(
                    &module.text[span.span.start()..span.span.end()],
                    definition.name,
                    "{key:?} span does not cover its name"
                );
            }

            // Lookup agrees with the table at both ends of every token.
            for (path, entries) in &index.refs {
                for (span, _) in entries {
                    for offset in [span.start(), span.end() - 1, span.end()] {
                        assert_eq!(
                            index.target_at(path, offset).map(|(span, _)| span),
                            Some(*span),
                            "{}: lookup at {offset} missed {span:?}",
                            path.display()
                        );
                    }
                }
            }
        }
        assert!(checked > 20, "expected a substantial corpus, got {checked}");
    }
}
