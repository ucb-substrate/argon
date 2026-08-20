//! Token definitions for the hand-written Argon lexer.
//!
//! Tokens carry only a kind and a byte-offset span into the source; they never
//! own any text. The parser slices identifier/string text directly from the
//! input by span, so lexing and parsing are entirely copy-free.

/// The lexical category of a token. Mirrors the lexer rules in
/// `grammar/Argon.g4` (kept as the language reference).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TokenKind {
    // Keywords.
    KwEnum,
    KwStruct,
    KwMatch,
    KwConst,
    KwCell,
    KwMod,
    KwIf,
    KwFn,
    KwElse,
    KwLet,
    KwFor,
    KwIn,
    KwAs,
    KwTrue,
    KwFalse,

    // Names & literals.
    Ident,
    /// `#name` — the leading `#` is part of the token span; the parser strips it.
    Annotation,
    IntLit,
    /// `"..."` — the span includes both quotes; the parser trims them.
    StrLit,

    // Multi-character operators (lexed with maximal munch).
    PathSep,  // ::
    FatArrow, // =>
    EqEq,     // ==
    Neq,      // !=
    Geq,      // >=
    Leq,      // <=
    Arrow,    // ->

    // Single-character operators / punctuation.
    Lt,      // <
    Gt,      // >
    Eq,      // =
    Bang,    // !
    Plus,    // +
    Minus,   // -
    Star,    // *
    Slash,   // /
    Percent, // %
    LParen,  // (
    RParen,  // )
    LBrace,  // {
    RBrace,  // }
    LBrack,  // [
    RBrack,  // ]
    Dot,     // .
    Colon,   // :
    Semi,    // ;
    Comma,   // ,

    /// End of input. Its span is the empty range `[len, len)`.
    Eof,
    /// A byte that does not begin any valid token, or an unterminated string.
    /// Carries a span so a diagnostic can point at it; the parser turns it into
    /// a `ParseError` and recovers.
    Error,
}

impl TokenKind {
    /// A short human-readable name used in error messages (e.g. `expected ')'`).
    pub fn describe(self) -> &'static str {
        use TokenKind::*;
        match self {
            KwEnum => "'enum'",
            KwStruct => "'struct'",
            KwMatch => "'match'",
            KwConst => "'const'",
            KwCell => "'cell'",
            KwMod => "'mod'",
            KwIf => "'if'",
            KwFn => "'fn'",
            KwElse => "'else'",
            KwLet => "'let'",
            KwFor => "'for'",
            KwIn => "'in'",
            KwAs => "'as'",
            KwTrue => "'true'",
            KwFalse => "'false'",
            Ident => "identifier",
            Annotation => "annotation",
            IntLit => "integer literal",
            StrLit => "string literal",
            PathSep => "'::'",
            FatArrow => "'=>'",
            EqEq => "'=='",
            Neq => "'!='",
            Geq => "'>='",
            Leq => "'<='",
            Arrow => "'->'",
            Lt => "'<'",
            Gt => "'>'",
            Eq => "'='",
            Bang => "'!'",
            Plus => "'+'",
            Minus => "'-'",
            Star => "'*'",
            Slash => "'/'",
            Percent => "'%'",
            LParen => "'('",
            RParen => "')'",
            LBrace => "'{'",
            RBrace => "'}'",
            LBrack => "'['",
            RBrack => "']'",
            Dot => "'.'",
            Colon => "':'",
            Semi => "';'",
            Comma => "','",
            Eof => "end of input",
            Error => "invalid token",
        }
    }
}

/// A lexed token: a kind plus the half-open byte range `[start, end)` it covers
/// in the original source. Offsets already include the `offset_base` (the count
/// of leading whitespace bytes trimmed before lexing), so they index the
/// original (untrimmed) input — matching the spans the ANTLR integration
/// produced.
#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub start: u32,
    pub end: u32,
}

impl Token {
    #[inline]
    pub fn new(kind: TokenKind, start: u32, end: u32) -> Self {
        Self { kind, start, end }
    }
}

/// Classify a freshly scanned identifier slice as a keyword or a plain
/// identifier. There are only 15 short, disjoint keywords, so a `match` on
/// `(len, bytes)` lowers to a jump table + `memcmp` and beats a hash map with
/// zero setup cost.
#[inline]
pub fn keyword_or_ident(s: &[u8]) -> TokenKind {
    use TokenKind::*;
    match s.len() {
        2 => match s {
            b"fn" => KwFn,
            b"if" => KwIf,
            b"as" => KwAs,
            b"in" => KwIn,
            _ => Ident,
        },
        3 => match s {
            b"let" => KwLet,
            b"for" => KwFor,
            b"mod" => KwMod,
            _ => Ident,
        },
        4 => match s {
            b"enum" => KwEnum,
            b"cell" => KwCell,
            b"true" => KwTrue,
            b"else" => KwElse,
            _ => Ident,
        },
        5 => match s {
            b"match" => KwMatch,
            b"const" => KwConst,
            b"false" => KwFalse,
            _ => Ident,
        },
        6 => {
            if s == b"struct" {
                KwStruct
            } else {
                Ident
            }
        }
        _ => Ident,
    }
}
