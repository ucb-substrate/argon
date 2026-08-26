use enumify::enumify;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ast::Span,
    solver::{ConstraintId, Var},
};

use super::{CellId, CompiledData, Ty};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticError {
    pub span: Span,
    pub kind: StaticErrorKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Error)]
pub enum StaticErrorKind {
    /// Multiple declarations with the same name.
    ///
    /// For example, two cells named `my_cell`.
    #[error("duplicate name declaration")]
    DuplicateNameDeclaration,
    /// Attempted to declare an object with the same name as a built-in object.
    ///
    /// For example, users cannot declare cells or functions named `rect`.
    #[error("redeclaration of built-in object")]
    RedeclarationOfBuiltin,
    /// Attempted to treat a non-enum object like an enum using the `::` operator.
    #[error("expected an enum")]
    NotAnEnum,
    /// Attempted to use an enum variant that is not declared by the enum.
    #[error("not a variant of the enum: {0}")]
    InvalidVariant(String),
    /// A cell had an expression in tail position, which is not permitted.
    #[error("cells may not have an expression in tail position")]
    CellWithTailExpr,
    /// If conditions must have type bool.
    #[error("if conditions must have type bool")]
    IfCondNotBool,
    /// Branches in expressions must evaluate to the same type.
    #[error("branches must evaluate to same type")]
    BranchesDifferentTypes,
    /// Multiple match arms have matching patterns.
    #[error("match arms must be distinct")]
    DuplicateMatchArm,
    /// Match arms must be comprehensive.
    #[error("match arms must be comprehensive")]
    MatchArmsNotComprehensive,
    /// The operands in a binary expression must have the same type.
    #[error("operands of binary expression must have the same type")]
    BinOpMismatchedTypes,
    /// Floating-point values cannot be compared for equality or inequality.
    #[error("cannot compare equality or inequality of floating point numbers")]
    FloatEquality,
    /// Enum values cannot be ordered.
    #[error("cannot perform greater/less than comparisons on enum values")]
    EnumsNotOrd,
    /// Nil values cannot be ordered.
    #[error("cannot perform greater/less than comparisons on nil")]
    NilNotOrd,
    /// Empty sequence values cannot be ordered.
    #[error("cannot perform greater/less than comparisons on seq nil")]
    SeqNilNotOrd,
    /// Sequences may only be compared with an empty sequence for equality.
    #[error("sequences can only be compared for equality/inequality to seq nil (`[]`)")]
    SeqMustCompareEqSeqNil,
    /// A type cannot be used in a binary expression.
    #[error("type cannot be used in a binary expression: {0:?}")]
    BinOpInvalidType(Ty),
    /// A type cannot be used in a unary operation.
    #[error("type cannot be used in a unary operation")]
    UnaryOpInvalidType,
    /// A type cannot be used in a comparison expression.
    #[error("type cannot be used in comparison expression")]
    ComparisonInvalidType,
    /// A referenced type has not been declared.
    #[error("unknown type")]
    UnknownType,
    /// The requested field does not exist on the given type.
    #[error("no field {field} on type {ty:?}")]
    NoFieldOnTy { field: String, ty: Ty },
    /// A tuple index is out of range.
    #[error("tuple index out of range")]
    TupleIndexOutOfRange,
    /// The given type does not support positional field access.
    #[error("the fields of type {ty:?} cannot be accessed via index field access")]
    CannotIndexFieldAccess { ty: Ty },
    /// The given type cannot be indexed.
    #[error("type {ty:?} cannot be indexed")]
    CannotIndex { ty: Ty },
    /// The given type cannot be iterated.
    #[error("cannot iterate over type {ty:?}")]
    CannotIterate { ty: Ty },
    /// A value has the wrong concrete type.
    #[error("expected type {expected:?}, found {found:?}")]
    IncorrectTy { expected: Ty, found: Ty },
    /// A value does not belong to the expected type category.
    #[error("expected type category {expected}, found {found:?}")]
    IncorrectTyCategory { found: Ty, expected: String },
    /// A list constructor was called without elements.
    #[error("list constructors cannot be empty (use `[]` for an empty list)")]
    EmptyListConstructor,
    /// A function or cell received the wrong number of positional arguments.
    #[error("expected {expected} position arguments, found {found}")]
    CallIncorrectPositionalArity { expected: usize, found: usize },
    /// A call contains an unsupported keyword argument.
    #[error("invalid keyword argument")]
    InvalidKwArg,
    /// A call supplies the same keyword argument more than once.
    #[error("duplicate keyword argument")]
    DuplicateKwArg,
    /// An identifier was used without being declared in the current scope.
    #[error("`{name}` is not declared in this scope")]
    UndeclaredVar { name: String },
    /// A cell was referenced before its declaration was processed.
    #[error(
        "cannot use `{name}` before its declaration; move the `cell {name} ...` declaration above this use"
    )]
    UseBeforeDeclaration { name: String },
    /// A value of the given type cannot be called.
    #[error("cannot call type {0:?}")]
    CannotCall(Ty),
    /// The requested type cast is invalid.
    #[error("invalid type cast")]
    InvalidCast,
    /// A referenced module does not exist in the loaded workspace.
    #[error("module `{module}` does not exist or could not be loaded")]
    InvalidMod { module: String },
    /// A `use` path names an item that does not exist in the target module.
    #[error("unresolved import `{path}`")]
    UnresolvedImport { path: String },
    /// Module references form a dependency cycle.
    #[error("cyclic module dependency: {cycle}")]
    CyclicModuleDependency { cycle: String },
    /// Source text could not be lexed.
    #[error("error during lexing: {0}")]
    LexError(String),
    /// Source text could not be parsed.
    #[error("error during parsing: {0}")]
    ParseError(String),
    /// A layer-properties file is invalid.
    #[error("{0}")]
    InvalidLyp(String),
    /// A source file could not be loaded or resolved.
    #[error("could not load source: {0}")]
    SourceError(String),
    /// The requested feature is not implemented.
    #[error("unimplemented")]
    Unimplemented,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecError {
    pub span: Option<Span>,
    pub cell: CellId,
    pub kind: ExecErrorKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Error)]
pub enum ExecErrorKind {
    /// A transformation contains a non-Manhattan rotation.
    #[error("non-Manhattan rotation")]
    InvalidRotation,
    /// An imported GDS file could not be loaded or converted.
    #[error("invalid GDS import: {0}")]
    InvalidGds(String),
    /// The requested cell does not exist.
    #[error("invalid cell `{0}`")]
    InvalidCell(String),
    /// A cell invocation supplied the wrong number of arguments.
    #[error("invalid cell arguments: expected {expected} arguments, found {found}")]
    InvalidCellArity { expected: usize, found: usize },
    /// A cell invocation supplied an argument of the wrong type.
    #[error("invalid cell argument {index}: expected {expected:?}, found {found}")]
    InvalidCellArgumentType {
        index: usize,
        expected: Ty,
        found: String,
    },
    /// A cell does not have enough constraints for a unique solution.
    #[error("cell is underconstrained")]
    Underconstrained,
    /// A rectangle uses a layer absent from the layer-properties file.
    #[error("rectangle uses layer `{layer}`, which is not defined in LYP file `{lyp}`")]
    IllegalLayer { layer: String, lyp: String },
    /// A text label uses a layer absent from the layer-properties file.
    #[error("text uses layer `{layer}`, which is not defined in LYP file `{lyp}`")]
    IllegalTextLayer { layer: String, lyp: String },
    /// A constraint conflicts with the rest of the system.
    #[error("inconsistent constraint")]
    InconsistentConstraint(ConstraintId),
    /// A value is not sufficiently close to its required rounding step.
    #[error("invalid rounding")]
    InvalidRounding(Var),
    /// A cell or instance has no bounding box.
    #[error("empty bbox")]
    EmptyBbox,
    /// An optional field was accessed without a value.
    #[error("empty field (field was not assigned a value)")]
    EmptyField,
    /// Rectangle edges appear in the wrong order.
    #[error("rect edges are in the wrong order: {0}")]
    FlippedRect(String),
    /// A polygon does not contain enough vertices.
    #[error("a polygon requires at least three points")]
    InvalidPolygon,
    /// A path does not contain enough centerline points.
    #[error("a path requires at least two points")]
    InvalidPath,
    /// An operation received an incompatible runtime value.
    #[error("operation on an incompatible type (check usage of `Any`)")]
    InvalidType,
    /// A runtime cast received an incompatible value.
    #[error("cast to an incompatible type (check usage of `Any`)")]
    InvalidCast,
    /// A sequence index is outside its valid range.
    #[error("index out of bounds")]
    IndexOutOfBounds,
    /// The head of an empty list was requested.
    #[error("attempted to access the head of an empty list")]
    HeadEmptyList,
    /// The tail of an empty list was requested.
    #[error("attempted to access the tail of an empty list")]
    TailEmptyList,
}

impl ExecErrorKind {
    pub fn is_invalid_cell(&self) -> bool {
        matches!(
            self,
            Self::InvalidCell(_)
                | Self::InvalidCellArity { .. }
                | Self::InvalidCellArgumentType { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[enumify]
pub enum CompileOutput {
    FatalParseErrors,
    StaticErrors(StaticErrorCompileOutput),
    ExecErrors(ExecErrorCompileOutput),
    Valid(CompiledData),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticErrorCompileOutput {
    pub errors: Vec<StaticError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecErrorCompileOutput {
    pub errors: Vec<ExecError>,
    pub output: Option<CompiledData>,
}
