%start Decls
%%
Decls -> Result<Vec<Decl<'input>>, ()>:
  Decls Decl {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

Decl -> Result<Decl<'input>, ()>
  : EnumDecl { Ok(Decl::Enum($1?)) }
  | CellDecl { Ok(Decl::Cell($1?)) }
  | ConstantDecl { Ok(Decl::Constant($1?)) }
  ;

Ident -> Result<Ident<'input>, ()>
  : 'IDENT' { Ok(Ident { span: $span, name: $lexer.span_str($span), id: None }) }
  ;

FloatLiteral -> Result<FloatLiteral, ()>
  : 'FLOATLIT' {
  let v = $1.map_err(|_| ())?;
  Ok(FloatLiteral { span: v.span(), value: parse_float($lexer.span_str(v.span()))?, }) }
  ;

EnumDecl -> Result<EnumDecl<'input>, ()>
  : 'ENUM' Ident '{' EnumVariants '}'
  {
    Ok(EnumDecl {
      name: $2?,
      variants: $4?,
    })
  }
  ;

ConstantDecl -> Result<ConstantDecl<'input>, ()>
  : 'CONST' Ident ':' Ident '=' Expr ';'
  {
    Ok(ConstantDecl {
      name: $2?,
      ty: $4?,
      value: $6?,
    })
  }
  ;

EnumVariants -> Result<Vec<Ident<'input>>, ()>:
  EnumVariants Ident ',' {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

CellDecl -> Result<CellDecl<'input>, ()>
  : 'CELL' Ident '(' ArgDecls ')' '{' Statements '}'
  {
    Ok(CellDecl {
      name: $2?,
      args: $4?,
      stmts: $7?,
    })
  }
  ;

Statements -> Result<Vec<Statement<'input>>, ()>:
  Statements Statement {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

Statement -> Result<Statement<'input>, ()>
  : Expr ';'
  {
    Ok(Statement::Expr($1?))
  }
  | 'LET' Ident '=' Expr ';'
  {
    Ok(Statement::LetBinding {
      name: $2?,
      value: $4?,
    })
  }
  ;

Expr -> Result<Expr<'input>, ()>
  : Expr '==' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Eq, left: $1?, right: $3?, span: $span, }))) }
  | Expr '!=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Ne, left: $1?, right: $3?, span: $span, }))) }
  | Expr '>=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Geq, left: $1?, right: $3?, span: $span, }))) }
  | Expr '>' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Gt, left: $1?, right: $3?, span: $span, }))) }
  | Expr '<=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Leq, left: $1?, right: $3?, span: $span, }))) }
  | Expr '<' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Lt, left: $1?, right: $3?, span: $span, }))) }
  | 'IF' Expr '{' Expr '}' 'ELSE' '{' Expr '}' { Ok(Expr::If(Box::new(IfExpr { cond: $2?, left: $4?, right: $8?, span: $span, }))) }
  | NonComparisonExpr { $1 }
  ;

NonComparisonExpr -> Result<Expr<'input>, ()>
  : NonComparisonExpr '+' Term { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Add, left: $1?, right: $3?, span: $span, }))) }
  | NonComparisonExpr '-' Term { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Sub, left: $1?, right: $3?, span: $span, }))) }
  | Term { $1 }
  ;

Term -> Result<Expr<'input>, ()>
  : Term '*' Factor { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Mul, left: $1?, right: $3?, span: $span, }))) }
  | Term '/' Factor { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Div, left: $1?, right: $3?, span: $span, }))) }
  | Factor { $1 }
  ;

Factor -> Result<Expr<'input>, ()>
  : '(' Expr ')' { $2 }
  | Factor '.' Ident { Ok(Expr::FieldAccess(Box::new(FieldAccessExpr { base: $1?, field: $3?, span: $span, }))) }
  | CallExpr { Ok(Expr::Call($1?)) }
  | Factor '!' { Ok(Expr::Emit(Box::new(EmitExpr { value: $1?, span: $span, }))) }
  | Ident '::' Ident { Ok(Expr::EnumValue(EnumValue {name: $1?, variant: $3?, span: $span, } )) }
  | Ident { Ok(Expr::Var($1?)) }
  | FloatLiteral { Ok(Expr::FloatLiteral($1?)) }
  ;


CallExpr -> Result<CallExpr<'input>, ()>
  : Ident '(' Args ')'
    {
      Ok(CallExpr {
        func: $1?,
        args: $3?,
        span: $span,
      })
    }
  ;

ArgDecls -> Result<Vec<ArgDecl<'input>>, ()>
  : { Ok(Vec::new()) }
  | ArgDecls1 { $1 }
  | ArgDecls1 ',' { $1 }
  ;

ArgDecls1 -> Result<Vec<ArgDecl<'input>>, ()>
  : ArgDecls1 ',' ArgDecl { flatten($1, $3) }
  | ArgDecl { Ok(vec![$1?]) }
  ;

ArgDecl -> Result<ArgDecl<'input>, ()>
  : Ident ':' Typ { Ok(ArgDecl { name: $1?, ty: $3? }) }
  ;

Typ -> Result<Typ<'input>, ()>
  : 'FLOAT' { Ok(Typ::Float) }
  | Ident { Ok(Typ::Ident($1?)) }
  ;

Args -> Result<Args<'input>, ()>
  : PosArgsTrailingComma KwArgs { Ok(Args { posargs: $1?, kwargs: $2? }) }
  | PosArgs { Ok(Args { posargs: $1?, kwargs: Vec::new() }) }
  ;

KwArgValue -> Result<KwArgValue<'input>, ()>
  : Ident '=' Expr
      {
        Ok(KwArgValue {
          name: $1?,
          value: $3?,
          span: $span,
        })
      }
  ;

KwArgs -> Result<Vec<KwArgValue<'input>>, ()>
  : KwArgsTrailingComma { $1 }
  | KwArgsNoComma { $1 }
  ;

KwArgsTrailingComma -> Result<Vec<KwArgValue<'input>>, ()>
  : KwArgValue ',' { Ok(vec![$1?]) }
  | KwArgsTrailingComma KwArgValue ',' {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
    }
  ;

KwArgsNoComma -> Result<Vec<KwArgValue<'input>>, ()>
  : KwArgValue { Ok(vec![$1?]) }
  | KwArgsTrailingComma KwArgValue {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
  }
  ;


PosArgs -> Result<Vec<Expr<'input>>, ()>
  : PosArgsTrailingComma { $1 }
  | PosArgsNoComma { $1 }
  ;

PosArgsTrailingComma -> Result<Vec<Expr<'input>>, ()>
  : Expr ',' { Ok(vec![$1?]) }
  | PosArgsTrailingComma Expr ',' {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
  }
  ;

PosArgsNoComma -> Result<Vec<Expr<'input>>, ()>
  : { Ok(Vec::new()) }
  | Expr { Ok(vec![$1?]) }
  | PosArgsTrailingComma Expr
    {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
    }
  ;
%%

use cfgrammar::Span;

#[derive(Debug, Clone)]
pub enum Decl<'a> {
  Enum(EnumDecl<'a>),
  Constant(ConstantDecl<'a>),
  Cell(CellDecl<'a>),
}

#[derive(Debug, Clone)]
pub struct Ident<'a> {
  pub span: Span,
  pub name: &'a str,
  pub id: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct FloatLiteral {
  span: Span,
  pub value: f64,
}

#[derive(Debug, Clone)]
pub struct EnumDecl<'a> {
  pub name: Ident<'a>,
  pub variants: Vec<Ident<'a>>,
}

#[derive(Debug, Clone)]
pub struct CellDecl<'a> {
  pub name: Ident<'a>,
  pub args: Vec<ArgDecl<'a>>,
  pub stmts: Vec<Statement<'a>>,
}

#[derive(Debug, Clone)]
pub struct ConstantDecl<'a> {
  pub name: Ident<'a>,
  pub ty: Ident<'a>,
  pub value: Expr<'a>,
}

#[derive(Debug, Clone)]
pub enum Statement<'a> {
  Expr(Expr<'a>),
  LetBinding {
    name: Ident<'a>,
    value: Expr<'a>,
  },
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum BinOp {
  Add,
  Sub,
  Mul,
  Div,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ComparisonOp {
  Eq,
  Ne,
  Geq,
  Gt,
  Leq,
  Lt,
}

#[derive(Debug, Clone)]
pub enum Expr<'a> {
  If(Box<IfExpr<'a>>),
  Comparison(Box<ComparisonExpr<'a>>),
  BinOp(Box<BinOpExpr<'a>>),
  Call(CallExpr<'a>),
  Emit(Box<EmitExpr<'a>>),
  EnumValue(EnumValue<'a>),
  FieldAccess(Box<FieldAccessExpr<'a>>),
  Var(Ident<'a>),
  FloatLiteral(FloatLiteral),
}

#[derive(Debug, Clone)]
pub struct IfExpr<'a> {
    pub cond: Expr<'a>,
    pub left: Expr<'a>,
    pub right: Expr<'a>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BinOpExpr<'a> {
    pub op: BinOp,
    pub left: Expr<'a>,
    pub right: Expr<'a>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ComparisonExpr<'a> {
    pub op: ComparisonOp,
    pub left: Expr<'a>,
    pub right: Expr<'a>,
    pub span: Span,
}


#[derive(Debug, Clone)]
pub struct FieldAccessExpr<'a> {
    pub base: Expr<'a>,
    pub field: Ident<'a>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumValue<'a> {
    pub name: Ident<'a>,
    pub variant: Ident<'a>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct CallExpr<'a> {
  pub func: Ident<'a>,
  pub args: Args<'a>,
  pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EmitExpr<'a> {
  pub value: Expr<'a>,
  pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Args<'a> {
  pub posargs: Vec<Expr<'a>>,
  pub kwargs: Vec<KwArgValue<'a>>,
}

#[derive(Debug, Clone)]
pub struct KwArgValue<'a> {
  pub name: Ident<'a>,
  pub value: Expr<'a>,
  pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ArgDecl<'a> {
  pub name: Ident<'a>,
  pub ty: Typ<'a>,
}

#[derive(Debug, Clone)]
pub enum Typ<'a> {
  Float,
  Ident(Ident<'a>),
}

fn parse_float(s: &str) -> Result<f64, ()> {
    match s.parse::<f64>() {
        Ok(val) => Ok(val),
        Err(_) => {
            Err(())
        }
    }
}

fn flatten<T>(lhs: Result<Vec<T>, ()>, rhs: Result<T, ()>)
           -> Result<Vec<T>, ()>
{
    let mut flt = lhs?;
    flt.push(rhs?);
    Ok(flt)
}

impl<'a> Expr<'a> {
    pub fn span(&self) -> Span {
        match self {
            Self::If(x) => x.span,
            Self::Comparison(x) => x.span,
            Self::BinOp(x) => x.span,
            Self::Call(x) => x.span,
            Self::Emit(x) => x.span,
            Self::EnumValue(x) => x.span,
            Self::FieldAccess(x) => x.span,
            Self::Var(x) => x.span,
            Self::FloatLiteral(x) => x.span,
        }
    }
}
