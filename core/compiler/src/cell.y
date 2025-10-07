%start CallExpr
%%

Ident -> Result<Ident<&'input str, ParseMetadata>, ()>
  : 'IDENT' { Ok(Ident { span: $span, name: $lexer.span_str($span), metadata: () }) }
  ;

IdentPath -> Result<IdentPath<&'input str, ParseMetadata>, ()>
  : Ident { Ok(IdentPath { path: vec![$1?], span: $span }) }
  | Ident '::' IdentPath { 
    let mut path = vec![$1?];
    path.extend($3?.path);
    Ok(IdentPath { path, span: $span })
  }
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

CallExpr -> Result<CallExpr<&'input str, ParseMetadata>, ()>
  : IdentPath '(' Args ')'
    {
      Ok(CallExpr {
        func: $1?,
        args: $3?,
        span: $span,
        metadata: (),
      })
    }
  ;

Expr -> Result<Expr<&'input str, ParseMetadata>, ()>
  : Ident '::' Ident { Ok(Expr::EnumValue(EnumValue {name: $1?, variant: $3?, span: $span, metadata: (), } )) }
  | IntLiteral { Ok(Expr::IntLiteral($1?)) }
  | FloatLiteral { Ok(Expr::FloatLiteral($1?)) }
  ;

Args -> Result<Args<&'input str, ParseMetadata>, ()>
  : PosArgs { Ok(Args { posargs: $1?, kwargs: Vec::new(), span: $span, metadata: (), }) }
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
