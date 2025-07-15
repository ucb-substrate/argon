%start Decls
%%
Decls -> Result<Vec<Decl<'input, ParseMetadata>>, ()>:
  Decls Decl {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

Decl -> Result<Decl<'input, ParseMetadata>, ()>
  : EnumDecl { Ok(Decl::Enum($1?)) }
  | CellDecl { Ok(Decl::Cell($1?)) }
  | ConstantDecl { Ok(Decl::Constant($1?)) }
  ;

Ident -> Result<Ident<'input, ParseMetadata>, ()>
  : 'IDENT' { Ok(Ident { span: $span, name: $lexer.span_str($span), metadata: () }) }
  ;

FloatLiteral -> Result<FloatLiteral, ()>
  : 'FLOATLIT' {
  let v = $1.map_err(|_| ())?;
  Ok(FloatLiteral { span: v.span(), value: parse_float($lexer.span_str(v.span()))?, }) }
  ;

EnumDecl -> Result<EnumDecl<'input, ParseMetadata>, ()>
  : 'ENUM' Ident '{' EnumVariants '}'
  {
    Ok(EnumDecl {
      name: $2?,
      variants: $4?,
      metadata: (),
    })
  }
  ;

ConstantDecl -> Result<ConstantDecl<'input, ParseMetadata>, ()>
  : 'CONST' Ident ':' Ident '=' Expr ';'
  {
    Ok(ConstantDecl {
      name: $2?,
      ty: $4?,
      value: $6?,
      metadata: (),
    })
  }
  ;

EnumVariants -> Result<Vec<Ident<'input, ParseMetadata>>, ()>:
  EnumVariants Ident ',' {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

CellDecl -> Result<CellDecl<'input, ParseMetadata>, ()>
  : 'CELL' Ident '(' ArgDecls ')' '{' Statements '}'
  {
    Ok(CellDecl {
      name: $2?,
      args: $4?,
      stmts: $7?,
      metadata: (),
    })
  }
  ;

Statements -> Result<Vec<Statement<'input, ParseMetadata>>, ()>:
  Statements Statement {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

Statement -> Result<Statement<'input, ParseMetadata>, ()>
  : Expr ';'
  {
    Ok(Statement::Expr { value: $1?, semicolon: true, })
  }
  | 'LET' Ident '=' Expr ';'
  {
    Ok(Statement::LetBinding {
      name: $2?,
      value: $4?,
    })
  }
  | BlockExpr
  {
    Ok(Statement::Expr { value: $1?, semicolon: false, })
  }
  ;

Scope -> Result<Scope<'input, ParseMetadata>, ()>
  : '{' Statements '}'
  {
    let mut __stmts = $2?;
    if let Some(Statement::Expr { value, semicolon }) = __stmts.last().cloned() {
      if !semicolon {
        __stmts.pop().unwrap();
        return Ok(Scope {
          span: $span,
          stmts: __stmts,
          tail: Some(value),
        })
      }
    }
    Ok(Scope {
      span: $span,
      stmts: __stmts,
      tail: None,
    })
  }
  | '{' Statements NonBlockExpr '}'
  {
    Ok(Scope {
      span: $span,
      stmts: $2?,
      tail: Some($3?),
    })
  }
  ;

Expr -> Result<Expr<'input, ParseMetadata>, ()>
  : NonBlockExpr { $1 }
  | BlockExpr { $1 }
  ;

BlockExpr -> Result<Expr<'input, ParseMetadata>, ()>
  : 'IF' Expr '{' Expr '}' 'ELSE' '{' Expr '}' { Ok(Expr::If(Box::new(IfExpr { cond: $2?, then: $4?, else_: $8?, span: $span, metadata: (), }))) }
  | Scope { Ok(Expr::Scope(Box::new($1?))) }
  ;

NonBlockExpr -> Result<Expr<'input, ParseMetadata>, ()>
  : NonBlockExpr '==' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Eq, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '!=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Ne, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '>=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Geq, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '>' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Gt, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '<=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Leq, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '<' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Lt, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonComparisonExpr { $1 }
  ;

NonComparisonExpr -> Result<Expr<'input, ParseMetadata>, ()>
  : NonComparisonExpr '+' Term { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Add, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonComparisonExpr '-' Term { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Sub, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | Term { $1 }
  ;

Term -> Result<Expr<'input, ParseMetadata>, ()>
  : Term '*' Factor { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Mul, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | Term '/' Factor { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Div, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | Factor { $1 }
  ;

Factor -> Result<Expr<'input, ParseMetadata>, ()>
  : '(' Expr ')' { $2 }
  | Factor '.' Ident { Ok(Expr::FieldAccess(Box::new(FieldAccessExpr { base: $1?, field: $3?, span: $span, metadata: (), }))) }
  | CallExpr { Ok(Expr::Call($1?)) }
  | Factor '!' { Ok(Expr::Emit(Box::new(EmitExpr { value: $1?, span: $span, metadata: (), }))) }
  | Ident '::' Ident { Ok(Expr::EnumValue(EnumValue {name: $1?, variant: $3?, span: $span, metadata: (), } )) }
  | Ident { Ok(Expr::Var(VarExpr { name: $1?, metadata: (), })) }
  | FloatLiteral { Ok(Expr::FloatLiteral($1?)) }
  ;


CallExpr -> Result<CallExpr<'input, ParseMetadata>, ()>
  : Ident '(' Args ')'
    {
      Ok(CallExpr {
        func: $1?,
        args: $3?,
        span: $span,
        metadata: (),
      })
    }
  ;

ArgDecls -> Result<Vec<ArgDecl<'input, ParseMetadata>>, ()>
  : { Ok(Vec::new()) }
  | ArgDecls1 { $1 }
  | ArgDecls1 ',' { $1 }
  ;

ArgDecls1 -> Result<Vec<ArgDecl<'input, ParseMetadata>>, ()>
  : ArgDecls1 ',' ArgDecl { flatten($1, $3) }
  | ArgDecl { Ok(vec![$1?]) }
  ;

ArgDecl -> Result<ArgDecl<'input, ParseMetadata>, ()>
  : Ident ':' Typ { Ok(ArgDecl { name: $1?, ty: $3?, metadata: () }) }
  ;

Typ -> Result<Typ<'input, ParseMetadata>, ()>
  : 'FLOAT' { Ok(Typ::Float) }
  | Ident { Ok(Typ::Ident($1?)) }
  ;

Args -> Result<Args<'input, ParseMetadata>, ()>
  : PosArgsTrailingComma KwArgs { Ok(Args { posargs: $1?, kwargs: $2?, metadata: (), }) }
  | PosArgs { Ok(Args { posargs: $1?, kwargs: Vec::new(), metadata: (), }) }
  ;

KwArgValue -> Result<KwArgValue<'input, ParseMetadata>, ()>
  : Ident '=' Expr
      {
        Ok(KwArgValue {
          name: $1?,
          value: $3?,
          span: $span,
          metadata: ()
        })
      }
  ;

KwArgs -> Result<Vec<KwArgValue<'input, ParseMetadata>>, ()>
  : KwArgsTrailingComma { $1 }
  | KwArgsNoComma { $1 }
  ;

KwArgsTrailingComma -> Result<Vec<KwArgValue<'input, ParseMetadata>>, ()>
  : KwArgValue ',' { Ok(vec![$1?]) }
  | KwArgsTrailingComma KwArgValue ',' {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
    }
  ;

KwArgsNoComma -> Result<Vec<KwArgValue<'input, ParseMetadata>>, ()>
  : KwArgValue { Ok(vec![$1?]) }
  | KwArgsTrailingComma KwArgValue {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
  }
  ;


PosArgs -> Result<Vec<Expr<'input, ParseMetadata>>, ()>
  : PosArgsTrailingComma { $1 }
  | PosArgsNoComma { $1 }
  ;

PosArgsTrailingComma -> Result<Vec<Expr<'input, ParseMetadata>>, ()>
  : Expr ',' { Ok(vec![$1?]) }
  | PosArgsTrailingComma Expr ',' {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
  }
  ;

PosArgsNoComma -> Result<Vec<Expr<'input, ParseMetadata>>, ()>
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
use crate::ast::*;
use crate::parse::ParseMetadata;
