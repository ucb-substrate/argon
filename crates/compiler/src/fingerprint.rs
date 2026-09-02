//! Content fingerprints for top-level declarations.
//!
//! A fingerprint names a declaration by what it *is* -- its own text, and the
//! fingerprints of everything it refers to -- so two revisions of a workspace
//! agree on it exactly when the declaration means the same thing in both. That
//! is what lets a compiled cell be recognised across an edit, which `VarId`
//! cannot do: ids come from one workspace-global counter threaded across
//! modules, so editing an early module renumbers every declaration after it.
//!
//! ```text
//! self_hash(item) = H( kind, file, module, name, body )
//! fp(scc)         = H( sorted self_hash of members, sorted fp of dependency SCCs )
//! fp(item)        = H( fp(scc(item)), self_hash(item) )
//! ```
//!
//! Declarations are condensed into strongly-connected components first, since
//! functions may refer to one another in any order and may recurse.
//!
//! Fingerprints are *conservative*: a formatting change invalidates a
//! declaration whose meaning is unchanged. Two different meanings sharing a
//! fingerprint must never happen, so anything the walker cannot account for
//! has to make the fingerprint differ.

use std::{
    collections::HashMap,
    hash::Hasher,
    ops::Range,
    path::{Path, PathBuf},
};

use indexmap::{IndexMap, IndexSet};

use crate::{
    ast::{
        ArgDecl, Decl, Expr, IdentPath, ModPath, Scope, Statement, WorkspaceAst,
        annotated::AnnotatedAst,
    },
    compile::{EnumId, Ty, VarId, VarIdTyMetadata},
};

/// A declaration's content fingerprint.
pub type Fingerprint = u64;

/// The kinds of declaration a compiled cell's output can depend on.
///
/// `mod` and `use` are absent because neither can be called or named as a type,
/// and a reference through an alias resolves to the original declaration's own
/// `VarId`. `struct` and `const` are absent because `parser::check_unsupported`
/// rejects both; the matches over [`Decl`] below are exhaustive so that
/// supporting either becomes a compile error here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Cell,
    Fn,
    Enum,
}

impl ItemKind {
    fn tag(self) -> u8 {
        match self {
            Self::Cell => 0,
            Self::Fn => 1,
            Self::Enum => 2,
        }
    }
}

/// Where a declaration is written, and what it fingerprints to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemSite {
    pub fingerprint: Fingerprint,
    pub kind: ItemKind,
    pub path: PathBuf,
    /// Byte range of the declaration within its module's backing text, which
    /// is what every `cfgrammar::Span` in that module indexes.
    ///
    /// An `enum` declaration carries no span of its own, so its name's span
    /// stands in; an enum body contains no executable code, so no compiled
    /// span can fall inside one.
    pub span: Range<usize>,
}

/// Fingerprints and locations for every top-level declaration in a workspace.
#[derive(Clone, Debug, Default)]
pub struct ItemIndex {
    sites: IndexMap<VarId, ItemSite>,
}

impl ItemIndex {
    pub fn build(ast: &WorkspaceAst<VarIdTyMetadata>) -> Self {
        let mut builder = Builder::default();
        builder.collect_declarations(ast);
        builder.collect_dependencies(ast);
        Self {
            sites: builder.finish(),
        }
    }

    pub fn fingerprint(&self, var: VarId) -> Option<Fingerprint> {
        self.sites.get(&var).map(|site| site.fingerprint)
    }

    pub fn site(&self, var: VarId) -> Option<&ItemSite> {
        self.sites.get(&var)
    }

    pub fn len(&self) -> usize {
        self.sites.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (VarId, &ItemSite)> {
        self.sites.iter().map(|(var, site)| (*var, site))
    }
}

/// One declaration, before its dependencies are known.
struct Item {
    kind: ItemKind,
    path: PathBuf,
    span: Range<usize>,
    self_hash: u64,
    deps: IndexSet<VarId>,
}

#[derive(Default)]
struct Builder {
    items: IndexMap<VarId, Item>,
    /// The `VarId` an enum's name is bound to, by the id carried in its [`Ty`].
    ///
    /// A reference to an enum *variant* is a multi-segment path, which
    /// `dispatch_ident_path` reports as `(None, ty)` with no `VarId` at all,
    /// so this map is the only way such a reference reaches a fingerprint.
    enums: HashMap<EnumId, VarId>,
}

fn hasher() -> fnv::FnvHasher {
    fnv::FnvHasher::default()
}

/// Writes a length-prefixed string, so that the boundary between two of them is
/// recoverable from the digest and `["ab", "c"]` cannot collide with
/// `["a", "bc"]`.
fn write_str(hasher: &mut impl Hasher, value: &str) {
    hasher.write_usize(value.len());
    hasher.write(value.as_bytes());
}

fn write_module(hasher: &mut impl Hasher, module: &ModPath) {
    hasher.write_usize(module.len());
    for segment in module {
        write_str(hasher, segment);
    }
}

impl Builder {
    /// Records every declaration and its own hash, without looking at what it
    /// refers to.
    fn collect_declarations(&mut self, ast: &WorkspaceAst<VarIdTyMetadata>) {
        // Enums first: a declaration walked below may name a variant of an enum
        // declared in any module, and resolving that needs the whole map.
        for annotated in ast.values() {
            for decl in declarations(annotated) {
                if let Decl::Enum(decl) = decl
                    && let Some((name_id, enum_id)) = decl.metadata
                {
                    self.enums.insert(enum_id, name_id);
                }
            }
        }

        for (module, annotated) in ast.iter() {
            for decl in declarations(annotated) {
                let (var, kind, name, span) = match decl {
                    Decl::Cell(decl) => (
                        decl.metadata.1,
                        ItemKind::Cell,
                        decl.name.name.as_str(),
                        span_range(decl.span),
                    ),
                    Decl::Fn(decl) => (
                        decl.metadata.1,
                        ItemKind::Fn,
                        decl.name.name.as_str(),
                        span_range(decl.span),
                    ),
                    Decl::Enum(decl) => {
                        let Some((var, _)) = decl.metadata else {
                            continue;
                        };
                        (
                            var,
                            ItemKind::Enum,
                            decl.name.name.as_str(),
                            span_range(decl.name.span),
                        )
                    }
                    Decl::Struct(_) | Decl::Constant(_) | Decl::Mod(_) | Decl::Use(_) => {
                        continue;
                    }
                };

                let mut hasher = hasher();
                hasher.write_u8(kind.tag());
                // The file and module qualify the name. This is load-bearing
                // rather than decorative: `CompiledCell::name` becomes the GDS
                // structure name, so two byte-identical cells in different
                // modules must not share a fingerprint -- and it is what makes
                // a `use` re-pointed at an identically-written declaration
                // elsewhere invalidate its dependents.
                write_str(&mut hasher, &annotated.path.to_string_lossy());
                write_module(&mut hasher, module);
                write_str(&mut hasher, name);
                match decl {
                    // A cell's or function's declaration span runs from its
                    // keyword to its closing brace with no surrounding trivia,
                    // so this is exactly the declaration's text.
                    Decl::Cell(_) | Decl::Fn(_) => {
                        write_str(&mut hasher, &annotated.text[span.clone()]);
                    }
                    // An `enum` has no span to slice, and its structure is its
                    // whole declaration anyway.
                    Decl::Enum(decl) => {
                        hasher.write_usize(decl.variants.len());
                        for variant in &decl.variants {
                            write_str(&mut hasher, &variant.name);
                        }
                    }
                    Decl::Struct(_) | Decl::Constant(_) | Decl::Mod(_) | Decl::Use(_) => {
                        unreachable!("filtered above")
                    }
                }

                self.items.insert(
                    var,
                    Item {
                        kind,
                        path: annotated.path.clone(),
                        span,
                        self_hash: hasher.finish(),
                        deps: IndexSet::new(),
                    },
                );
            }
        }
    }

    /// Records what each declaration refers to, keeping only references that
    /// resolve to another top-level declaration. A `VarId` naming a local, a
    /// parameter, or a loop variable is dropped: whatever defines it is already
    /// inside the text this declaration hashes.
    fn collect_dependencies(&mut self, ast: &WorkspaceAst<VarIdTyMetadata>) {
        for annotated in ast.values() {
            for decl in declarations(annotated) {
                let (var, mut deps) = match decl {
                    Decl::Cell(decl) => {
                        let mut deps = IndexSet::new();
                        for arg in &decl.args {
                            self.arg_decl(arg, &mut deps);
                        }
                        self.scope(&decl.scope, &mut deps);
                        (decl.metadata.1, deps)
                    }
                    Decl::Fn(decl) => {
                        let mut deps = IndexSet::new();
                        for arg in &decl.args {
                            self.arg_decl(arg, &mut deps);
                        }
                        // The signature's checked type covers both the
                        // parameters and the return type.
                        self.ty(&decl.metadata.2, &mut deps);
                        self.scope(&decl.scope, &mut deps);
                        (decl.metadata.1, deps)
                    }
                    // An enum's meaning is entirely its own variant list.
                    Decl::Enum(_)
                    | Decl::Struct(_)
                    | Decl::Constant(_)
                    | Decl::Mod(_)
                    | Decl::Use(_) => continue,
                };
                deps.shift_remove(&var);
                deps.retain(|dep| self.items.contains_key(dep));
                if let Some(item) = self.items.get_mut(&var) {
                    item.deps = deps;
                }
            }
        }
    }

    /// A parameter's dependencies come from its *checked* type, not its
    /// written one: a `TySpec`'s identifier carries no metadata, so the
    /// declaration it names is only recoverable from `ArgDecl`'s `Ty`.
    fn arg_decl(&self, arg: &ArgDecl<arcstr::Substr, VarIdTyMetadata>, out: &mut IndexSet<VarId>) {
        self.ty(&arg.metadata.1, out);
    }

    /// Collects the declarations a checked type refers to.
    fn ty(&self, ty: &Ty, out: &mut IndexSet<VarId>) {
        match ty {
            Ty::Enum(enum_ty) => {
                if let Some(var) = self.enums.get(&enum_ty.id) {
                    out.insert(*var);
                }
            }
            // Stop at the declaring cell. `CellTy::data` is an `Arc`-shared DAG
            // of every field's type, including instantiated sub-cells, so
            // descending it is exponential in hierarchy depth -- the exact
            // blow-up `CellFnTy::cell`'s sharing exists to prevent. The
            // declaring cell is itself an item whose own fingerprint covers its
            // fields.
            Ty::Cell(cell) | Ty::Inst(cell) => {
                if let Some(def) = cell.def {
                    out.insert(def);
                }
            }
            Ty::CellFn(cell_fn) => {
                if let Some(def) = cell_fn.cell.def {
                    out.insert(def);
                }
            }
            Ty::Seq(inner) => self.ty(inner, out),
            Ty::Tuple(items) => {
                for item in items {
                    self.ty(item, out);
                }
            }
            Ty::Fn(fn_ty) => {
                for arg in &fn_ty.args {
                    self.ty(arg, out);
                }
                self.ty(&fn_ty.ret, out);
            }
            Ty::Unknown
            | Ty::Any
            | Ty::Bool
            | Ty::Float
            | Ty::Int
            | Ty::Rect
            | Ty::Polygon
            | Ty::Path
            | Ty::Point
            | Ty::String
            | Ty::Nil
            | Ty::SeqNil => {}
        }
    }

    fn ident_path(
        &self,
        path: &IdentPath<arcstr::Substr, VarIdTyMetadata>,
        out: &mut IndexSet<VarId>,
    ) {
        if let Some(var) = path.metadata.0 {
            out.insert(var);
        }
        self.ty(&path.metadata.1, out);
    }

    fn scope(&self, scope: &Scope<arcstr::Substr, VarIdTyMetadata>, out: &mut IndexSet<VarId>) {
        self.ty(&scope.metadata, out);
        for stmt in &scope.stmts {
            match stmt {
                Statement::Expr { value, .. } => self.expr(value, out),
                Statement::LetBinding(binding) => self.expr(&binding.value, out),
                Statement::ForLoop(loop_) => {
                    self.expr(&loop_.seq, out);
                    self.scope(&loop_.body, out);
                }
            }
        }
        if let Some(tail) = &scope.tail {
            self.expr(tail, out);
        }
    }

    fn expr(&self, expr: &Expr<arcstr::Substr, VarIdTyMetadata>, out: &mut IndexSet<VarId>) {
        match expr {
            Expr::If(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.cond, out);
                self.scope(&e.then, out);
                self.scope(&e.else_, out);
            }
            Expr::Match(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.scrutinee, out);
                for arm in &e.arms {
                    // A match pattern is an enum variant, and the enum reaches
                    // us only through the pattern's checked type.
                    self.ident_path(&arm.pattern, out);
                    self.expr(&arm.expr, out);
                }
            }
            Expr::BinOp(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.left, out);
                self.expr(&e.right, out);
            }
            Expr::UnaryOp(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.operand, out);
            }
            Expr::Call(e) => {
                if let Some(var) = e.metadata.0 {
                    out.insert(var);
                }
                self.ty(&e.metadata.1, out);
                self.ident_path(&e.func, out);
                for arg in &e.args.posargs {
                    self.expr(arg, out);
                }
                for kwarg in &e.args.kwargs {
                    self.ty(&kwarg.metadata, out);
                    self.expr(&kwarg.value, out);
                }
            }
            Expr::Emit(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.value, out);
            }
            Expr::FieldAccess(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.base, out);
            }
            Expr::IndexFieldAccess(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.base, out);
            }
            Expr::Index(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.base, out);
                self.expr(&e.index, out);
            }
            Expr::IdentPath(e) => self.ident_path(e, out),
            Expr::Scope(e) => self.scope(e, out),
            Expr::Cast(e) => {
                self.ty(&e.metadata, out);
                self.expr(&e.value, out);
            }
            Expr::Tuple(e) => {
                self.ty(&e.metadata, out);
                for item in &e.items {
                    self.expr(item, out);
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

    /// Condenses the reference graph and folds each component's dependencies
    /// into its members' fingerprints.
    fn finish(self) -> IndexMap<VarId, ItemSite> {
        let order = self.items.keys().copied().collect::<Vec<_>>();
        let components = strongly_connected(&order, |var| {
            self.items.get(&var).map(|item| item.deps.iter().copied())
        });

        // Tarjan emits components in reverse topological order, so every
        // dependency outside a component already has a fingerprint by the time
        // the component is folded.
        let mut component_of = HashMap::new();
        let mut component_fp: Vec<Fingerprint> = Vec::with_capacity(components.len());
        for (index, members) in components.iter().enumerate() {
            for member in members {
                component_of.insert(*member, index);
            }
        }

        let mut sites = IndexMap::with_capacity(self.items.len());
        for (index, members) in components.iter().enumerate() {
            let mut self_hashes = members
                .iter()
                .map(|member| self.items[member].self_hash)
                .collect::<Vec<_>>();
            self_hashes.sort_unstable();

            let mut external = members
                .iter()
                .flat_map(|member| self.items[member].deps.iter())
                .filter(|dep| component_of.get(*dep) != Some(&index))
                .filter_map(|dep| component_of.get(dep).map(|other| component_fp[*other]))
                .collect::<Vec<_>>();
            // Sorted and deduplicated so the digest does not depend on
            // traversal order or on a dependency being named twice.
            external.sort_unstable();
            external.dedup();

            let mut scc_hasher = hasher();
            scc_hasher.write_usize(self_hashes.len());
            for hash in &self_hashes {
                scc_hasher.write_u64(*hash);
            }
            scc_hasher.write_usize(external.len());
            for fingerprint in &external {
                scc_hasher.write_u64(*fingerprint);
            }
            let scc = scc_hasher.finish();
            component_fp.push(scc);

            for member in members {
                let item = &self.items[member];
                // Mixed with the member's own hash so that two mutually
                // recursive declarations, which share a component and so share
                // `scc`, still fingerprint differently.
                let mut member_hasher = hasher();
                member_hasher.write_u64(scc);
                member_hasher.write_u64(item.self_hash);
                sites.insert(
                    *member,
                    ItemSite {
                        fingerprint: member_hasher.finish(),
                        kind: item.kind,
                        path: item.path.clone(),
                        span: item.span.clone(),
                    },
                );
            }
        }
        sites
    }
}

/// The declarations a module actually contains.
///
/// Generated declarations -- the entry cell a compiler invocation splices in --
/// sit at the front of the list and are skipped: they are not part of the
/// workspace, and their text changes with every invocation.
fn declarations(
    annotated: &AnnotatedAst<VarIdTyMetadata>,
) -> impl Iterator<Item = &Decl<arcstr::Substr, VarIdTyMetadata>> {
    annotated
        .ast
        .decls
        .iter()
        .skip(annotated.generated_declarations)
}

fn span_range(span: cfgrammar::Span) -> Range<usize> {
    span.start()..span.end()
}

/// Iterative Tarjan, returning components in reverse topological order.
///
/// Iterative rather than recursive because the declaration graph is as large as
/// the workspace, and this runs from analyzer paths that do not have the
/// compiler's dedicated large stack.
fn strongly_connected<I>(
    nodes: &[VarId],
    successors: impl Fn(VarId) -> Option<I>,
) -> Vec<Vec<VarId>>
where
    I: Iterator<Item = VarId>,
{
    #[derive(Clone, Copy)]
    struct Visit {
        index: u32,
        lowlink: u32,
        on_stack: bool,
    }

    let mut state: HashMap<VarId, Visit> = HashMap::with_capacity(nodes.len());
    let mut stack: Vec<VarId> = Vec::new();
    let mut components = Vec::new();
    let mut next_index = 0_u32;

    for &root in nodes {
        if state.contains_key(&root) {
            continue;
        }
        // Each frame keeps the node's remaining successors, so the walk
        // resumes where it left off rather than restarting.
        let mut frames: Vec<(VarId, Vec<VarId>, usize)> = vec![(
            root,
            successors(root).map(Iterator::collect).unwrap_or_default(),
            0,
        )];
        state.insert(
            root,
            Visit {
                index: next_index,
                lowlink: next_index,
                on_stack: true,
            },
        );
        next_index += 1;
        stack.push(root);

        while let Some((node, edges, cursor)) = frames.last_mut() {
            if *cursor < edges.len() {
                let next = edges[*cursor];
                *cursor += 1;
                match state.get(&next).copied() {
                    None => {
                        state.insert(
                            next,
                            Visit {
                                index: next_index,
                                lowlink: next_index,
                                on_stack: true,
                            },
                        );
                        next_index += 1;
                        stack.push(next);
                        frames.push((
                            next,
                            successors(next).map(Iterator::collect).unwrap_or_default(),
                            0,
                        ));
                    }
                    Some(visit) if visit.on_stack => {
                        let node = *node;
                        let low = state[&node].lowlink.min(visit.index);
                        state.get_mut(&node).expect("visited").lowlink = low;
                    }
                    Some(_) => {}
                }
                continue;
            }

            let node = *node;
            let visit = state[&node];
            if visit.lowlink == visit.index {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    state.get_mut(&member).expect("on stack").on_stack = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                component.sort_unstable();
                components.push(component);
            }
            frames.pop();
            if let Some((parent, _, _)) = frames.last() {
                let parent = *parent;
                let low = state[&parent].lowlink.min(visit.lowlink);
                state.get_mut(&parent).expect("visited").lowlink = low;
            }
        }
    }
    components
}

impl ItemIndex {
    /// Declaration extents in one file, ordered by position.
    ///
    /// Extents never overlap or nest within a file, so an offset identifies at
    /// most one declaration.
    pub fn sites_in(&self, path: &Path) -> Vec<&ItemSite> {
        let mut sites = self
            .sites
            .values()
            .filter(|site| site.path == path)
            .collect::<Vec<_>>();
        sites.sort_by_key(|site| site.span.start);
        sites
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compile::static_compile,
        parse::{STD_PATH, STD_SOURCE, parse_source_text},
    };

    /// Fingerprints for one virtual workspace, keyed by declaration name so a
    /// test can talk about `shapes` rather than about whatever `VarId` it drew.
    fn fingerprints(source: &str) -> HashMap<String, Fingerprint> {
        let root = parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        let std = parse_source_text(STD_SOURCE, PathBuf::from(STD_PATH)).unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (typed, output) = static_compile(&ast).expect("a root module");
        assert!(output.errors.is_empty(), "{:?}", output.errors);

        let index = ItemIndex::build(&typed);
        let mut named = HashMap::new();
        for annotated in typed.values() {
            if annotated.path != Path::new("/virtual/lib.ar") {
                continue;
            }
            for decl in declarations(annotated) {
                let (var, name) = match decl {
                    Decl::Cell(decl) => (decl.metadata.1, decl.name.name.to_string()),
                    Decl::Fn(decl) => (decl.metadata.1, decl.name.name.to_string()),
                    Decl::Enum(decl) => match decl.metadata {
                        Some((var, _)) => (var, decl.name.name.to_string()),
                        None => continue,
                    },
                    _ => continue,
                };
                named.insert(name, index.fingerprint(var).expect("every item is indexed"));
            }
        }
        named
    }

    /// Reports which declarations changed fingerprint between two revisions.
    fn changed(before: &str, after: &str) -> Vec<String> {
        let (a, b) = (fingerprints(before), fingerprints(after));
        let mut names = a
            .iter()
            .filter(|(name, fp)| b.get(*name).is_some_and(|other| other != *fp))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    const BASE: &str = "\
fn helper() -> Float { 1. }
fn unrelated() -> Float { 2. }
fn middle() -> Float { helper() }
cell uses_middle() { let r = rect(\"met1\", x0 = middle(), y0 = 0., x1 = 1., y1 = 1.); }
cell uses_unrelated() { let r = rect(\"met1\", x0 = unrelated(), y0 = 0., x1 = 1., y1 = 1.); }
";

    #[test]
    fn an_unchanged_workspace_fingerprints_identically() {
        assert_eq!(fingerprints(BASE), fingerprints(BASE));
    }

    #[test]
    fn changing_a_fn_invalidates_only_its_callers() {
        // `middle` calls `helper`, and `uses_middle` calls `middle`; nothing
        // reaches `unrelated` or `uses_unrelated`.
        assert_eq!(
            changed(
                BASE,
                &BASE.replace("fn helper() -> Float { 1. }", "fn helper() -> Float { 9. }")
            ),
            ["helper", "middle", "uses_middle"],
            "a changed fn must invalidate its transitive callers and nothing else"
        );
    }

    #[test]
    fn changing_a_leaf_cell_invalidates_nothing_else() {
        assert_eq!(
            changed(
                BASE,
                &BASE.replace(
                    "x1 = 1., y1 = 1.); }\ncell uses_unrelated",
                    "x1 = 7., y1 = 1.); }\ncell uses_unrelated"
                )
            ),
            ["uses_middle"]
        );
    }

    /// Text that moves without changing keeps its fingerprint. This is what
    /// lets an edit reuse declarations that merely shifted down the file.
    #[test]
    fn inserting_a_declaration_above_does_not_disturb_the_others() {
        let after = format!("fn inserted() -> Float {{ 3. }}\n{BASE}");
        assert_eq!(changed(BASE, &after), Vec::<String>::new());
    }

    #[test]
    fn a_recursive_fn_is_fingerprinted_and_invalidates_its_callers() {
        let base = "\
fn countdown(n: Int) -> Int { if n <= 0 { 0 } else { countdown(n - 1) } }
fn other(n: Int) -> Int { n }
cell top() { let r = rect(\"met1\", x0 = countdown(3) as Float, y0 = 0., x1 = 1., y1 = 1.); }
";
        // Self-recursion must terminate, and must not make everything equal.
        let fps = fingerprints(base);
        assert_ne!(fps["countdown"], fps["other"]);
        assert_eq!(
            changed(base, &base.replace("{ n }", "{ n + 1 }")),
            ["other"],
            "changing an unrelated fn must not touch the recursive one"
        );
        assert_eq!(
            changed(base, &base.replace("countdown(n - 1)", "countdown(n - 2)")),
            ["countdown", "top"]
        );
    }

    #[test]
    fn mutually_recursive_fns_are_distinct_and_invalidate_together() {
        let base = "\
fn ping(n: Int) -> Int { if n <= 0 { 0 } else { pong(n - 1) } }
fn pong(n: Int) -> Int { if n <= 0 { 1 } else { ping(n - 1) } }
fn lonely(n: Int) -> Int { n }
";
        let fps = fingerprints(base);
        assert_ne!(
            fps["ping"], fps["pong"],
            "two members of one component must not share a fingerprint"
        );
        // They are one component, so a change to either moves both.
        assert_eq!(
            changed(base, &base.replace("{ 1 }", "{ 2 }")),
            ["ping", "pong"]
        );
    }

    #[test]
    fn adding_an_enum_variant_invalidates_its_users() {
        let base = "\
enum Mode { Fast, Slow, }
fn pick(m: Mode) -> Float { match m { Mode::Fast => 1., Mode::Slow => 2., } }
fn untouched() -> Float { 5. }
";
        let after = "\
enum Mode { Fast, Slow, Medium, }
fn pick(m: Mode) -> Float { match m { Mode::Fast => 1., Mode::Slow => 2., Mode::Medium => 3., } }
fn untouched() -> Float { 5. }
";
        assert_eq!(changed(base, after), ["Mode", "pick"]);
    }

    /// An enum used only as a parameter type, never matched on. The reference
    /// carries no `VarId`, so this only works through the `EnumId` map.
    #[test]
    fn an_enum_named_only_as_a_type_is_still_a_dependency() {
        let base = "\
enum Mode { Fast, Slow, }
fn takes(m: Mode) -> Float { 1. }
";
        let after = "\
enum Mode { Fast, Slow, Medium, }
fn takes(m: Mode) -> Float { 1. }
";
        assert_eq!(changed(base, after), ["Mode", "takes"]);
    }

    #[test]
    fn renaming_a_declaration_changes_its_fingerprint() {
        let renamed = BASE.replace("unrelated", "renamed");
        let (a, b) = (fingerprints(BASE), fingerprints(&renamed));
        assert!(b.contains_key("renamed"));
        assert_ne!(a["unrelated"], b["renamed"], "the name is part of identity");
    }

    /// Two declarations with byte-identical bodies in different modules must
    /// not share a fingerprint, since `CompiledCell::name` becomes the exported
    /// GDS structure name.
    #[test]
    fn identical_bodies_in_different_modules_differ() {
        let root = parse_source_text(
            "mod other;\nfn twin() -> Float { 1. }",
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let other = parse_source_text(
            "fn twin() -> Float { 1. }",
            PathBuf::from("/virtual/other.ar"),
        )
        .unwrap();
        let std = parse_source_text(STD_SOURCE, PathBuf::from(STD_PATH)).unwrap();
        let ast = IndexMap::from([
            (Vec::new(), root),
            (vec!["other".to_owned()], other),
            (vec!["std".to_owned()], std),
        ]);
        let (typed, output) = static_compile(&ast).expect("a root module");
        assert!(output.errors.is_empty(), "{:?}", output.errors);
        let index = ItemIndex::build(&typed);
        let mut fingerprints = index
            .iter()
            .map(|(_, site)| site.fingerprint)
            .collect::<Vec<_>>();
        let total = fingerprints.len();
        fingerprints.sort_unstable();
        fingerprints.dedup();
        assert_eq!(total, fingerprints.len(), "fingerprints must be distinct");
    }

    /// Every example in the corpus indexes without panicking or diverging, and
    /// yields extents that are in bounds, disjoint, and distinctly
    /// fingerprinted.
    #[test]
    fn corpus_fingerprints_are_well_formed() {
        use crate::parse::parse_workspace_with_std;

        let examples = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples"));
        let mut checked = 0;
        for entry in std::fs::read_dir(&examples).unwrap() {
            let root = entry.unwrap().path().join("lib.ar");
            if !root.is_file() {
                continue;
            }
            let parsed = parse_workspace_with_std(&root).ast();
            let Some((typed, output)) = static_compile(&parsed) else {
                continue;
            };
            if typed.is_empty() || !output.errors.is_empty() {
                continue;
            }
            checked += 1;
            let index = ItemIndex::build(&typed);

            let mut seen: HashMap<Fingerprint, VarId> = HashMap::new();
            for (var, site) in index.iter() {
                if let Some(other) = seen.insert(site.fingerprint, var) {
                    panic!(
                        "{}: {var:?} and {other:?} share a fingerprint",
                        root.display()
                    );
                }
            }

            let visible: IndexMap<&Path, usize> = typed
                .values()
                .map(|module| (module.path.as_path(), module.source_text.len()))
                .collect();
            for module in typed.values() {
                let sites = index.sites_in(&module.path);
                let limit = visible[module.path.as_path()];
                for pair in sites.windows(2) {
                    assert!(
                        pair[0].span.end <= pair[1].span.start,
                        "{}: extents overlap",
                        module.path.display()
                    );
                }
                for site in sites {
                    assert!(
                        site.span.end <= limit,
                        "{}: {:?} is past the editor-visible source",
                        module.path.display(),
                        site.span
                    );
                }
            }
        }
        assert!(checked > 20, "expected a corpus, indexed {checked}");
    }

    /// Declaration extents never overlap, which is what lets a recorded span be
    /// matched to exactly one declaration by binary search.
    #[test]
    fn declaration_extents_are_disjoint_and_ordered() {
        let root = parse_source_text(BASE, PathBuf::from("/virtual/lib.ar")).unwrap();
        let std = parse_source_text(STD_SOURCE, PathBuf::from(STD_PATH)).unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (typed, _) = static_compile(&ast).expect("a root module");
        let index = ItemIndex::build(&typed);
        let sites = index.sites_in(Path::new("/virtual/lib.ar"));
        assert_eq!(sites.len(), 5);
        for pair in sites.windows(2) {
            assert!(
                pair[0].span.end <= pair[1].span.start,
                "extents overlap: {:?} then {:?}",
                pair[0].span,
                pair[1].span
            );
        }
    }
}

/// Translates spans recorded against one revision of a workspace onto another.
///
/// A compiled cell is only reused when every declaration it can reach has the
/// same content, so the text each of its spans points into is byte-identical
/// and all that can differ is where in the file that text begins. Locating the
/// declaration a span falls inside and shifting by the difference in start
/// offsets is therefore exact.
pub struct SpanRebase {
    /// Declaration extents in the revision the spans were recorded against,
    /// per file and ordered by position, with the fingerprint that names each.
    from: HashMap<PathBuf, Vec<(Range<usize>, Fingerprint)>>,
    /// Where each of those declarations lives now.
    to: HashMap<Fingerprint, (PathBuf, Range<usize>)>,
}

/// A span that could not be translated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseError {
    /// The span fell inside a declaration that is no longer present, which
    /// means the reused cell's closure was computed wrongly.
    UnknownDeclaration,
    /// The declaration is present but its extent changed length, so the text
    /// the span points into is not the text it was recorded against: two
    /// different declarations hashed alike.
    LengthChanged,
}

impl SpanRebase {
    /// A translation from `from` to `to`, or `None` when no declaration moved
    /// and every span is already correct.
    pub fn new(from: &ItemIndex, to: &ItemIndex) -> Option<Self> {
        let mut moved = false;
        let mut by_fingerprint = HashMap::with_capacity(to.len());
        for (_, site) in to.iter() {
            by_fingerprint.insert(site.fingerprint, (site.path.clone(), site.span.clone()));
        }
        let mut by_path: HashMap<PathBuf, Vec<(Range<usize>, Fingerprint)>> = HashMap::new();
        for (_, site) in from.iter() {
            match by_fingerprint.get(&site.fingerprint) {
                Some((path, span)) if path == &site.path && span == &site.span => {}
                _ => moved = true,
            }
            by_path
                .entry(site.path.clone())
                .or_default()
                .push((site.span.clone(), site.fingerprint));
        }
        if !moved {
            return None;
        }
        for extents in by_path.values_mut() {
            extents.sort_by_key(|(span, _)| span.start);
        }
        Some(Self {
            from: by_path,
            to: by_fingerprint,
        })
    }

    /// Translates one span in place.
    ///
    /// A span in a file with no declarations at all is left alone: that is the
    /// GDS-import case, whose spans name a `.gds` file at offset `0..0`.
    pub fn rebase(&self, span: &mut crate::ast::Span) -> Result<(), RebaseError> {
        let Some(extents) = self.from.get(&span.path) else {
            return Ok(());
        };
        let start = span.span.start();
        // Extents within a file never overlap or nest, so the last one
        // beginning at or before `start` is the only candidate.
        let index = extents.partition_point(|(extent, _)| extent.start <= start);
        let Some((extent, fingerprint)) = index.checked_sub(1).map(|index| &extents[index]) else {
            return Ok(());
        };
        if start >= extent.end {
            // Between declarations: whitespace, a comment, or a `use`. Nothing
            // the executor records a span for lives here.
            return Ok(());
        }
        let Some((path, moved)) = self.to.get(fingerprint) else {
            return Err(RebaseError::UnknownDeclaration);
        };
        if moved.end - moved.start != extent.end - extent.start {
            return Err(RebaseError::LengthChanged);
        }
        let shift = moved.start as isize - extent.start as isize;
        let shifted = |offset: usize| (offset as isize + shift) as usize;
        span.path = path.clone();
        span.span = cfgrammar::Span::new(shifted(span.span.start()), shifted(span.span.end()));
        Ok(())
    }

    /// Translates an optional span, leaving `None` alone.
    pub fn rebase_opt(&self, span: &mut Option<crate::ast::Span>) -> Result<(), RebaseError> {
        match span {
            Some(span) => self.rebase(span),
            None => Ok(()),
        }
    }
}
