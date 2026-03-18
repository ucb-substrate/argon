grammar Argon;

ast
    : decl* EOF
    ;

decl
    : enumDecl
    | structDecl
    | cellDecl
    | fnDecl
    | constantDecl
    | modDecl
    ;

ident
    : IDENT
    ;

identPath
    : ident (PATHSEP ident)*
    ;

nilLiteral
    : LPAREN RPAREN
    ;

seqNilLiteral
    : LBRACK RBRACK
    ;

floatLiteral
    : INTLIT DOT INTLIT?
    ;

intLiteral
    : INTLIT
    ;

stringLiteral
    : STRLIT
    ;

boolLiteral
    : TRUE
    | FALSE
    ;

literal
    : floatLiteral
    | intLiteral
    | stringLiteral
    | boolLiteral
    ;

enumDecl
    : ENUM ident LBRACE enumVariants RBRACE
    ;

structDecl
    : STRUCT ident LBRACE structFields RBRACE
    ;

constantDecl
    : CONST ident COLON ident EQ expr SEMI
    ;

modDecl
    : MOD ident SEMI
    ;

enumVariants
    : (ident COMMA)*
    ;

structFields
    : (structField COMMA)*
    ;

structField
    : ident COLON ident
    ;

cellDecl
    : CELL ident LPAREN argDecls RPAREN scope
    ;

fnDecl
    : FN ident LPAREN argDecls RPAREN ARROW tySpec scope
    | FN ident LPAREN argDecls RPAREN scope
    ;

argDecls
    : argDecls1 COMMA?
    |
    ;

argDecls1
    : argDecl (COMMA argDecl)*
    ;

argDecl
    : ident COLON tySpec
    ;

scope
    : scopeAnnotation unannotatedScope
    | unannotatedScope
    ;

scopeAnnotation
    : ANNOTATION
    ;

unannotatedScope
    : LBRACE statements RBRACE
    | LBRACE statements nonBlockExpr RBRACE
    ;

statements
    : statement*
    ;

statement
    : expr SEMI
    | blockExpr
    | letStmt SEMI
    | forStmt
    ;

letStmt
    : LET ident EQ expr
    ;

forStmt
    : FOR ident IN expr scope
    ;

expr
    : nonBlockExpr
    | blockExpr
    ;

blockExpr
    : IF expr scope ELSE scope
    | scopeAnnotation IF expr scope ELSE scope
    | matchExpr
    | scope
    ;

matchExpr
    : MATCH expr LBRACE matchArms RBRACE
    ;

matchArms
    : matchArm+
    ;

matchArm
    : identPath FAT_ARROW expr COMMA
    ;

nonBlockExpr
    : nonComparisonExpr ((EQEQ | NEQ | GEQ | GT | LEQ | LT) nonComparisonExpr)*
    ;

nonComparisonExpr
    : term ((PLUS | MINUS) term)*
    ;

term
    : factor ((STAR | SLASH | PERCENT) factor)*
    ;

factor
    : BANG factor
    | MINUS factor
    | subFactor
    ;

subFactor
    : objExpr (AS tySpec)*
    ;

objExpr
    : primaryExpr postfixOp*
    ;

postfixOp
    : DOT ident
    | DOT intLiteral
    | LBRACK expr RBRACK
    | BANG
    ;

primaryExpr
    : nilLiteral
    | seqNilLiteral
    | tupleExpr
    | LPAREN expr RPAREN
    | callExpr
    | identPath
    | literal
    ;

tupleExpr
    : LPAREN tupleExprList RPAREN
    ;

tupleExprList
    : expr COMMA (expr COMMA)*
    ;

callExpr
    : scopeAnnotation identPath LPAREN args RPAREN
    | identPath LPAREN args RPAREN
    ;

args
    : posArgsTrailingComma kwArgs
    | kwArgs
    | posArgs
    ;

kwArgValue
    : ident EQ expr
    ;

kwArgs
    : kwArgsTrailingComma
    | kwArgsNoComma
    ;

kwArgsTrailingComma
    : kwArgValue COMMA
    | kwArgsTrailingComma kwArgValue COMMA
    ;

kwArgsNoComma
    : kwArgValue
    | kwArgsTrailingComma kwArgValue
    ;

posArgs
    : posArgsTrailingComma
    | posArgsNoComma
    ;

posArgsTrailingComma
    : expr COMMA
    | posArgsTrailingComma expr COMMA
    ;

posArgsNoComma
    :
    | expr
    | posArgsTrailingComma expr
    ;

tySpec
    : ident
    | LBRACK tySpec RBRACK
    | LPAREN tySpecList RPAREN
    ;

tySpecList
    : tySpec (COMMA tySpec)*
    ;

ENUM: 'enum';
STRUCT: 'struct';
MATCH: 'match';
CONST: 'const';
CELL: 'cell';
MOD: 'mod';
IF: 'if';
FN: 'fn';
ELSE: 'else';
LET: 'let';
FOR: 'for';
IN: 'in';
AS: 'as';
TRUE: 'true';
FALSE: 'false';

ANNOTATION: '#' [_a-zA-Z] [_a-zA-Z0-9]*;
IDENT: [_a-zA-Z] [_a-zA-Z0-9]*;
INTLIT: [0-9]+;
STRLIT: '"' ~["\r\n]* '"';

PATHSEP: '::';
FAT_ARROW: '=>';
EQEQ: '==';
NEQ: '!=';
GEQ: '>=';
GT: '>';
LEQ: '<=';
LT: '<';
EQ: '=';
ARROW: '->';
PLUS: '+';
STAR: '*';
PERCENT: '%';
MINUS: '-';
SLASH: '/';
BANG: '!';
LPAREN: '(';
RPAREN: ')';
LBRACE: '{';
RBRACE: '}';
LBRACK: '[';
RBRACK: ']';
DOT: '.';
COLON: ':';
SEMI: ';';
COMMA: ',';

LINE_COMMENT: '//' ~[\r\n]* -> channel(HIDDEN);
WS: [ \t\r\n]+ -> channel(HIDDEN);
