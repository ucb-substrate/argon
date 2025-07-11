//! # Argon compiler
//! 
//! Pass 1: assign variable IDs
//! Pass 2: type checking
//! Pass 3: solving
use std::{collections::HashMap, ops::ControlFlow};

use anyhow::{anyhow, bail, Result};
use enumify::enumify;

use crate::parse::{ArgonAst, BinOp, CellDecl, Decl, EnumValue, Expr, Ident, IfExpr, Statement};

#[derive(Default)]
struct VarAssignState<'a> {
    next_id: VarId,
    active_bindings: HashMap<&'a str, VarId>,
}

impl<'a> VarAssignState<'a> {
    fn allocate_id(&mut self) -> VarId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

#[derive(Default)]
struct SolveState<'a> {
    deferred: Vec<PartialEval<'a>>,
    bindings: HashMap<VarId, Value<'a>>,
}

pub struct SolveInput<'a> {
    pub ast: &'a ArgonAst<'a>,
    pub cell: &'a str,
    pub params: HashMap<&'a str, f64>,
}

pub type VarId = u64;

type FutureValue<'a> = ControlFlow<VarId, Value<'a>>;

enum PartialEval<'a> {
    If(Box<PartialIfExpr<'a>>),
}

struct PartialIfExpr<'a> {
    expr: IfExpr<'a>,
    state: IfExprState<'a>,
}

pub enum IfExprState<'a> {
    Cond(PartialEval<'a>),
    Then(PartialEval<'a>),
    Else(PartialEval<'a>),
}

fn assign_var_ids(mut ast: ArgonAst<'_>) -> ArgonAst<'_> {
    let mut state = VarAssignState::default();
    for decl in &mut ast.decls {
        match decl {
            Decl::Cell(cell) => {
                for arg in &mut cell.args {
                    if state.active_bindings.contains_key(arg.name.name) {
                        panic!("detected duplicate argument name")
                    }
                    let id = state.allocate_id();
                    state.active_bindings.insert(arg.name.name, id);
                    arg.name.id = Some(id);
                }
                for stmt in &mut cell.stmts {
                    match stmt {
                        Statement::LetBinding { name, value } => {
                            let id = state.allocate_id();
                            state.active_bindings.insert(arg.name.name, id);
                            name.name.id = Some(id);
                        }
                        Statement::Expr(expr) => {

                        }
                    }

                }
            }
            _ => {}
        }
    }

    ast
}

fn solve(input: SolveInput<'_>) -> SolvedCell {
    let mut state = SolveState::default();
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
            .unwrap();
    todo!()
}

struct CellCtx<'a> {
    cell: Cell,
    bindings: HashMap<&'a str, Value<'a>>,
    next_id: u64,
}

impl<'a> CellCtx<'a> {
    pub fn new() -> Self {
        Self {
            cell: Cell::new(),
            bindings: HashMap::new(),
            next_id: 0,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id = self.next_id.checked_add(1).unwrap();
        self.next_id
    }

    fn compile(mut self, input: CompileInput<'a>) -> Result<SolvedCell> {
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
            .ok_or_else(|| anyhow!("no cell named `{}`", input.cell))?;
        for (name, value) in input.params {
            self.bindings.insert(
                name,
                Value::Linear(LinearExpr {
                    coeffs: Vec::new(),
                    constant: value,
                }),
            );
        }
        for stmt in cell.stmts.iter() {
            match stmt {
                Statement::Expr(expr) => {
                    self.eval(expr)?;
                }
                Statement::LetBinding { name, value } => {
                    let value = self.eval(value)?;
                    self.bindings.insert(name.name, value);
                }
            }
        }
        self.cell.solve()
    }

    fn eval(&mut self, expr: &Expr<'a>) -> Result<Value<'a>> {
        match expr {
            Expr::BinOp(expr) => {
                let left = self.eval(&expr.left)?.try_linear(expr.left.span())?;
                let right = self.eval(&expr.right)?.try_linear(expr.right.span())?;
                match expr.op {
                    BinOp::Add => Ok(Value::Linear(left + right)),
                    BinOp::Sub => Ok(Value::Linear(left - right)),
                    op => bail!(
                        "unsupported binary operator: {op:?} in expression at {:?}",
                        expr.span
                    ),
                }
            }
            Expr::Call(expr) => match expr.func.name {
                "Rect" => {
                    assert_eq!(expr.args.posargs.len(), 1);
                    let layer = self
                        .eval(&expr.args.posargs[0])?
                        .try_enum_value(expr.args.posargs[0].span())?;
                    let attrs = Attrs {
                        source: Some(SourceInfo {
                            span: expr.span,
                            id: self.alloc_id(),
                        }),
                    };
                    let rect = self.cell.physical_rect(layer.variant.name.into(), attrs);
                    for arg in expr.args.kwargs.iter() {
                        let value = self.eval(&arg.value)?;
                        match arg.name.name {
                            "x0" => {
                                let mut value = value.try_linear(arg.span)?;
                                value.coeffs.push((-1., rect.x0));
                                self.cell.add_constraint(Constraint::Linear(
                                    value.into_eq_constraint(ConstraintAttrs {
                                        span: Some(arg.span),
                                    }),
                                ));
                            }
                            "x1" => {
                                let mut value = value.try_linear(arg.span)?;
                                value.coeffs.push((-1., rect.x1));
                                self.cell.add_constraint(Constraint::Linear(
                                    value.into_eq_constraint(ConstraintAttrs {
                                        span: Some(arg.span),
                                    }),
                                ));
                            }
                            "y0" => {
                                let mut value = value.try_linear(arg.span)?;
                                value.coeffs.push((-1., rect.y0));
                                self.cell.add_constraint(Constraint::Linear(
                                    value.into_eq_constraint(ConstraintAttrs {
                                        span: Some(arg.span),
                                    }),
                                ));
                            }
                            "y1" => {
                                let mut value = value.try_linear(arg.span)?;
                                value.coeffs.push((-1., rect.y1));
                                self.cell.add_constraint(Constraint::Linear(
                                    value.into_eq_constraint(ConstraintAttrs {
                                        span: Some(arg.span),
                                    }),
                                ));
                            }
                            arg_name => {
                                bail!("unexpected argument: `{arg_name}` at {:?}", arg.name.span)
                            }
                        }
                    }
                    Ok(Value::Rect(rect))
                }
                "eq" => {
                    assert_eq!(expr.args.posargs.len(), 2);
                    let lhs = self
                        .eval(&expr.args.posargs[0])?
                        .try_linear(expr.args.posargs[0].span())?;
                    let rhs = self
                        .eval(&expr.args.posargs[1])?
                        .try_linear(expr.args.posargs[0].span())?;
                    self.cell
                        .add_constraint(Constraint::Linear((lhs - rhs).into_eq_constraint(
                            ConstraintAttrs {
                                span: Some(expr.span),
                            },
                        )));
                    Ok(Value::None)
                }
                f => bail!("unexpected draw call `{f}` at {:?}", expr.span),
            },
            Expr::FloatLiteral(v) => Ok(Value::Linear(LinearExpr {
                constant: v.value,
                coeffs: Vec::new(),
            })),
            Expr::Var(v) => Ok(self
                .bindings
                .get(v.name)
                .ok_or_else(|| anyhow!("no variable named `{}`", v.name))?
                .clone()),
            Expr::FieldAccess(expr) => {
                let base = self.eval(&expr.base)?;
                match base {
                    Value::Rect(r) => Ok(Value::Linear(LinearExpr::from(match expr.field.name {
                        "x0" => r.x0,
                        "x1" => r.x1,
                        "y0" => r.y0,
                        "y1" => r.y1,
                        f => bail!(
                            "type Rect has no field `{f}` (encountered at {:?})",
                            expr.field.span
                        ),
                    }))),
                    _ => bail!(
                        "object no field `{}` (encountered at {:?})",
                        expr.field.name,
                        expr.field.span
                    ),
                }
            }
            Expr::EnumValue(v) => Ok(Value::EnumValue(v.clone())),
            Expr::Emit(v) => {
                let value = self.eval(&v.value)?;
                let rect = value.try_rect(v.span)?;
                self.cell.emit_rect(rect.clone());
                Ok(Value::Rect(rect))
            }
            expr => bail!("cannot evaluate the expression at {:?}", expr.span()),
        }
    }
}

#[enumify]
#[derive(Debug, Clone)]
pub enum Value<'a> {
    EnumValue(EnumValue<'a>),
    Linear(LinearExpr),
    Rect(Rect<Var>),
    None,
}

impl<'a> Value<'a> {
    pub fn try_enum_value(self, espan: cfgrammar::Span) -> Result<EnumValue<'a>> {
        self.into_enum_value()
            .ok_or_else(|| anyhow!("expected value to be of type EnumValue at {espan:?}"))
    }
    pub fn try_linear(self, espan: cfgrammar::Span) -> Result<LinearExpr> {
        self.into_linear()
            .ok_or_else(|| anyhow!("expected value to be of type LinearExpr at {espan:?}"))
    }
    pub fn try_rect(self, espan: cfgrammar::Span) -> Result<Rect<Var>> {
        self.into_rect()
            .ok_or_else(|| anyhow!("expected value to be of type Rect at {espan:?}"))
    }
}

#[derive(Debug, Clone)]
pub struct LinearExpr {
    coeffs: Vec<(f64, Var)>,
    constant: f64,
}

impl std::ops::Add<LinearExpr> for LinearExpr {
    type Output = Self;
    fn add(self, rhs: LinearExpr) -> Self::Output {
        Self {
            coeffs: self.coeffs.into_iter().chain(rhs.coeffs).collect(),
            constant: self.constant + rhs.constant,
        }
    }
}

impl std::ops::Sub<LinearExpr> for LinearExpr {
    type Output = Self;
    fn sub(self, rhs: LinearExpr) -> Self::Output {
        Self {
            coeffs: self
                .coeffs
                .into_iter()
                .chain(rhs.coeffs.into_iter().map(|(c, v)| (-c, v)))
                .collect(),
            constant: self.constant - rhs.constant,
        }
    }
}

impl LinearExpr {
    pub fn into_eq_constraint(self, attrs: ConstraintAttrs) -> crate::solver::LinearConstraint {
        crate::solver::LinearConstraint {
            coeffs: self.coeffs.into_iter().map(|(k, v)| (k, v)).collect(),
            constant: self.constant,
            is_equality: true,
            attrs,
        }
    }
}

impl From<Var> for LinearExpr {
    fn from(value: Var) -> Self {
        Self {
            coeffs: vec![(1., value)],
            constant: 0.,
        }
    }
}

pub fn compile(input: CompileInput) -> Result<SolvedCell> {
    let ctx = CellCtx::new();
    ctx.compile(input)
}
