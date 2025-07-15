//! # Argon compiler
//!
//! Pass 1: assign variable IDs
//! Pass 2: type checking
//! Pass 3: solving
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    ops::ControlFlow,
    sync::Arc,
};

use anyhow::{anyhow, bail, Result};
use enumify::enumify;
use serde::{Deserialize, Serialize};

use crate::{
    ast::{
        ArgDecl, Ast, AstMetadata, AstTransformer, BinOp, BinOpExpr, CellDecl, ComparisonExpr,
        Decl, EnumDecl, EnumValue, Expr, Ident, IfExpr, Statement,
    },
    parse::ParseMetadata,
};
use crate::{
    ast::{FieldAccessExpr, Typ},
    parse::ParseAst,
};

pub(crate) struct VarIdPass<'a> {
    next_id: VarId,
    bindings: Vec<HashMap<&'a str, VarId>>,
}

pub(crate) struct VarIdMetadata;

impl AstMetadata for VarIdMetadata {
    type Ident = ();
    type EnumDecl = ();
    type CellDecl = ();
    type ConstantDecl = ();
    type Statement = ();
    type IfExpr = ();
    type BinOpExpr = ();
    type ComparisonExpr = ();
    type FieldAccessExpr = ();
    type EnumValue = ();
    type CallExpr = ();
    type EmitExpr = ();
    type Args = ();
    type KwArgValue = ();
    type ArgDecl = ();
    type Typ = ();
    type VarExpr = VarId;
}

impl<'a> VarIdPass<'a> {
    pub(crate) fn new() -> Self {
        Self {
            // allocate space for the global namespace
            bindings: vec![HashMap::new()],
            next_id: 1,
        }
    }

    fn lookup(&self, name: &str) -> Option<VarId> {
        for map in self.bindings.iter().rev() {
            if let Some(id) = map.get(name) {
                return Some(*id);
            }
        }
        None
    }

    fn alloc(&mut self, name: &'a str) -> VarId {
        let id = self.next_id;
        self.bindings.last_mut().unwrap().insert(name, id);
        self.next_id += 1;
        id
    }

    pub(crate) fn execute(mut self, input: CompileInput<'a>) -> Ast<'a, VarIdMetadata> {
        let cell = input
            .ast
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Cell(
                    v @ CellDecl {
                        name: Ident { name, .. },
                        ..
                    },
                ) if *name == input.cell => Some(v),
                _ => None,
            })
            .expect("top cell not found");
        for (name, value) in input.params {
            assert!(self.lookup(name).is_none(), "no duplicate parameters");
            self.alloc(name);
        }
        let stmts = cell
            .stmts
            .iter()
            .map(|stmt| self.transform_statement(stmt))
            .collect();

        Ast {
            decls: vec![Decl::Cell(CellDecl {
                name: self.transform_ident(&cell.name),
                args: cell
                    .args
                    .iter()
                    .map(|arg| self.transform_arg_decl(arg))
                    .collect(),
                stmts,
                metadata: (),
            })],
        }
    }
}

impl<'a> AstTransformer<'a> for VarIdPass<'a> {
    type Input = ParseMetadata;
    type Output = VarIdMetadata;

    fn dispatch_ident(
        &mut self,
        input: &Ident<'a, Self::Input>,
    ) -> <Self::Output as AstMetadata>::Ident {
    }

    fn dispatch_var_expr(
        &mut self,
        input: &crate::ast::VarExpr<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::VarExpr {
        self.lookup(input.name.name)
            .expect("used variable before declaration")
    }

    fn dispatch_enum_decl(
        &mut self,
        input: &crate::ast::EnumDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        variants: &Vec<Ident<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        input: &CellDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        args: &Vec<ArgDecl<'a, Self::Output>>,
        stmts: &Vec<Statement<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::CellDecl {
    }

    fn dispatch_constant_decl(
        &mut self,
        input: &crate::ast::ConstantDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        ty: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ConstantDecl {
    }

    fn dispatch_if_expr(
        &mut self,
        input: &IfExpr<'a, Self::Input>,
        cond: &Expr<'a, Self::Output>,
        then: &Expr<'a, Self::Output>,
        else_: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::IfExpr {
    }

    fn dispatch_bin_op_expr(
        &mut self,
        input: &BinOpExpr<'a, Self::Input>,
        left: &Expr<'a, Self::Output>,
        right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::BinOpExpr {
    }

    fn dispatch_comparison_expr(
        &mut self,
        input: &ComparisonExpr<'a, Self::Input>,
        left: &Expr<'a, Self::Output>,
        right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ComparisonExpr {
    }

    fn dispatch_field_access_expr(
        &mut self,
        input: &crate::ast::FieldAccessExpr<'a, Self::Input>,
        base: &Expr<'a, Self::Output>,
        field: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FieldAccessExpr {
    }

    fn dispatch_enum_value(
        &mut self,
        input: &EnumValue<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        variant: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EnumValue {
    }

    fn dispatch_call_expr(
        &mut self,
        input: &crate::ast::CallExpr<'a, Self::Input>,
        func: &Ident<'a, Self::Output>,
        args: &crate::ast::Args<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CallExpr {
    }

    fn dispatch_emit_expr(
        &mut self,
        input: &crate::ast::EmitExpr<'a, Self::Input>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EmitExpr {
    }

    fn dispatch_args(
        &mut self,
        input: &crate::ast::Args<'a, Self::Input>,
        posargs: &Vec<Expr<'a, Self::Output>>,
        kwargs: &Vec<crate::ast::KwArgValue<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::Args {
    }

    fn dispatch_kw_arg_value(
        &mut self,
        input: &crate::ast::KwArgValue<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::KwArgValue {
    }

    fn dispatch_arg_decl(
        &mut self,
        input: &ArgDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        ty: &Typ<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ArgDecl {
    }

    fn transform_arg_decl(
        &mut self,
        input: &ArgDecl<'a, Self::Input>,
    ) -> ArgDecl<'a, Self::Output> {
        assert!(
            self.lookup(input.name.name).is_none(),
            "argument should not already be declared"
        );
        self.alloc(input.name.name);
        let name = self.transform_ident(&input.name);
        let ty = self.transform_typ(&input.ty);
        ArgDecl {
            name,
            ty,
            metadata: (),
        }
    }

    fn enter_scope(&mut self, _input: &crate::ast::Scope<'a, Self::Input>) {
        self.bindings.push(Default::default());
    }

    fn exit_scope(
        &mut self,
        _input: &crate::ast::Scope<'a, Self::Input>,
        _output: &crate::ast::Scope<'a, Self::Output>,
    ) {
        self.bindings.pop();
    }

    fn transform_statement(
        &mut self,
        input: &Statement<'a, Self::Input>,
    ) -> Statement<'a, Self::Output> {
        match input {
            Statement::Expr { value, semicolon } => Statement::Expr {
                value: self.transform_expr(value),
                semicolon: *semicolon,
            },
            Statement::LetBinding { name, value } => {
                self.alloc(name.name);
                let name = self.transform_ident(name);
                let value = self.transform_expr(value);
                Statement::LetBinding { name, value }
            }
        }
    }
}

pub struct CompileInput<'a> {
    pub ast: &'a ParseAst<'a>,
    pub cell: &'a str,
    pub params: HashMap<&'a str, f64>,
}

pub type ScopeId = u64;
pub type VarId = u64;
pub type ConstraintVarId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    pub span: cfgrammar::Span,
    pub id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rect<T> {
    pub layer: String,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
    pub source: Option<SourceInfo>,
}

// #[derive(Default)]
// struct CompileState<'a> {
//     deferred: Vec<PartialEval<'a>>,
//     bindings: HashMap<VarId, Value<'a>>,
// }
//
// pub struct CompiledCell {
//     rects: Vec<Rect<f64>>,
// }
//
// enum Defer<R, D> {
//     Ready(R),
//     Deferred(D),
// }
//
// type DeferExpr<'a> = Defer<Value<'a>, PartialEval<'a>>;
//
// struct PartialEval<'a> {
//     state: PartialEvalState<'a>,
//     scope_id: ScopeId,
//     predicate: ProgressPredicate,
// }
//
// #[derive(Clone)]
// struct ProgressPredicate {
//     // sum of products
//     terms: Vec<Vec<ConstraintVarId>>,
// }

// enum PartialEvalState<'a> {
//     If(Box<PartialIfExpr<'a>>),
//     Comparison(Box<ComparisonExpr<'a>>),
//     BinOp(Box<BinOpExpr<'a>>),
//     Call(CallExpr<'a>),
//     Emit(Box<EmitExpr<'a>>),
//     EnumValue(EnumValue<'a>),
//     FieldAccess(Box<FieldAccessExpr<'a>>),
//     Var(Ident<'a>),
//     FloatLiteral(FloatLiteral),
// }
//
// struct PartialIfExpr<'a> {
//     expr: IfExpr<'a>,
//     state: IfExprState<'a>,
// }
//
// pub enum IfExprState<'a> {
//     Cond(PartialEval<'a>),
//     Then(PartialEval<'a>),
//     Else(PartialEval<'a>),
// }
//
// pub struct BinOpExprState<'a> {
//     left: PartialEval<'a>,
//     right: PartialEval<'a>,
// }
//
// struct PartialComparisonExpr<'a> {
//     expr: ComparisonExpr<'a>,
//     state: ComparisonExprState<'a>,
// }
//
// fn pass1(input: CompileInput<'_>) -> Ast<'_, Pass1Metadata> {
//     Pass1State::default().execute(input)
// }
//
// fn compile(input: CompileInput<'_>) -> CompiledCell {
//     let mut state = CompileState::default();
//     for stmt in &mut cell.stmts {
//         eval_stmt(&mut state, stmt);
//     }
//     todo!()
// }
//
// fn eval_stmt(state: &mut CompileState, stmt: &Statement) {
//     match stmt {
//         Statement::LetBinding { name, value } => {
//             let id = state.allocate_id();
//             state.active_bindings.insert(name.name, id);
//             value
//         }
//         Statement::Expr(expr) => {}
//     }
// }
//
// fn eval_expr<'a>(state: &mut CompileState, expr: &Expr<'a>) -> DeferExpr<'a> {
//     match expr {
//         Expr::If(e) => match eval_expr(state, &e.cond) {
//             Defer::Ready(v) => match eval_expr(state, &e.then) {
//                 Defer::Ready(v) => Defer::Ready(v),
//                 Defer::Deferred(state) => Defer::Deferred(PartialEval {
//                     state: PartialEvalState::If(Box::new(PartialIfExpr {
//                         expr: e,
//                         state: IfExprState::Then(state),
//                     })),
//                     scope_id: 0,
//                     predicate: state.predicate.clone(),
//                 }),
//             },
//             Defer::Deferred(state) => Defer::Deferred(PartialEval {
//                 state: PartialEvalState::If(Box::new(PartialIfExpr {
//                     expr: e,
//                     state: IfExprState::Cond(state),
//                 })),
//                 scope_id: 0,
//                 predicate: state.predicate.clone(),
//             }),
//         },
//         _ => todo!(),
//     }
// }
//
// impl<'a> Scope<'a> {
//     fn lookup(&self, var: VarId) -> Option<&VarBinding<'a>> {
//         self.bindings.get(&var).or_else(|| self.parent.lookup(var))
//     }
// }
//
// struct CellCtx<'a> {
//     cell: Cell,
//     bindings: HashMap<&'a str, Value<'a>>,
//     next_id: u64,
// }
//
// impl<'a> CellCtx<'a> {
//     pub fn new() -> Self {
//         Self {
//             cell: Cell::new(),
//             bindings: HashMap::new(),
//             next_id: 0,
//         }
//     }
//
//     fn alloc_id(&mut self) -> u64 {
//         self.next_id = self.next_id.checked_add(1).unwrap();
//         self.next_id
//     }
//
//     fn compile(mut self, input: CompileInput<'a>) -> Result<CompiledCell> {
//         let cell = input
//             .ast
//             .decls
//             .iter()
//             .find_map(|d| match d {
//                 Decl::Cell(
//                     v @ CellDecl {
//                         name: Ident { name, .. },
//                         ..
//                     },
//                 ) if *name == input.cell => Some(v),
//                 _ => None,
//             })
//             .ok_or_else(|| anyhow!("no cell named `{}`", input.cell))?;
//         for (name, value) in input.params {
//             self.bindings.insert(
//                 name,
//                 Value::Linear(LinearExpr {
//                     coeffs: Vec::new(),
//                     constant: value,
//                 }),
//             );
//         }
//         for stmt in cell.stmts.iter() {
//             match stmt {
//                 Statement::Expr(expr) => {
//                     self.eval(expr)?;
//                 }
//                 Statement::LetBinding { name, value } => {
//                     let value = self.eval(value)?;
//                     self.bindings.insert(name.name, value);
//                 }
//             }
//         }
//         self.cell.solve()
//     }
//
//     fn eval(&mut self, expr: &Expr<'a>) -> Result<Value<'a>> {
//         match expr {
//             Expr::BinOp(expr) => {
//                 let left = self.eval(&expr.left)?.try_linear(expr.left.span())?;
//                 let right = self.eval(&expr.right)?.try_linear(expr.right.span())?;
//                 match expr.op {
//                     BinOp::Add => Ok(Value::Linear(left + right)),
//                     BinOp::Sub => Ok(Value::Linear(left - right)),
//                     op => bail!(
//                         "unsupported binary operator: {op:?} in expression at {:?}",
//                         expr.span
//                     ),
//                 }
//             }
//             Expr::Call(expr) => match expr.func.name {
//                 "Rect" => {
//                     assert_eq!(expr.args.posargs.len(), 1);
//                     let layer = self
//                         .eval(&expr.args.posargs[0])?
//                         .try_enum_value(expr.args.posargs[0].span())?;
//                     let attrs = Attrs {
//                         source: Some(SourceInfo {
//                             span: expr.span,
//                             id: self.alloc_id(),
//                         }),
//                     };
//                     let rect = self.cell.physical_rect(layer.variant.name.into(), attrs);
//                     for arg in expr.args.kwargs.iter() {
//                         let value = self.eval(&arg.value)?;
//                         match arg.name.name {
//                             "x0" => {
//                                 let mut value = value.try_linear(arg.span)?;
//                                 value.coeffs.push((-1., rect.x0));
//                                 self.cell.add_constraint(Constraint::Linear(
//                                     value.into_eq_constraint(ConstraintAttrs {
//                                         span: Some(arg.span),
//                                     }),
//                                 ));
//                             }
//                             "x1" => {
//                                 let mut value = value.try_linear(arg.span)?;
//                                 value.coeffs.push((-1., rect.x1));
//                                 self.cell.add_constraint(Constraint::Linear(
//                                     value.into_eq_constraint(ConstraintAttrs {
//                                         span: Some(arg.span),
//                                     }),
//                                 ));
//                             }
//                             "y0" => {
//                                 let mut value = value.try_linear(arg.span)?;
//                                 value.coeffs.push((-1., rect.y0));
//                                 self.cell.add_constraint(Constraint::Linear(
//                                     value.into_eq_constraint(ConstraintAttrs {
//                                         span: Some(arg.span),
//                                     }),
//                                 ));
//                             }
//                             "y1" => {
//                                 let mut value = value.try_linear(arg.span)?;
//                                 value.coeffs.push((-1., rect.y1));
//                                 self.cell.add_constraint(Constraint::Linear(
//                                     value.into_eq_constraint(ConstraintAttrs {
//                                         span: Some(arg.span),
//                                     }),
//                                 ));
//                             }
//                             arg_name => {
//                                 bail!("unexpected argument: `{arg_name}` at {:?}", arg.name.span)
//                             }
//                         }
//                     }
//                     Ok(Value::Rect(rect))
//                 }
//                 "eq" => {
//                     assert_eq!(expr.args.posargs.len(), 2);
//                     let lhs = self
//                         .eval(&expr.args.posargs[0])?
//                         .try_linear(expr.args.posargs[0].span())?;
//                     let rhs = self
//                         .eval(&expr.args.posargs[1])?
//                         .try_linear(expr.args.posargs[0].span())?;
//                     self.cell
//                         .add_constraint(Constraint::Linear((lhs - rhs).into_eq_constraint(
//                             ConstraintAttrs {
//                                 span: Some(expr.span),
//                             },
//                         )));
//                     Ok(Value::None)
//                 }
//                 f => bail!("unexpected draw call `{f}` at {:?}", expr.span),
//             },
//             Expr::FloatLiteral(v) => Ok(Value::Linear(LinearExpr {
//                 constant: v.value,
//                 coeffs: Vec::new(),
//             })),
//             Expr::Var(v) => Ok(self
//                 .bindings
//                 .get(v.name)
//                 .ok_or_else(|| anyhow!("no variable named `{}`", v.name))?
//                 .clone()),
//             Expr::FieldAccess(expr) => {
//                 let base = self.eval(&expr.base)?;
//                 match base {
//                     Value::Rect(r) => Ok(Value::Linear(LinearExpr::from(match expr.field.name {
//                         "x0" => r.x0,
//                         "x1" => r.x1,
//                         "y0" => r.y0,
//                         "y1" => r.y1,
//                         f => bail!(
//                             "type Rect has no field `{f}` (encountered at {:?})",
//                             expr.field.span
//                         ),
//                     }))),
//                     _ => bail!(
//                         "object no field `{}` (encountered at {:?})",
//                         expr.field.name,
//                         expr.field.span
//                     ),
//                 }
//             }
//             Expr::EnumValue(v) => Ok(Value::EnumValue(v.clone())),
//             Expr::Emit(v) => {
//                 let value = self.eval(&v.value)?;
//                 let rect = value.try_rect(v.span)?;
//                 self.cell.emit_rect(rect.clone());
//                 Ok(Value::Rect(rect))
//             }
//             expr => bail!("cannot evaluate the expression at {:?}", expr.span()),
//         }
//     }
// }
//
// #[enumify]
// #[derive(Debug, Clone)]
// pub enum Value<'a> {
//     EnumValue(EnumValue<'a>),
//     Linear(LinearExpr),
//     Rect(Rect<Var>),
//     None,
// }
//
// impl<'a> Value<'a> {
//     pub fn try_enum_value(self, espan: cfgrammar::Span) -> Result<EnumValue<'a>> {
//         self.into_enum_value()
//             .ok_or_else(|| anyhow!("expected value to be of type EnumValue at {espan:?}"))
//     }
//     pub fn try_linear(self, espan: cfgrammar::Span) -> Result<LinearExpr> {
//         self.into_linear()
//             .ok_or_else(|| anyhow!("expected value to be of type LinearExpr at {espan:?}"))
//     }
//     pub fn try_rect(self, espan: cfgrammar::Span) -> Result<Rect<Var>> {
//         self.into_rect()
//             .ok_or_else(|| anyhow!("expected value to be of type Rect at {espan:?}"))
//     }
// }
//
// #[derive(Debug, Clone)]
// pub struct LinearExpr {
//     coeffs: Vec<(f64, Var)>,
//     constant: f64,
// }
//
// impl std::ops::Add<LinearExpr> for LinearExpr {
//     type Output = Self;
//     fn add(self, rhs: LinearExpr) -> Self::Output {
//         Self {
//             coeffs: self.coeffs.into_iter().chain(rhs.coeffs).collect(),
//             constant: self.constant + rhs.constant,
//         }
//     }
// }
//
// impl std::ops::Sub<LinearExpr> for LinearExpr {
//     type Output = Self;
//     fn sub(self, rhs: LinearExpr) -> Self::Output {
//         Self {
//             coeffs: self
//                 .coeffs
//                 .into_iter()
//                 .chain(rhs.coeffs.into_iter().map(|(c, v)| (-c, v)))
//                 .collect(),
//             constant: self.constant - rhs.constant,
//         }
//     }
// }
//
// impl LinearExpr {
//     pub fn into_eq_constraint(self, attrs: ConstraintAttrs) -> crate::solver::LinearConstraint {
//         crate::solver::LinearConstraint {
//             coeffs: self.coeffs.into_iter().map(|(k, v)| (k, v)).collect(),
//             constant: self.constant,
//             is_equality: true,
//             attrs,
//         }
//     }
// }
//
// impl From<Var> for LinearExpr {
//     fn from(value: Var) -> Self {
//         Self {
//             coeffs: vec![(1., value)],
//             constant: 0.,
//         }
//     }
// }
//
// pub fn compile(input: CompileInput) -> Result<SolvedCell> {
//     let ctx = CellCtx::new();
//     ctx.compile(input)
// }
//
