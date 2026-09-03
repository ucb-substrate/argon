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

use arcstr::ArcStr;

use crate::{
    ast::{
        ArgDecl, CellDecl, Decl, EnumDecl, Expr, FnDecl, Ident, IdentPath, ModPath, Scope,
        Statement, TySpec, TySpecKind, UseDecl, WorkspaceAst,
    },
    compile::{BUILTINS, EnumId, RESERVED_CELL_FIELDS, Ty, VarId, VarIdTyMetadata, module_prefix},
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

/// Kind of an editor completion candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Function,
    Cell,
    Variable,
    Parameter,
    Enum,
    Variant,
    Module,
    Field,
    Type,
    Keyword,
}

/// Protocol-independent completion data produced by the typed index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
    /// Text to insert when it differs from `label` (notably keyword args).
    pub insert_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterInfo {
    pub label: String,
    pub keyword: bool,
}

/// A callable signature, shared by signature help and completion details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    pub label: String,
    pub parameters: Vec<ParameterInfo>,
}

/// Quick information for the token or expression under a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    pub span: cfgrammar::Span,
    pub contents: String,
}

/// One source symbol suitable for a document outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineSymbol {
    pub name: String,
    pub detail: String,
    pub kind: SymbolKind,
    pub range: cfgrammar::Span,
    pub selection_range: cfgrammar::Span,
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
    detail: String,
    ty: Option<Ty>,
    signature: Option<SignatureInfo>,
    full_span: Option<crate::ast::Span>,
}

#[derive(Debug, Clone)]
struct ScopeBinding {
    name: String,
    key: DefKey,
    scope: cfgrammar::Span,
    available_after: usize,
}

#[derive(Debug, Clone)]
struct ModuleItem {
    name: String,
    key: DefKey,
    available_after: usize,
}

struct DefinitionInfo {
    detail: String,
    ty: Option<Ty>,
    signature: Option<SignatureInfo>,
    full_span: Option<cfgrammar::Span>,
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
    /// Editor-visible source of every file that parsed, including files that
    /// contributed no identifier.
    ///
    /// Every span in the index is a byte offset into the text recorded here,
    /// and an index outlives the sources it was built from: it keeps answering
    /// while a half-written edit does not compile. Carrying the text makes the
    /// index self-describing, so a reader can align an offset against the
    /// buffer an editor has now instead of assuming the two still agree.
    sources: HashMap<PathBuf, ArcStr>,
    defs: HashMap<DefKey, Definition>,
    usages: HashMap<DefKey, Vec<crate::ast::Span>>,
    file_modules: HashMap<PathBuf, ModPath>,
    module_items: HashMap<ModPath, Vec<ModuleItem>>,
    scope_bindings: HashMap<PathBuf, Vec<ScopeBinding>>,
    expression_types: HashMap<PathBuf, Vec<(cfgrammar::Span, Ty)>>,
    hover_details: HashMap<PathBuf, Vec<(cfgrammar::Span, String)>>,
}

impl NavIndex {
    pub fn build(ast: &WorkspaceAst<VarIdTyMetadata>) -> Self {
        Builder::new(ast).run()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// The files this index covers.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.sources.keys().map(PathBuf::as_path)
    }

    /// The text this index's offsets into `file` are relative to.
    pub fn source(&self, file: &Path) -> Option<&ArcStr> {
        self.sources.get(file)
    }

    /// The files an index built from `ast` would cover.
    ///
    /// Answered without building the index, so one that is about to be judged
    /// unusable is never built. Kept in step with the `sources` insert in
    /// the builder's walk, which is the definition this reproduces.
    pub fn coverage(ast: &WorkspaceAst<VarIdTyMetadata>) -> HashSet<&Path> {
        ast.values()
            .filter(|module| indexes_source(module))
            .map(|module| module.path.as_path())
            .collect()
    }

    /// Whether `coverage` still covers every file this index does.
    ///
    /// A file drops out of the index when it stops parsing, because the parser
    /// reports a failure rather than a partial tree and the module is left
    /// empty. That is a transient state while someone is typing, so it is a
    /// signal to keep serving the previous index rather than to replace it.
    /// `tracked` is the set of files the workspace still contains, so that a
    /// file which was genuinely removed does not pin the index forever.
    ///
    /// Coverage is per file walked, not per file that yielded an identifier: a
    /// file whose every declaration is commented out still parses, and must
    /// not be mistaken for one that stopped parsing.
    pub fn covered_by(&self, coverage: &HashSet<&Path>, tracked: &HashSet<&Path>) -> bool {
        self.files()
            .all(|path| coverage.contains(path) || !tracked.contains(path))
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

    fn completion_for(&self, name: &str, key: &DefKey) -> Option<CompletionCandidate> {
        let definition = self.defs.get(key)?;
        Some(CompletionCandidate {
            label: name.to_owned(),
            kind: completion_kind(definition.kind),
            detail: Some(definition.detail.clone()),
            insert_text: None,
        })
    }

    fn module_completions(
        &self,
        module: &ModPath,
        offset: Option<usize>,
    ) -> Vec<CompletionCandidate> {
        self.module_items
            .get(module)
            .into_iter()
            .flatten()
            .filter(|item| offset.is_none_or(|offset| item.available_after <= offset))
            .filter_map(|item| self.completion_for(&item.name, &item.key))
            .collect()
    }

    /// Names visible at `offset`, with inner lexical bindings shadowing outer
    /// bindings and module items.
    pub fn completions_at(&self, file: &Path, offset: usize) -> Vec<CompletionCandidate> {
        // Language-level names do not depend on a successful parse or type
        // check. Start with them so completion remains useful while a new or
        // half-written file has no entry in the typed index yet.
        let static_candidates = Self::static_completions();
        let Some(module) = self.file_modules.get(file) else {
            return static_candidates;
        };
        let mut candidates: HashMap<String, CompletionCandidate> = static_candidates
            .into_iter()
            .map(|candidate| (candidate.label.clone(), candidate))
            .collect();
        for candidate in self.module_completions(module, Some(offset)) {
            candidates.insert(candidate.label.clone(), candidate);
        }
        for (label, key) in [
            ("lib", DefKey::Module(Vec::new())),
            ("std", DefKey::Module(vec!["std".to_owned()])),
        ] {
            if let Some(candidate) = self.completion_for(label, &key) {
                candidates.insert(label.to_owned(), candidate);
            }
        }

        let mut active = self
            .scope_bindings
            .get(file)
            .into_iter()
            .flatten()
            .filter(|binding| {
                binding.scope.start() <= offset
                    && offset <= binding.scope.end()
                    && binding.available_after <= offset
            })
            .collect::<Vec<_>>();
        // Outer scopes first so a same-named binding in the innermost scope
        // wins when inserted into the map.
        active
            .sort_by_key(|binding| std::cmp::Reverse(binding.scope.end() - binding.scope.start()));
        for binding in active {
            if let Some(candidate) = self.completion_for(&binding.name, &binding.key) {
                candidates.insert(candidate.label.clone(), candidate);
            }
        }

        let mut candidates = candidates.into_values().collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.label.cmp(&right.label));
        candidates
    }

    /// Builtins, primitive types, and keywords that can be completed without
    /// consulting a parsed or type-checked workspace.
    pub fn static_completions() -> Vec<CompletionCandidate> {
        let mut candidates =
            Vec::with_capacity(BUILTINS.len() + PRIMITIVE_TYPES.len() + KEYWORDS.len());
        candidates.extend(BUILTINS.into_iter().map(|name| {
            let signature = builtin_signature(name).expect("every builtin has editor metadata");
            CompletionCandidate {
                label: name.to_owned(),
                kind: CompletionKind::Function,
                detail: Some(signature.label),
                insert_text: None,
            }
        }));
        candidates.extend(PRIMITIVE_TYPES.into_iter().map(|name| CompletionCandidate {
            label: name.to_owned(),
            kind: CompletionKind::Type,
            detail: Some(format!("builtin type {name}")),
            insert_text: None,
        }));
        candidates.extend(KEYWORDS.into_iter().map(|keyword| CompletionCandidate {
            label: keyword.to_owned(),
            kind: CompletionKind::Keyword,
            detail: None,
            insert_text: None,
        }));
        candidates.sort_by(|left, right| left.label.cmp(&right.label));
        candidates
    }

    /// Members following a `.` after the expression ending at `offset`.
    pub fn member_completions_at(&self, file: &Path, offset: usize) -> Vec<CompletionCandidate> {
        let from_definition = self
            .target_at(file, offset)
            .and_then(|(_, target)| match target {
                Target::Def(key) => self.defs.get(key)?.ty.as_ref(),
                _ => None,
            });
        let ty = from_definition.or_else(|| {
            self.expression_types
                .get(file)?
                .iter()
                .filter(|(span, _)| {
                    span.end() == offset || (span.start() <= offset && offset <= span.end())
                })
                .min_by_key(|(span, _)| {
                    (usize::from(span.end() != offset), span.end() - span.start())
                })
                .map(|(_, ty)| ty)
        });
        ty.map(field_completions).unwrap_or_default()
    }

    /// Items exported by a module, or variants of an enum, named by a path
    /// immediately before `::`.
    pub fn qualified_completions(
        &self,
        file: &Path,
        qualifier: &[String],
    ) -> Vec<CompletionCandidate> {
        let Some(current) = self.file_modules.get(file) else {
            return Vec::new();
        };
        let module = module_prefix(current, qualifier.iter().map(String::as_str), 0);
        if self.module_items.contains_key(&module) {
            let mut result = self.module_completions(&module, None);
            result.sort_by(|left, right| left.label.cmp(&right.label));
            return result;
        }

        let Some((enum_name, prefix)) = qualifier.split_last() else {
            return Vec::new();
        };
        let module = if prefix.is_empty() {
            current.clone()
        } else {
            module_prefix(current, prefix.iter().map(String::as_str), 0)
        };
        let Some(enum_key) = self
            .module_items
            .get(&module)
            .into_iter()
            .flatten()
            .find(|item| item.name == *enum_name)
            .map(|item| &item.key)
        else {
            return Vec::new();
        };
        let DefKey::Var(enum_id) = enum_key else {
            return Vec::new();
        };
        let mut result = self
            .defs
            .iter()
            .filter_map(|(key, definition)| match key {
                DefKey::Variant(id, name) if id == enum_id => {
                    self.completion_for(name, key).or_else(|| {
                        Some(CompletionCandidate {
                            label: name.clone(),
                            kind: CompletionKind::Variant,
                            detail: Some(definition.detail.clone()),
                            insert_text: None,
                        })
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        result.sort_by(|left, right| left.label.cmp(&right.label));
        result
    }

    pub fn hover_at(&self, file: &Path, offset: usize) -> Option<HoverInfo> {
        if let Some((span, target)) = self.target_at(file, offset) {
            let contents = match target {
                Target::Def(key) => self
                    .defs
                    .get(key)
                    .map(|definition| definition.detail.clone()),
                Target::Builtin(Builtin::Function(name)) => {
                    builtin_signature(name).map(|signature| signature.label)
                }
                Target::Builtin(Builtin::Type(name)) => Some(format!("builtin type {name}")),
                Target::Builtin(Builtin::Field(_)) | Target::Builtin(Builtin::KwArg(_)) => self
                    .hover_details
                    .get(file)
                    .into_iter()
                    .flatten()
                    .find(|(candidate, _)| *candidate == span)
                    .map(|(_, detail)| detail.clone()),
                Target::Unresolved => None,
            };
            if let Some(contents) = contents {
                return Some(HoverInfo { span, contents });
            }
        }

        self.expression_types
            .get(file)?
            .iter()
            .filter(|(span, _)| span.start() <= offset && offset <= span.end())
            .min_by_key(|(span, _)| span.end() - span.start())
            .map(|(span, ty)| HoverInfo {
                span: *span,
                contents: format!("expression: {ty}"),
            })
    }

    pub fn signature_at(&self, file: &Path, offset: usize) -> Option<SignatureInfo> {
        match self.target_at(file, offset)?.1 {
            Target::Def(key) => self.defs.get(key)?.signature.clone(),
            Target::Builtin(Builtin::Function(name)) => builtin_signature(name),
            _ => None,
        }
    }

    /// Resolves a newly typed callee name when it is not present in the stale
    /// typed source yet.
    pub fn signature_named_at(
        &self,
        file: &Path,
        offset: usize,
        path: &[String],
    ) -> Option<SignatureInfo> {
        let (name, prefix) = path.split_last()?;
        if prefix.is_empty()
            && let Some(signature) = builtin_signature(name)
        {
            return Some(signature);
        }
        let module = if prefix.is_empty() {
            self.file_modules.get(file)?.clone()
        } else {
            module_prefix(
                self.file_modules.get(file)?,
                path.iter().map(String::as_str),
                1,
            )
        };
        self.module_items
            .get(&module)?
            .iter()
            .filter(|item| {
                item.name == *name && (!prefix.is_empty() || item.available_after <= offset)
            })
            .find_map(|item| self.defs.get(&item.key)?.signature.clone())
    }

    pub fn keyword_completions_for_signature(
        &self,
        signature: &SignatureInfo,
    ) -> Vec<CompletionCandidate> {
        signature
            .parameters
            .iter()
            .filter(|parameter| parameter.keyword)
            .map(|parameter| {
                let name = parameter
                    .label
                    .split_once(':')
                    .map_or(parameter.label.as_str(), |(name, _)| name)
                    .to_owned();
                CompletionCandidate {
                    label: name.clone(),
                    kind: CompletionKind::Parameter,
                    detail: Some(parameter.label.clone()),
                    insert_text: Some(format!("{name}=")),
                }
            })
            .collect()
    }

    pub fn document_symbols(&self, file: &Path) -> Vec<OutlineSymbol> {
        let mut symbols = self
            .defs
            .values()
            .filter_map(|definition| {
                let DefLocation::Source(selection) = &definition.location else {
                    return None;
                };
                (selection.path == file).then(|| OutlineSymbol {
                    name: definition.name.clone(),
                    detail: definition.detail.clone(),
                    kind: definition.kind,
                    range: definition
                        .full_span
                        .as_ref()
                        .filter(|span| span.path == file)
                        .map_or(selection.span, |span| span.span),
                    selection_range: selection.span,
                })
            })
            .collect::<Vec<_>>();
        symbols.sort_by_key(|symbol| (symbol.range.start(), symbol.selection_range.start()));
        symbols
    }
}

const PRIMITIVE_TYPES: [&str; 9] = [
    "Any", "Bool", "Float", "Int", "Path", "Point", "Polygon", "Rect", "String",
];

const KEYWORDS: [&str; 14] = [
    "as", "cell", "else", "enum", "false", "fn", "for", "if", "in", "let", "match", "mod", "true",
    "use",
];

fn completion_kind(kind: SymbolKind) -> CompletionKind {
    match kind {
        SymbolKind::Function => CompletionKind::Function,
        SymbolKind::Cell => CompletionKind::Cell,
        SymbolKind::Local | SymbolKind::LoopVar => CompletionKind::Variable,
        SymbolKind::Parameter => CompletionKind::Parameter,
        SymbolKind::Enum => CompletionKind::Enum,
        SymbolKind::Variant => CompletionKind::Variant,
        SymbolKind::Module => CompletionKind::Module,
    }
}

/// The primitive type `name` spells, read off the type checker's own table so
/// the two cannot disagree about which names resolve.
fn builtin_type(name: &str) -> Option<&'static str> {
    Ty::from_name(name)?.primitive_name()
}

/// The [`VarId`] of the cell a type was declared by, if it was declared by one.
fn declaring_cell(ty: &Ty) -> Option<VarId> {
    match ty {
        Ty::CellFn(cell_fn) => cell_fn.cell.def,
        Ty::Cell(cell) | Ty::Inst(cell) => cell.def,
        _ => None,
    }
}

fn builtin_function(name: &str) -> Option<&'static str> {
    BUILTINS.into_iter().find(|builtin| *builtin == name)
}

fn parameter(label: &str, keyword: bool) -> ParameterInfo {
    ParameterInfo {
        label: label.to_owned(),
        keyword,
    }
}

fn signature(label: &str, positional: &[&str], keywords: &[&str]) -> SignatureInfo {
    SignatureInfo {
        label: label.to_owned(),
        parameters: positional
            .iter()
            .map(|label| parameter(label, false))
            .chain(keywords.iter().map(|label| parameter(label, true)))
            .collect(),
    }
}

fn builtin_signature(name: &str) -> Option<SignatureInfo> {
    let rect_keywords = [
        "x0: Float",
        "x1: Float",
        "y0: Float",
        "y1: Float",
        "x0i: Float",
        "x1i: Float",
        "y0i: Float",
        "y1i: Float",
        "w: Float",
        "h: Float",
    ];
    let result = match name {
        "list" => signature("fn list(value: T, ...) -> [T]", &["value: T"], &[]),
        "cons" => signature(
            "fn cons(value: T, tail: [T]) -> [T]",
            &["value: T", "tail: [T]"],
            &[],
        ),
        "head" => signature("fn head(sequence: [T]) -> T", &["sequence: [T]"], &[]),
        "tail" => signature("fn tail(sequence: [T]) -> [T]", &["sequence: [T]"], &[]),
        "range_full" => signature(
            "fn range_full(start: Int, stop: Int, step: Int) -> [Int]",
            &["start: Int", "stop: Int", "step: Int"],
            &[],
        ),
        "crect" => {
            let mut keywords = rect_keywords.to_vec();
            keywords.push("layer: String");
            signature("fn crect(*, coordinates...) -> Rect", &[], &keywords)
        }
        "rect" => signature(
            "fn rect(layer: String, *, coordinates...) -> Rect",
            &["layer: String"],
            &rect_keywords,
        ),
        "polygon" => signature(
            "fn polygon(layer: String, points: Int, *, xN/yN: Float) -> Polygon",
            &["layer: String", "points: Int"],
            &[],
        ),
        "path" => signature(
            "fn path(layer: String, points: Int, *, coordinates...) -> Path",
            &["layer: String", "points: Int"],
            &[
                "width: Float",
                "widthi: Float",
                "begin_extension: Float",
                "begin_extensioni: Float",
                "end_extension: Float",
                "end_extensioni: Float",
            ],
        ),
        "text" => signature(
            "fn text(text: String, layer: String, x: Float, y: Float)",
            &["text: String", "layer: String", "x: Float", "y: Float"],
            &[],
        ),
        "float" => signature("fn float() -> Float", &[], &[]),
        "eq" => signature(
            "fn eq(left: Float, right: Float)",
            &["left: Float", "right: Float"],
            &[],
        ),
        "dimension" => signature(
            "fn dimension(x0: Float, y0: Float, x1: Float, y1: Float, offset: Float, label_offset: Float, flip: Bool)",
            &[
                "x0: Float",
                "y0: Float",
                "x1: Float",
                "y1: Float",
                "offset: Float",
                "label_offset: Float",
                "flip: Bool",
            ],
            &[],
        ),
        "inst" => signature(
            "fn inst(cell: Cell, *, placement...) -> Inst",
            &["cell: Cell"],
            &[
                "reflect: Bool",
                "angle: Int",
                "x: Float",
                "y: Float",
                "xi: Float",
                "yi: Float",
                "construction: Bool",
            ],
        ),
        "bbox" => signature(
            "fn bbox(value: Cell | Inst) -> Rect",
            &["value: Cell | Inst"],
            &[],
        ),
        _ => return None,
    };
    Some(result)
}

fn field_candidate(name: impl Into<String>, ty: &Ty) -> CompletionCandidate {
    let name = name.into();
    CompletionCandidate {
        label: name.clone(),
        kind: CompletionKind::Field,
        detail: Some(format!("field {name}: {ty}")),
        insert_text: None,
    }
}

fn field_completions(ty: &Ty) -> Vec<CompletionCandidate> {
    let fields: Vec<(String, Ty)> = match ty {
        Ty::Rect => [
            ("x0", Ty::Float),
            ("x1", Ty::Float),
            ("y0", Ty::Float),
            ("y1", Ty::Float),
            ("w", Ty::Float),
            ("h", Ty::Float),
            ("layer", Ty::String),
        ]
        .into_iter()
        .map(|(name, ty)| (name.to_owned(), ty))
        .collect(),
        Ty::Polygon => [
            ("points", Ty::Seq(Box::new(Ty::Point))),
            ("layer", Ty::String),
        ]
        .into_iter()
        .map(|(name, ty)| (name.to_owned(), ty))
        .collect(),
        Ty::Path => [
            ("points", Ty::Seq(Box::new(Ty::Point))),
            ("layer", Ty::String),
            ("width", Ty::Float),
            ("begin_extension", Ty::Float),
            ("end_extension", Ty::Float),
        ]
        .into_iter()
        .map(|(name, ty)| (name.to_owned(), ty))
        .collect(),
        Ty::Point => [("x", Ty::Float), ("y", Ty::Float)]
            .into_iter()
            .map(|(name, ty)| (name.to_owned(), ty))
            .collect(),
        Ty::Inst(cell) => RESERVED_CELL_FIELDS
            .into_iter()
            .map(|name| (name.to_owned(), Ty::Float))
            .chain(
                cell.data
                    .iter()
                    .map(|(name, ty)| (name.clone(), ty.clone())),
            )
            .collect(),
        Ty::Tuple(items) => items
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, ty)| (index.to_string(), ty))
            .collect(),
        _ => Vec::new(),
    };
    let mut fields = fields
        .into_iter()
        .map(|(name, ty)| field_candidate(name, &ty))
        .collect::<Vec<_>>();
    fields.sort_by(|left, right| left.label.cmp(&right.label));
    fields.dedup_by(|left, right| left.label == right.label);
    fields
}

fn declared_signature(
    keyword: &str,
    name: &str,
    args: &[ArgDecl<arcstr::Substr, VarIdTyMetadata>],
    return_ty: Option<&Ty>,
) -> SignatureInfo {
    let parameters = args
        .iter()
        .map(|arg| parameter(&format!("{}: {}", arg.name.name, arg.metadata.1), false))
        .collect::<Vec<_>>();
    let args = parameters
        .iter()
        .map(|parameter| parameter.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let return_ty = return_ty
        .filter(|ty| **ty != Ty::Nil)
        .map_or_else(String::new, |ty| format!(" -> {ty}"));
    SignatureInfo {
        label: format!("{keyword} {name}({args}){return_ty}"),
        parameters,
    }
}

struct Builder<'a> {
    ast: &'a WorkspaceAst<VarIdTyMetadata>,
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

/// Whether a module is real source rather than something the compiler
/// synthesized.
///
/// Two module paths name no Argon source file: the entry-cell splice
/// ([`crate::parse::CELL_PATH`]), which has no file at all, and a module made
/// up entirely of GDS imports, which is recorded against the binary `.gds`
/// file the cells came from. Neither is something an editor can usefully open,
/// so checking for the source extension covers both and any future
/// synthesized path.
fn is_navigable(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "ar")
}

/// Whether a module contributes its source text to an index.
///
/// A file that did not parse has an empty declaration list and no usable
/// offsets, so it is left out entirely rather than recorded as a file the
/// index covers.
fn indexes_source(module: &crate::ast::annotated::AnnotatedAst<VarIdTyMetadata>) -> bool {
    module.parsed && is_navigable(&module.path)
}

impl<'a> Builder<'a> {
    fn new(ast: &'a WorkspaceAst<VarIdTyMetadata>) -> Self {
        let mut builder = Self {
            ast,
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

    /// Records what the reference walk needs to have seen already: which
    /// `VarId` each enum's name holds, and which `let` declares each of a
    /// cell's fields. Both can be referred to from a module that is walked
    /// earlier.
    fn collect_declarations(&mut self) {
        for (module, ast) in self.ast.iter() {
            if !is_navigable(&ast.path) {
                continue;
            }
            if indexes_source(ast) {
                self.index
                    .file_modules
                    .insert(ast.path.clone(), module.clone());
            }
            self.index.defs.insert(
                DefKey::Module(module.clone()),
                Definition {
                    kind: SymbolKind::Module,
                    name: module.last().cloned().unwrap_or_default(),
                    location: DefLocation::File(ast.path.clone()),
                    detail: module
                        .last()
                        .map_or_else(|| "workspace root".to_owned(), |name| format!("mod {name}")),
                    ty: None,
                    signature: None,
                    full_span: None,
                },
            );
            if let Some((name, parent)) = module.split_last() {
                self.index
                    .module_items
                    .entry(parent.to_vec())
                    .or_default()
                    .push(ModuleItem {
                        name: name.clone(),
                        key: DefKey::Module(module.clone()),
                        available_after: 0,
                    });
            }
            for decl in &ast.ast.decls {
                match decl {
                    Decl::Enum(decl) => {
                        if let Some((name_id, enum_id)) = decl.metadata {
                            self.enums.insert(enum_id, name_id);
                            self.index
                                .module_items
                                .entry(module.clone())
                                .or_default()
                                .push(ModuleItem {
                                    name: decl.name.name.to_string(),
                                    key: DefKey::Var(name_id),
                                    available_after: 0,
                                });
                        }
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
                        self.index
                            .module_items
                            .entry(module.clone())
                            .or_default()
                            .push(ModuleItem {
                                name: decl.name.name.to_string(),
                                key: DefKey::Var(decl.metadata.1),
                                // Cells enter the compiler's binding frame
                                // only after their body is checked.
                                available_after: if decl.span.end() <= ast.source_text.len() {
                                    decl.span.end()
                                } else {
                                    // GDS declarations are generated before
                                    // user source and are always in scope.
                                    0
                                },
                            });
                    }
                    Decl::Fn(decl) => {
                        self.index
                            .module_items
                            .entry(module.clone())
                            .or_default()
                            .push(ModuleItem {
                                name: decl.name.name.to_string(),
                                key: DefKey::Var(decl.metadata.1),
                                available_after: 0,
                            });
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
            if indexes_source(ast) {
                self.index
                    .sources
                    .insert(ast.path.clone(), ast.source_text.clone());
            }
            for decl in &ast.ast.decls {
                self.decl(decl);
            }
        }
        for entries in self.index.refs.values_mut() {
            entries.sort_by_key(|(span, _)| (span.start(), std::cmp::Reverse(span.end())));
        }
        for entries in self.index.expression_types.values_mut() {
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

    fn record_hover_detail(&mut self, span: cfgrammar::Span, detail: String) {
        if self.visible(span) {
            self.index
                .hover_details
                .entry(self.path.to_path_buf())
                .or_default()
                .push((span, detail));
        }
    }

    fn define(
        &mut self,
        key: DefKey,
        kind: SymbolKind,
        name: &Ident<arcstr::Substr, VarIdTyMetadata>,
        info: DefinitionInfo,
    ) {
        let DefinitionInfo {
            detail,
            ty,
            signature,
            full_span,
        } = info;
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
                detail,
                ty,
                signature,
                full_span: full_span
                    .filter(|span| self.visible(*span))
                    .map(|span| self.span(span)),
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
        // A name the type pass rejected has no id, so there is nothing to key
        // a definition by and nothing that could refer to it.
        let Some((name_id, _)) = decl.metadata else {
            return;
        };
        let variants = decl
            .variants
            .iter()
            .map(|variant| variant.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        self.define(
            DefKey::Var(name_id),
            SymbolKind::Enum,
            &decl.name,
            DefinitionInfo {
                detail: format!("enum {} {{ {variants} }}", decl.name.name),
                ty: None,
                signature: None,
                full_span: None,
            },
        );
        for variant in &decl.variants {
            self.define(
                DefKey::Variant(name_id, variant.name.to_string()),
                SymbolKind::Variant,
                variant,
                DefinitionInfo {
                    detail: format!("{}::{}", decl.name.name, variant.name),
                    ty: None,
                    signature: None,
                    full_span: None,
                },
            );
        }
    }

    fn cell_decl(&mut self, decl: &'a CellDecl<arcstr::Substr, VarIdTyMetadata>) {
        let signature = declared_signature("cell", &decl.name.name, &decl.args, None);
        self.define(
            DefKey::Var(decl.metadata.1),
            SymbolKind::Cell,
            &decl.name,
            DefinitionInfo {
                detail: signature.label.clone(),
                ty: None,
                signature: Some(signature),
                full_span: Some(decl.span),
            },
        );
        for arg in &decl.args {
            self.arg_decl(arg, decl.scope.span);
        }
        self.scope(&decl.scope);
    }

    fn fn_decl(&mut self, decl: &'a FnDecl<arcstr::Substr, VarIdTyMetadata>) {
        let return_ty = match &decl.metadata.2 {
            Ty::Fn(fn_ty) => Some(&fn_ty.ret),
            _ => None,
        };
        let signature = declared_signature("fn", &decl.name.name, &decl.args, return_ty);
        self.define(
            DefKey::Var(decl.metadata.1),
            SymbolKind::Function,
            &decl.name,
            DefinitionInfo {
                detail: signature.label.clone(),
                ty: Some(decl.metadata.2.clone()),
                signature: Some(signature),
                full_span: Some(decl.span),
            },
        );
        for arg in &decl.args {
            self.arg_decl(arg, decl.scope.span);
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
        let module = module_prefix(
            self.current,
            decl.path.iter().map(|ident| ident.name.as_str()),
            1,
        );
        let target = self
            .exported(&module, &item.name)
            .map_or(Target::Unresolved, |id| Target::Def(DefKey::Var(id)));
        self.record(item.span, target.clone());
        // An alias is another name for the same binding, not a new definition,
        // so navigating from it lands on the original declaration.
        if let Some(alias) = &decl.alias {
            self.record(alias.span, target.clone());
        }
        if let Target::Def(key) = target {
            self.index
                .module_items
                .entry(self.current.clone())
                .or_default()
                .push(ModuleItem {
                    name: decl.alias.as_ref().unwrap_or(item).name.to_string(),
                    key,
                    available_after: 0,
                });
        }
    }

    /// The `VarId` a module exports under `name`, if any.
    fn exported(&self, module: &ModPath, name: &str) -> Option<VarId> {
        self.exported_within(module, name, 0)
    }

    /// [`Self::exported`], following the module's own imports.
    ///
    /// A module's exports are its declarations plus everything it imported:
    /// `declare_use_decl` installs an import into the module's binding frame,
    /// so `use lib::a::thing;` resolves when `a` only re-exported `thing`.
    /// Declarations are checked first because they are bound last and so win.
    /// `depth` bounds the chase, since a cycle of re-exports is a user error
    /// the type checker reports rather than something to hang on.
    fn exported_within(&self, module: &ModPath, name: &str, depth: usize) -> Option<VarId> {
        const MAX_REEXPORT_DEPTH: usize = 16;
        let decls = &self.ast.get(module)?.ast.decls;
        let declared = decls.iter().find_map(|decl| match decl {
            Decl::Fn(decl) if decl.name.name == name => Some(decl.metadata.1),
            Decl::Cell(decl) if decl.name.name == name => Some(decl.metadata.1),
            Decl::Enum(decl) if decl.name.name == name => decl.metadata.map(|(id, _)| id),
            _ => None,
        });
        if declared.is_some() || depth == MAX_REEXPORT_DEPTH {
            return declared;
        }
        decls.iter().find_map(|decl| {
            let Decl::Use(decl) = decl else { return None };
            let item = decl.path.last()?;
            if decl.alias.as_ref().unwrap_or(item).name != name {
                return None;
            }
            let exporter =
                module_prefix(module, decl.path.iter().map(|ident| ident.name.as_str()), 1);
            self.exported_within(&exporter, &item.name, depth + 1)
        })
    }

    fn arg_decl(
        &mut self,
        arg: &'a ArgDecl<arcstr::Substr, VarIdTyMetadata>,
        scope: cfgrammar::Span,
    ) {
        let key = DefKey::Var(arg.metadata.0);
        let ty = arg.metadata.1.clone();
        self.define(
            key.clone(),
            SymbolKind::Parameter,
            &arg.name,
            DefinitionInfo {
                detail: format!("parameter {}: {ty}", arg.name.name),
                ty: Some(ty.clone()),
                signature: None,
                full_span: None,
            },
        );
        self.index
            .scope_bindings
            .entry(self.path.to_path_buf())
            .or_default()
            .push(ScopeBinding {
                name: arg.name.name.to_string(),
                key,
                scope,
                available_after: scope.start(),
            });
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
            (TySpecKind::Ident(name), ty) => {
                // `ty_from_spec` resolves a name that is not a primitive by
                // looking it up, so an annotation can also name a declaration.
                // A cell's type carries the id of the cell that declared it,
                // which is the only such name with somewhere to jump to.
                let target = match declaring_cell(ty) {
                    Some(id) => Target::Def(DefKey::Var(id)),
                    None => builtin_type(&name.name).map_or(Target::Unresolved, |name| {
                        Target::Builtin(Builtin::Type(name))
                    }),
                };
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
                    let key = DefKey::Var(binding.metadata);
                    let ty = binding.value.ty();
                    self.define(
                        key.clone(),
                        SymbolKind::Local,
                        &binding.name,
                        DefinitionInfo {
                            detail: format!("let {}: {ty}", binding.name.name),
                            ty: Some(ty),
                            signature: None,
                            full_span: Some(binding.span),
                        },
                    );
                    self.index
                        .scope_bindings
                        .entry(self.path.to_path_buf())
                        .or_default()
                        .push(ScopeBinding {
                            name: binding.name.name.to_string(),
                            key,
                            scope: scope.span,
                            available_after: binding.span.end(),
                        });
                }
                Statement::ForLoop(loop_) => {
                    self.expr(&loop_.seq);
                    let key = DefKey::Var(loop_.metadata);
                    let ty = match loop_.seq.ty() {
                        Ty::Seq(element) => *element,
                        _ => Ty::Unknown,
                    };
                    self.define(
                        key.clone(),
                        SymbolKind::LoopVar,
                        &loop_.var,
                        DefinitionInfo {
                            detail: format!("loop variable {}: {ty}", loop_.var.name),
                            ty: Some(ty),
                            signature: None,
                            full_span: Some(loop_.span),
                        },
                    );
                    self.index
                        .scope_bindings
                        .entry(self.path.to_path_buf())
                        .or_default()
                        .push(ScopeBinding {
                            name: loop_.var.name.to_string(),
                            key,
                            scope: loop_.body.span,
                            available_after: loop_.body.span.start(),
                        });
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
        if self.visible(expr.span()) {
            self.index
                .expression_types
                .entry(self.path.to_path_buf())
                .or_default()
                .push((expr.span(), expr.ty()));
        }
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
                    self.record_hover_detail(
                        kwarg.name.span,
                        format!("keyword argument {}: {}", kwarg.name.name, kwarg.metadata),
                    );
                    self.expr(&kwarg.value);
                }
            }
            Expr::FieldAccess(access) => {
                let base = access.base.ty();
                self.expr(&access.base);
                let target = self.field_target(&base, &access.field.name);
                self.record(access.field.span, target);
                self.record_hover_detail(
                    access.field.span,
                    format!("field {}: {}", access.field.name, access.metadata),
                );
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
            // Comparisons are `BinOp::Cmp`, so this covers them too.
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
    ///
    /// `prefix` is already only the module segments, so none of it names an
    /// item to be dropped.
    fn module_path(&mut self, prefix: &'a [Ident<arcstr::Substr, VarIdTyMetadata>]) {
        for length in 1..=prefix.len() {
            let module = module_prefix(
                self.current,
                prefix[..length].iter().map(|ident| ident.name.as_str()),
                0,
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
                .map_or(
                    // A GDS-backed cell declares no fields to trace back to:
                    // its source declaration is only a signature, and the
                    // fields it publishes come from the layout at execution
                    // time. Treat them like any other compiler-provided field.
                    Target::Builtin(Builtin::Field(name.to_string())),
                    |fields| {
                        fields
                            .get(name)
                            .map_or(Target::Unresolved, |id| Target::Def(DefKey::Var(*id)))
                    },
                ),
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
    fn compiler_internal_paths_are_not_navigable() {
        assert!(!is_navigable(Path::new(crate::parse::CELL_PATH)));
        assert!(!is_navigable(Path::new("/virtual/sram.gds")));
        assert!(is_navigable(Path::new(STD_PATH)));
        assert!(is_navigable(Path::new(ROOT)));
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

    /// A type annotation resolves through an ordinary lookup when it is not a
    /// primitive, so it can name a cell.
    #[test]
    fn a_type_annotation_naming_a_cell_resolves_to_it() {
        check(
            r#"
cell inn$0er() {
    let met = rect("met1");
}

fn take(c: inn$0er) -> Float { 1. }

cell top() {
    eq(rect("met1").w, take(inner()));
}
"#,
            &["inner#0", "inner#0"],
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

    /// A GDS-backed cell's declaration is a signature the compiler generated,
    /// with no `let` to trace a field back to. Its fields come from the layout
    /// at execution time, so they are compiler-provided rather than names that
    /// failed to resolve.
    #[test]
    fn a_field_of_a_gds_backed_instance_is_a_builtin() {
        let source = "\
cell top() {
    let c = imported();
    let i = inst(c);
    eq(i.metal, 0.);
}
";
        // What `add_gds_imports` produces: a bare signature appended after the
        // editor-visible source and promoted to the front of declaration order.
        let generated = "cell imported() {}\n";
        let mut root =
            parse_source_text(format!("{source}\n{generated}"), PathBuf::from(ROOT)).unwrap();
        root.promote_last_declarations(1);
        root.source_text = arcstr::ArcStr::from(source);
        root.generated_declarations = 1;
        let std = parse_source_text(STD_SOURCE, PathBuf::from(STD_PATH)).unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (typed, _) = static_compile(&ast).unwrap();
        let index = NavIndex::build(&typed);

        let offset = source.find("metal").unwrap();
        let (_, target) = index
            .target_at(Path::new(ROOT), offset)
            .expect("the field is indexed");
        assert_eq!(*target, Target::Builtin(Builtin::Field("metal".to_owned())));

        // The generated signature itself is past the editor-visible source, so
        // nothing in it is indexed.
        assert!(
            index
                .refs
                .get(Path::new(ROOT))
                .is_some_and(|entries| entries.iter().all(|(span, _)| span.end() <= source.len()))
        );
    }

    /// An import resolves against what the exporting module makes available,
    /// which includes what that module itself imported.
    #[test]
    fn a_re_exported_item_resolves_to_its_original_declaration() {
        let source =
            "use lib::middle::thing;\n\ncell top() {\n    eq(rect(\"met1\").w, thing());\n}\n";
        let modules = [
            (Vec::new(), source, ROOT),
            (
                vec!["middle".to_owned()],
                "use lib::origin::thing;\n",
                "/virtual/middle.ar",
            ),
            (
                vec!["origin".to_owned()],
                "fn thing() -> Float { 3. }\n",
                "/virtual/origin.ar",
            ),
        ];
        let mut ast: IndexMap<_, _> = modules
            .into_iter()
            .map(|(module, text, path)| {
                (
                    module,
                    parse_source_text(text.to_owned(), PathBuf::from(path)).unwrap(),
                )
            })
            .collect();
        ast.insert(
            vec!["std".to_owned()],
            parse_source_text(STD_SOURCE, PathBuf::from(STD_PATH)).unwrap(),
        );
        let (typed, _) = static_compile(&ast).unwrap();
        let index = NavIndex::build(&typed);

        let definition = index
            .definition_at(Path::new(ROOT), source.find("thing").unwrap())
            .expect("the imported name resolves");
        assert_eq!(definition.kind, SymbolKind::Function);
        let DefLocation::Source(span) = &definition.location else {
            panic!("expected a source location, got {:?}", definition.location);
        };
        assert_eq!(span.path, Path::new("/virtual/origin.ar"));
    }

    /// A module made up entirely of GDS imports is recorded against the binary
    /// the cells came from. There is no Argon source there, so nothing in it
    /// is a place to send an editor.
    #[test]
    fn a_gds_only_module_is_not_a_file_to_jump_to() {
        let source = "cell top() {\n    let c = macros::sram();\n    let i = inst(c);\n}\n";
        let root = parse_source_text(source.to_owned(), PathBuf::from(ROOT)).unwrap();
        // What `add_gds_imports` produces for a namespaced import: a module of
        // bare signatures whose path is the `.gds` file and whose
        // editor-visible source is empty.
        let mut macros = parse_source_text(
            "cell sram() {}\n".to_owned(),
            PathBuf::from("/virtual/sram.gds"),
        )
        .unwrap();
        macros.promote_last_declarations(1);
        macros.source_text = arcstr::ArcStr::from("");
        let std = parse_source_text(STD_SOURCE, PathBuf::from(STD_PATH)).unwrap();
        let ast = IndexMap::from([
            (Vec::new(), root),
            (vec!["macros".to_owned()], macros),
            (vec!["std".to_owned()], std),
        ]);
        let (typed, _) = static_compile(&ast).unwrap();
        let index = NavIndex::build(&typed);

        assert!(
            index.source(Path::new("/virtual/sram.gds")).is_none(),
            "a binary is not an indexed source"
        );
        for name in ["macros", "sram"] {
            assert!(
                index
                    .definition_at(Path::new(ROOT), source.find(name).unwrap())
                    .is_none(),
                "{name} should have nowhere to jump to"
            );
        }
    }

    /// `&&`, `||` and comparisons are all `Expr::BinOp`, and `!` is
    /// `Expr::UnaryOp`, so one arm each walks their operands. Worth pinning:
    /// a missed arm costs navigation inside a condition silently.
    #[test]
    fn operands_of_boolean_and_comparison_operators_are_walked() {
        check(
            r#"
cell top(alpha: Float, flag: Bool) {
    let ok = al$0pha >= 40. && !fl$0ag;
    if o$0k { } else { };
}
"#,
            &["alpha#0", "flag#0", "ok#0"],
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

    fn labels(candidates: Vec<CompletionCandidate>) -> Vec<String> {
        candidates
            .into_iter()
            .map(|candidate| candidate.label)
            .collect()
    }

    #[test]
    fn static_completion_does_not_require_an_indexed_file() {
        let labels = labels(NavIndex::default().completions_at(Path::new("new.ar"), 0));
        for expected in ["cell", "rect", "Float"] {
            assert!(
                labels.iter().any(|label| label == expected),
                "missing {expected}: {labels:?}"
            );
        }
    }

    #[test]
    fn completion_respects_lexical_scope_and_declaration_order() {
        let source = r#"
fn helper(value: Float) -> Float { value }
cell earlier_cell() {}
cell top(arg: Float) {
    let earlier = rect("met1");
    if true {
        let nested = 1.;
        eq($0earlier.x0, arg);
    } else {};
    let later = 2.;
}
"#;
        let (_, index, offsets) = index(source);
        let labels = labels(index.completions_at(Path::new(ROOT), offsets[0]));
        for expected in ["arg", "earlier", "nested", "earlier_cell", "helper", "rect"] {
            assert!(
                labels.iter().any(|label| label == expected),
                "missing {expected}: {labels:?}"
            );
        }
        for hidden in ["later", "top"] {
            assert!(
                !labels.iter().any(|label| label == hidden),
                "unexpected {hidden}: {labels:?}"
            );
        }
    }

    #[test]
    fn completion_is_type_aware_for_members_and_enum_paths() {
        let source = r#"
enum Mode { Fast, Small }
cell top() {
    let shape = rect("met1");
    eq(shape.x0, 0.);
}
"#;
        let (source, index, _) = index(source);
        let base_end = source.find("shape.x0").unwrap() + "shape".len();
        let fields = labels(index.member_completions_at(Path::new(ROOT), base_end));
        for expected in ["h", "layer", "w", "x0", "x1", "y0", "y1"] {
            assert!(
                fields.iter().any(|field| field == expected),
                "missing {expected}: {fields:?}"
            );
        }
        assert_eq!(
            labels(index.qualified_completions(Path::new(ROOT), &["Mode".to_owned()])),
            vec!["Fast", "Small"]
        );
        let rect = source.find("rect").unwrap();
        let signature = index.signature_at(Path::new(ROOT), rect).unwrap();
        let keyword_args = index.keyword_completions_for_signature(&signature);
        assert!(keyword_args.iter().any(|candidate| {
            candidate.label == "x0" && candidate.insert_text.as_deref() == Some("x0=")
        }));
    }

    #[test]
    fn hover_signatures_and_outline_use_typed_declarations() {
        let source = "fn scale(value: Float) -> Float { value * 2. }\ncell top() { scale(1.); }\n";
        let (source, index, _) = index(source);
        let call = source.rfind("scale").unwrap();
        assert_eq!(
            index.signature_at(Path::new(ROOT), call).unwrap().label,
            "fn scale(value: Float) -> Float"
        );
        assert_eq!(
            index.hover_at(Path::new(ROOT), call).unwrap().contents,
            "fn scale(value: Float) -> Float"
        );
        let symbols = index.document_symbols(Path::new(ROOT));
        assert!(symbols.iter().any(|symbol| symbol.name == "scale"));
        assert!(symbols.iter().any(|symbol| symbol.name == "value"));
        assert!(symbols.iter().any(|symbol| symbol.name == "top"));
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
