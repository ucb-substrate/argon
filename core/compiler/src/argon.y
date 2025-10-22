%start Ast
%%
Ast -> Result<Ast<&'input str, ParseMetadata>, ()>:
  Decls {
    Ok(Ast {
      decls: $1?,
      span: $span,
    })
  };

Decls -> Result<Vec<Decl<&'input str, ParseMetadata>>, ()>:
  Decls Decl {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

Decl -> Result<Decl<&'input str, ParseMetadata>, ()>
  : EnumDecl { Ok(Decl::Enum($1?)) }
  | StructDecl { Ok(Decl::Struct($1?)) }
  | CellDecl { Ok(Decl::Cell($1?)) }
  | FnDecl { Ok(Decl::Fn($1?)) }
  | ConstantDecl { Ok(Decl::Constant($1?)) }
  | ModDecl { Ok(Decl::Mod($1?)) }
  ;

Ident -> Result<Ident<&'input str, ParseMetadata>, ()>
  : 'IDENT' { 
  let _ = $1.map_err(|_| ())?;
  Ok(Ident { span: $span, name: $lexer.span_str($span), metadata: () })
  }
  ;

IdentPath -> Result<IdentPath<&'input str, ParseMetadata>, ()>
  : Ident { Ok(IdentPath { path: vec![$1?], metadata: (), span: $span }) }
  | Ident '::' IdentPath { 
    let mut path = vec![$1?];
    path.extend($3?.path);
    Ok(IdentPath { path, metadata: (), span: $span })
  }
  ;

NilLiteral -> Result<NilLiteral, ()>
  : 'NIL' {
  let v = $1.map_err(|_| ())?;
  Ok(NilLiteral { span: v.span(), }) }
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

StringLiteral -> Result<StringLiteral<&'input str>, ()>
  : 'STRLIT' {
  let v = $1.map_err(|_| ())?;
  Ok(StringLiteral { span: v.span(), value: $lexer.span_str(v.span()).trim_matches('"'), }) }
  ;

BoolLiteral -> Result<BoolLiteral, ()>
  : 'TRUE' {
  let v = $1.map_err(|_| ())?;
  Ok(BoolLiteral { span: v.span(), value: true, })
  }
  | 'FALSE' {
  let v = $1.map_err(|_| ())?;
  Ok(BoolLiteral { span: v.span(), value: false, })
  }
  ;

EnumDecl -> Result<EnumDecl<&'input str, ParseMetadata>, ()>
  : 'ENUM' Ident '{' EnumVariants '}'
  {
    Ok(EnumDecl {
      name: $2?,
      variants: $4?,
      metadata: (),
    })
  }
  ;

StructDecl -> Result<StructDecl<&'input str, ParseMetadata>, ()>
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

ConstantDecl -> Result<ConstantDecl<&'input str, ParseMetadata>, ()>
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

ModDecl -> Result<ModDecl<&'input str, ParseMetadata>, ()>
  : 'MOD' Ident ';'
  {
    Ok(ModDecl {
      ident: $2?,
      span: $span,
    })
  }
  ;

EnumVariants -> Result<Vec<Ident<&'input str, ParseMetadata>>, ()>:
  EnumVariants Ident ',' {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

StructFields -> Result<Vec<StructField<&'input str, ParseMetadata>>, ()>:
  StructFields StructField ',' {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

StructField -> Result<StructField<&'input str, ParseMetadata>, ()>
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

CellDecl -> Result<CellDecl<&'input str, ParseMetadata>, ()>
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

FnDecl -> Result<FnDecl<&'input str, ParseMetadata>, ()>
  : 'FN' Ident '(' ArgDecls ')' '->' TySpec Scope
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

Statements -> Result<Vec<Statement<&'input str, ParseMetadata>>, ()>:
  Statements Statement {
    let mut __tmp = $1?;
    __tmp.push($2?);
    Ok(__tmp)
  }
  | { Ok(Vec::new()) }
  ;

Statement -> Result<Statement<&'input str, ParseMetadata>, ()>
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

ScopeAnnotation -> Result<Ident<&'input str, ParseMetadata>, ()>
    : 'ANNOTATION' 
    { 
        Ok(Ident {
            span: cfgrammar::Span::new(
                $span.start() + 1,
                $span.end(),
            ),
            name: &$lexer.span_str($span)[1..],
            metadata: ()
        })
    }
    ;

UnannotatedScope -> Result<Scope<&'input str, ParseMetadata>, ()>
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

Scope -> Result<Scope<&'input str, ParseMetadata>, ()>
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

Expr -> Result<Expr<&'input str, ParseMetadata>, ()>
  : NonBlockExpr { $1 }
  | BlockExpr { $1 }
  ;

BlockExpr -> Result<Expr<&'input str, ParseMetadata>, ()>
  : 'IF' Expr Scope 'ELSE' Scope { Ok(Expr::If(Box::new(IfExpr { scope_annotation: None, cond: $2?, then: $3?, else_: $5?, span: $span, metadata: (), }))) }
  | ScopeAnnotation 'IF' Expr Scope 'ELSE' Scope { Ok(Expr::If(Box::new(IfExpr { scope_annotation: Some($1?), cond: $3?, then: $4?, else_: $6?, span: $span, metadata: (), }))) }
  | MatchExpr { Ok(Expr::Match(Box::new($1?))) }
  | Scope { Ok(Expr::Scope(Box::new($1?))) }
  ;

MatchExpr -> Result<MatchExpr<&'input str, ParseMetadata>, ()>
  : 'MATCH' Expr '{' MatchArms '}' { Ok(MatchExpr { scrutinee: $2?, arms: $4?, span: $span, metadata: () }) }
  ;

MatchArms -> Result<Vec<MatchArm<&'input str, ParseMetadata>>, ()>
  : MatchArm { Ok(vec![$1?]) }
  | MatchArms MatchArm {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
  }
  ;

MatchArm -> Result<MatchArm<&'input str, ParseMetadata>, ()>
  : IdentPath '=>' Expr ',' { Ok(MatchArm { pattern: $1?, expr: $3?, span: $span, }) }
  ;

NonBlockExpr -> Result<Expr<&'input str, ParseMetadata>, ()>
  : NonBlockExpr '==' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Eq, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '!=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Ne, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '>=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Geq, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '>' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Gt, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '<=' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Leq, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonBlockExpr '<' NonComparisonExpr { Ok(Expr::Comparison(Box::new(ComparisonExpr { op: ComparisonOp::Lt, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonComparisonExpr { $1 }
  ;

NonComparisonExpr -> Result<Expr<&'input str, ParseMetadata>, ()>
  : NonComparisonExpr '+' Term { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Add, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | NonComparisonExpr '-' Term { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Sub, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | Term { $1 }
  ;

Term -> Result<Expr<&'input str, ParseMetadata>, ()>
  : Term '*' Factor { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Mul, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | Term '/' Factor { Ok(Expr::BinOp(Box::new(BinOpExpr { op: BinOp::Div, left: $1?, right: $3?, span: $span, metadata: (), }))) }
  | Factor { $1 }
  ;

Factor -> Result<Expr<&'input str, ParseMetadata>, ()>
  : '!' Factor { Ok(Expr::UnaryOp(Box::new(UnaryOpExpr { op: UnaryOp::Not, operand: $2?, span: $span, metadata: () }))) }
  | '-' Factor { Ok(Expr::UnaryOp(Box::new(UnaryOpExpr { op: UnaryOp::Neg, operand: $2?, span: $span, metadata: () }))) }
  | SubFactor { $1 }
  ;

SubFactor -> Result<Expr<&'input str, ParseMetadata>, ()>
  : '(' Expr ')' { $2 }
  | CallExpr { Ok(Expr::Call($1?)) }
  | SubFactor '.' Ident { Ok(Expr::FieldAccess(Box::new(FieldAccessExpr { base: $1?, field: $3?, span: $span, metadata: (), }))) }
  | SubFactor '!' { Ok(Expr::Emit(Box::new(EmitExpr { value: $1?, span: $span, metadata: (), }))) }
  | IdentPath { Ok(Expr::IdentPath($1?)) }
  | IntLiteral { Ok(Expr::IntLiteral($1?)) }
  | FloatLiteral { Ok(Expr::FloatLiteral($1?)) }
  | StringLiteral { Ok(Expr::StringLiteral($1?)) }
  | BoolLiteral { Ok(Expr::BoolLiteral($1?)) }
  | NilLiteral { Ok(Expr::Nil($1?)) }
  | SubFactor 'AS' TySpec { Ok(Expr::Cast(Box::new(CastExpr { value: $1?, ty: $3?, span: $span, metadata: (), }))) }
  ;


CallExpr -> Result<CallExpr<&'input str, ParseMetadata>, ()>
  : ScopeAnnotation IdentPath '(' Args ')'
    {
      Ok(CallExpr {
        scope_annotation: Some($1?),
        func: $2?,
        args: $4?,
        span: $span,
        metadata: (),
      })
    }
  | IdentPath '(' Args ')'
    {
      Ok(CallExpr {
        scope_annotation: None,
        func: $1?,
        args: $3?,
        span: $span,
        metadata: (),
      })
    }
  ;

ArgDecls -> Result<Vec<ArgDecl<&'input str, ParseMetadata>>, ()>
  : { Ok(Vec::new()) }
  | ArgDecls1 { $1 }
  | ArgDecls1 ',' { $1 }
  ;

ArgDecls1 -> Result<Vec<ArgDecl<&'input str, ParseMetadata>>, ()>
  : ArgDecls1 ',' ArgDecl { flatten($1, $3) }
  | ArgDecl { Ok(vec![$1?]) }
  ;

ArgDecl -> Result<ArgDecl<&'input str, ParseMetadata>, ()>
  : Ident ':' TySpec { Ok(ArgDecl { name: $1?, ty: $3?, metadata: () }) }
  ;

TySpec -> Result<TySpec<&'input str, ParseMetadata>, ()>
  : Ident { Ok(TySpec { kind: TySpecKind::Ident($1?), span: $span, }) }
  | '[' TySpec ']' { Ok(TySpec { kind: TySpecKind::Seq(Box::new($2?)), span: $span, }) }
  ;

Args -> Result<Args<&'input str, ParseMetadata>, ()>
  : PosArgsTrailingComma KwArgs { Ok(Args { posargs: $1?, kwargs: $2?, span: $span, metadata: (), }) }
  | KwArgs { Ok(Args { posargs: Vec::new(), kwargs: $1?, span: $span, metadata: (), }) }
  | PosArgs { Ok(Args { posargs: $1?, kwargs: Vec::new(), span: $span, metadata: (), }) }
  ;

KwArgValue -> Result<KwArgValue<&'input str, ParseMetadata>, ()>
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

KwArgs -> Result<Vec<KwArgValue<&'input str, ParseMetadata>>, ()>
  : KwArgsTrailingComma { $1 }
  | KwArgsNoComma { $1 }
  ;

KwArgsTrailingComma -> Result<Vec<KwArgValue<&'input str, ParseMetadata>>, ()>
  : KwArgValue ',' { Ok(vec![$1?]) }
  | KwArgsTrailingComma KwArgValue ',' {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
    }
  ;

KwArgsNoComma -> Result<Vec<KwArgValue<&'input str, ParseMetadata>>, ()>
  : KwArgValue { Ok(vec![$1?]) }
  | KwArgsTrailingComma KwArgValue {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
  }
  ;


PosArgs -> Result<Vec<Expr<&'input str, ParseMetadata>>, ()>
  : PosArgsTrailingComma { $1 }
  | PosArgsNoComma { $1 }
  ;

PosArgsTrailingComma -> Result<Vec<Expr<&'input str, ParseMetadata>>, ()>
  : Expr ',' { Ok(vec![$1?]) }
  | PosArgsTrailingComma Expr ',' {
      let mut __tmp = $1?;
      __tmp.push($2?);
      Ok(__tmp)
  }
  ;

PosArgsNoComma -> Result<Vec<Expr<&'input str, ParseMetadata>>, ()>
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
