grammar Argon;

compilationUnit
    : sourceItem* EOF
    ;

sourceItem
    : decl
    | topLevelStatement
    ;

topLevelStatement
    : letStmt SEMI?
    | forStmt
    | bareCallStmt SEMI?
    | expr SEMI?
    ;

decl
    : enumDecl
    | structDecl
    | cellDecl
    | fnDecl
    | constDecl
    | modDecl
    ;

enumDecl
    : ENUM IDENT LBRACE enumVariants? RBRACE
    ;

enumVariants
    : IDENT COMMA
    | enumVariants IDENT COMMA
    ;

structDecl
    : STRUCT IDENT LBRACE structFields? RBRACE
    ;

structFields
    : structField COMMA
    | structFields structField COMMA
    ;

structField
    : IDENT COLON tySpec
    ;

cellDecl
    : CELL IDENT LPAREN argDecls RPAREN scope
    ;

fnDecl
    : FN IDENT LPAREN argDecls RPAREN ARROW tySpec scope
    | FN IDENT LPAREN argDecls RPAREN scope
    ;

constDecl
    : CONST IDENT COLON tySpec EQ expr SEMI
    ;

modDecl
    : MOD IDENT SEMI
    ;

argDecls
    : argDeclList COMMA?
    |
    ;

argDeclList
    : argDecl
    | argDeclList COMMA argDecl
    ;

argDecl
    : IDENT (COLON tySpec)?
    ;

scope
    : annotation? unannotatedScope
    ;

unannotatedScope
    : LBRACE statement* expr? RBRACE
    ;

statement
    : expr SEMI
    | blockExpr
    | letStmt SEMI
    | forStmt
    ;

letStmt
    : LET identList (EQ expr)?
    ;

identList
    : IDENT (COMMA IDENT)*
    ;

forStmt
    : FOR IDENT IN expr scope
    ;

expr
    : blockExpr
    | comparisonExpr
    ;

blockExpr
    : annotation? IF expr scope ELSE scope
    | MATCH expr LBRACE matchArm+ RBRACE
    | scope
    ;

matchArm
    : identPath FAT_ARROW expr COMMA
    ;

comparisonExpr
    : additiveExpr ((EQEQ | NEQ | GEQ | GT | LEQ | LT) additiveExpr)*
    ;

additiveExpr
    : multiplicativeExpr ((PLUS | MINUS) multiplicativeExpr)*
    ;

multiplicativeExpr
    : unaryExpr ((STAR | SLASH | PERCENT) unaryExpr)*
    ;

unaryExpr
    : BANG unaryExpr
    | MINUS unaryExpr
    | LPAREN IDENT RPAREN unaryExpr
    | castExpr
    ;

castExpr
    : postfixExpr (AS tySpec)*
    ;

postfixExpr
    : primaryExpr postfixOp*
    ;

postfixOp
    : DOT IDENT
    | DOT INTLIT
    | LBRACK expr RBRACK
    | BANG
    ;

primaryExpr
    : LPAREN RPAREN
    | LBRACK RBRACK
    | tupleExpr
    | LPAREN expr RPAREN
    | blockExpr
    | structLiteralExpr
    | deferExpr
    | scopedCallExpr
    | identPath
    | literal
    ;

structLiteralExpr
    : identPath LBRACE structLiteralFields? RBRACE
    ;

structLiteralFields
    : structLiteralField (COMMA structLiteralField)* COMMA?
    ;

structLiteralField
    : IDENT (EQ expr)?
    ;

deferExpr
    : DEFER IDENT scope
    ;

bareCallStmt
    : identPath bareArg+
    ;

bareArg
    : identPath
    | literal
    | LPAREN expr RPAREN
    | scope
    ;

tupleExpr
    : LPAREN expr COMMA tupleExprRest? RPAREN
    ;

tupleExprRest
    : expr (COMMA expr)* COMMA?
    ;

scopedCallExpr
    : annotation? identPath LPAREN args RPAREN
    ;

args
    : argumentList?
    ;

argumentList
    : argument (COMMA argument)* COMMA?
    ;

argument
    : IDENT EQ expr
    | expr
    ;

identPath
    : IDENT (PATHSEP IDENT)*
    ;

tySpec
    : IDENT
    | LBRACK tySpec RBRACK
    | LPAREN tySpecList RPAREN
    ;

tySpecList
    : tySpec (COMMA tySpec)*
    ;

literal
    : floatLiteral
    | INTLIT
    | STRLIT
    | TRUE
    | FALSE
    ;

floatLiteral
    : INTLIT DOT
    | INTLIT DOT INTLIT
    ;

annotation
    : ANNOTATION
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
DEFER: 'defer';
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
