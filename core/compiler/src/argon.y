%start Ast
%%
Ast -> Result<Ast<'input, ParseMetadata>, ()>:
  Decls {
    Ok(Ast {
      decls: $1?,
      span: $span,
    })
  };

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
  | StructDecl { Ok(Decl::Struct($1?)) }
  | CellDecl { Ok(Decl::Cell($1?)) }
  | FnDecl { Ok(Decl::Fn($1?)) }
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

IntLiteral -> Result<IntLiteral, ()>
  : 'INTLIT' {
  let v = $1.map_err(|_| ())?;
  Ok(IntLiteral { span: v.span(), value: parse_int($lexer.span_str(v.span()))?, }) }
  ;

StringLiteral -> Result<StringLiteral, ()>
  : 'STRLIT' {
  let v = $1.map_err(|_| ())?;
  Ok(StringLiteral { span: v.span(), value: parse_str($lexer.span_str(v.span()))?, }) }
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

StructDecl -> Result<StructDecl<'input, ParseMetadata>, ()>
  : 'STRUCT' Ident '{' StructFields '}'
  {
    Ok(StructDecl {
      name: $2?,
      fields: $4?,
      span: $span,
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

StructFields -> Result<Vec<StructField<'input, ParseMetadata>>, ()>:
  StructFields StructField ',' {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

StructField -> Result<StructField<'input, ParseMetadata>, ()>
  : Ident ':' Ident
  {
    Ok(StructField {
        name: $1?,
        ty: $3?,
        span: $span,
        metadata: (),
    })
  }
  ;

CellDecl -> Result<CellDecl<'input, ParseMetadata>, ()>
  : 'CELL' Ident '(' ArgDecls ')' Scope
  {
    Ok(CellDecl {
      name: $2?,
      args: $4?,
      scope: $6?,
      span: $span,
      metadata: (),
    })
  }
  ;

FnDecl -> Result<FnDecl<'input, ParseMetadata>, ()>
  : 'FN' Ident '(' ArgDecls ')' '->' Ident Scope
  {
    Ok(FnDecl {
      name: $2?,
      args: $4?,
      scope: $8?,
      return_ty: Some($7?),
      span: $span,
      metadata: (),
    })
  }
  | 'FN' Ident '(' ArgDecls ')' Scope
  {
    Ok(FnDecl {
      name: $2?,
      args: $4?,
      scope: $6?,
      return_ty: None,
      span: $span,
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
    Ok(Statement::LetBinding(LetBinding {
      name: $2?,
      value: $4?,
      span: $span,
      metadata: (),
    }))
  }
  | BlockExpr
  {
    Ok(Statement::Expr { value: $1?, semicolon: false, })
  }
  ;

ScopeAnnotation -> Result<Ident<'input, ParseMetadata>, ()>
    : 'ANNOTATION' { Ok(Ident { span: $span, name: &$lexer.span_str($span)[1..], metadata: () }) }
    ;

UnannotatedScope -> Result<Scope<'input, ParseMetadata>, ()>
  : '{' Statements '}'
  {
    let mut __stmts = $2?;
    if let Some(Statement::Expr { value, semicolon }) = __stmts.last().cloned() && !semicolon {
      __stmts.pop().unwrap();
      return Ok(Scope {
        scope_annotation: None,
        span: $span,
        stmts: __stmts,
        tail: Some(value),
        metadata: (),
      })
    }
    Ok(Scope {
      scope_annotation: None,
      span: $span,
      stmts: __stmts,
      tail: None,
      metadata: (),
    })
  }
  | '{' Statements NonBlockExpr '}'
  {
    Ok(Scope {
      scope_annotation: None,
      span: $span,
      stmts: $2?,
      tail: Some($3?),
      metadata: (),
    })
  }
  ;

Scope -> Result<Scope<'input, ParseMetadata>, ()>
    : ScopeAnnotation UnannotatedScope
    {
        Ok(Scope {
            scope_annotation: Some($1?),
            ..$2?
        })
    }
    | UnannotatedScope 
    {
        $1
    }
    ;

Expr -> Result<Expr<'input, ParseMetadata>, ()>
  : NonBlockExpr { $1 }
  | BlockExpr { $1 }
  ;

BlockExpr -> Result<Expr<'input, ParseMetadata>, ()>
  : 'IF' Expr Scope 'ELSE' Scope { Ok(Expr::If(Box::new(IfExpr { scope_annotation: None, cond: $2?, then: $3?, else_: $5?, span: $span, metadata: (), }))) }
  | ScopeAnnotation 'IF' Expr Scope 'ELSE' Scope { Ok(Expr::If(Box::new(IfExpr { scope_annotation: Some($1?), cond: $3?, then: $4?, else_: $6?, span: $span, metadata: (), }))) }
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
  : '!' Factor { Ok(Expr::UnaryOp(Box::new(UnaryOpExpr { op: UnaryOp::Not, operand: $2?, span: $span, metadata: () }))) }
  | '-' Factor { Ok(Expr::UnaryOp(Box::new(UnaryOpExpr { op: UnaryOp::Neg, operand: $2?, span: $span, metadata: () }))) }
  | SubFactor { $1 }
  ;

SubFactor -> Result<Expr<'input, ParseMetadata>, ()>
  : '(' Expr ')' { $2 }
  | CallExpr { Ok(Expr::Call($1?)) }
  | SubFactor '.' Ident { Ok(Expr::FieldAccess(Box::new(FieldAccessExpr { base: $1?, field: $3?, span: $span, metadata: (), }))) }
  | SubFactor '!' { Ok(Expr::Emit(Box::new(EmitExpr { value: $1?, span: $span, metadata: (), }))) }
  | Ident '::' Ident { Ok(Expr::EnumValue(EnumValue {name: $1?, variant: $3?, span: $span, metadata: (), } )) }
  | Ident { Ok(Expr::Var(VarExpr { name: $1?, metadata: (), })) }
  | IntLiteral { Ok(Expr::IntLiteral($1?)) }
  | FloatLiteral { Ok(Expr::FloatLiteral($1?)) }
  | StringLiteral { Ok(Expr::StringLiteral($1?)) }
  | SubFactor 'AS' Ident { Ok(Expr::Cast(Box::new(CastExpr { value: $1?, ty: $3?, span: $span, metadata: (), }))) }
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
  : Ident ':' Ident { Ok(ArgDecl { name: $1?, ty: $3?, metadata: () }) }
  ;

Args -> Result<Args<'input, ParseMetadata>, ()>
  : PosArgsTrailingComma KwArgs { Ok(Args { posargs: $1?, kwargs: $2?, metadata: (), }) }
  | KwArgs { Ok(Args { posargs: Vec::new(), kwargs: $1?, metadata: (), }) }
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

use crate::ast::*;
use crate::parse::ParseMetadata;
