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
    : (ident (COMMA ident)* COMMA?)?
    ;

structFields
    : (structField (COMMA structField)* COMMA?)?
    ;

structField
    : ident COLON ident
    ;

cellDecl
    : CELL ident LPAREN argDecls RPAREN scope
    ;

fnDecl
    : FN ident LPAREN argDecls RPAREN (ARROW tySpec)? scope
    ;

argDecls
    : (argDecl (COMMA argDecl)* COMMA?)?
    ;

argDecl
    : ident COLON tySpec
    ;

scope
    : scopeAnnotation? unannotatedScope
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
    | ifExpr
    | matchExpr
    | scope
    | letBinding SEMI
    | forLoop
    ;

letBinding
    : LET ident EQ expr
    ;

forLoop
    : FOR ident IN expr scope
    ;

expr
    : nonBlockExpr
    | ifExpr
    | matchExpr
    | scope
    ;

ifExpr
    : scopeAnnotation? IF expr scope ELSE scope
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
    : LPAREN expr RPAREN
    | BANG nonBlockExpr
    | MINUS nonBlockExpr
    | nonBlockExpr DOT ident
    | nonBlockExpr DOT intLiteral
    | nonBlockExpr LBRACK expr RBRACK
    | nonBlockExpr BANG
    | nonBlockExpr AS tySpec
    | nonBlockExpr (STAR | SLASH | PERCENT) nonBlockExpr
    | nonBlockExpr (PLUS | MINUS) nonBlockExpr
    | nonBlockExpr (EQEQ | NEQ | GEQ | GT | LEQ | LT) nonBlockExpr
    | nilLiteral
    | seqNilLiteral
    | tupleExpr
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
    : scopeAnnotation? identPath LPAREN args RPAREN
    ;

args
    : posArgList (COMMA kwArgList)? COMMA?
    | kwArgList COMMA?
    |
    ;

kwArgValue
    : ident EQ expr
    ;

kwArgList
    : kwArgValue (COMMA kwArgValue)*
    ;

posArgList
    : expr (COMMA expr)*
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
