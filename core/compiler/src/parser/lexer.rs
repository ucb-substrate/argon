//! Hand-written streaming lexer for Argon.
//!
//! Scans the source as raw bytes (`&[u8]`). Every token boundary falls on an
//! ASCII byte (identifiers, keywords, numbers and operators are all ASCII;
//! non-ASCII can only appear inside string/comment bodies, which are scanned
//! byte-wise), so byte offsets are always valid UTF-8 char boundaries for the
//! tokens the parser slices. Whitespace and `//` line comments are skipped
//! (the grammar puts them on the hidden channel).

use super::token::{Token, TokenKind, keyword_or_ident};

#[inline]
fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

#[inline]
fn is_ident_continue(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

pub struct Lexer<'a> {
    /// The (leading-whitespace-trimmed) source bytes.
    src: &'a [u8],
    /// Current scan position within `src`.
    pos: usize,
    /// Added to every emitted offset so spans index the original input.
    base: u32,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str, offset_base: usize) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            base: offset_base as u32,
        }
    }

    #[inline]
    fn tok(&self, kind: TokenKind, start: usize, end: usize) -> Token {
        Token::new(kind, start as u32 + self.base, end as u32 + self.base)
    }

    /// Skip whitespace and `//` line comments.
    fn skip_trivia(&mut self) {
        let n = self.src.len();
        while self.pos < n {
            match self.src[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                b'/' if self.pos + 1 < n && self.src[self.pos + 1] == b'/' => {
                    self.pos += 2;
                    while self.pos < n {
                        let b = self.src[self.pos];
                        if b == b'\n' || b == b'\r' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Produce the next significant token (or `Eof`).
    pub fn next_token(&mut self) -> Token {
        self.skip_trivia();
        let n = self.src.len();
        let start = self.pos;
        if start >= n {
            return self.tok(TokenKind::Eof, n, n);
        }

        let b = self.src[start];
        match b {
            _ if is_ident_start(b) => self.lex_ident_or_keyword(start),
            b'#' => self.lex_annotation(start),
            b'0'..=b'9' => self.lex_int(start),
            b'"' => self.lex_string(start),
            _ => self.lex_operator(start, b),
        }
    }

    fn lex_ident_or_keyword(&mut self, start: usize) -> Token {
        self.pos += 1;
        while self.pos < self.src.len() && is_ident_continue(self.src[self.pos]) {
            self.pos += 1;
        }
        let kind = keyword_or_ident(&self.src[start..self.pos]);
        self.tok(kind, start, self.pos)
    }

    fn lex_annotation(&mut self, start: usize) -> Token {
        // ANNOTATION: '#' [_a-zA-Z] [_a-zA-Z0-9]* — the '#' must be followed by
        // an identifier start, otherwise a lone '#' is not a valid token.
        let after_hash = start + 1;
        if after_hash >= self.src.len() || !is_ident_start(self.src[after_hash]) {
            self.pos = after_hash;
            return self.tok(TokenKind::Error, start, after_hash);
        }
        self.pos = after_hash + 1;
        while self.pos < self.src.len() && is_ident_continue(self.src[self.pos]) {
            self.pos += 1;
        }
        self.tok(TokenKind::Annotation, start, self.pos)
    }

    fn lex_int(&mut self, start: usize) -> Token {
        self.pos += 1;
        while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        self.tok(TokenKind::IntLit, start, self.pos)
    }

    fn lex_string(&mut self, start: usize) -> Token {
        // STRLIT: '"' ~["\r\n]* '"' — no escapes; a newline or EOF before the
        // closing quote is a lexical error.
        let n = self.src.len();
        let mut i = start + 1;
        while i < n {
            match self.src[i] {
                b'"' => {
                    self.pos = i + 1;
                    return self.tok(TokenKind::StrLit, start, i + 1);
                }
                b'\r' | b'\n' => break,
                _ => i += 1,
            }
        }
        // Unterminated string: consume up to the offending position.
        self.pos = i;
        self.tok(TokenKind::Error, start, i)
    }

    fn lex_operator(&mut self, start: usize, b: u8) -> Token {
        let n = self.src.len();
        let peek2 = |off: usize| -> u8 {
            if start + off < n {
                self.src[start + off]
            } else {
                0
            }
        };
        // Two-character operators take priority (maximal munch).
        let (kind, len) = match b {
            b':' if peek2(1) == b':' => (TokenKind::PathSep, 2),
            b'=' if peek2(1) == b'=' => (TokenKind::EqEq, 2),
            b'=' if peek2(1) == b'>' => (TokenKind::FatArrow, 2),
            b'!' if peek2(1) == b'=' => (TokenKind::Neq, 2),
            b'>' if peek2(1) == b'=' => (TokenKind::Geq, 2),
            b'<' if peek2(1) == b'=' => (TokenKind::Leq, 2),
            b'-' if peek2(1) == b'>' => (TokenKind::Arrow, 2),
            b':' => (TokenKind::Colon, 1),
            b'=' => (TokenKind::Eq, 1),
            b'!' => (TokenKind::Bang, 1),
            b'>' => (TokenKind::Gt, 1),
            b'<' => (TokenKind::Lt, 1),
            b'-' => (TokenKind::Minus, 1),
            b'+' => (TokenKind::Plus, 1),
            b'*' => (TokenKind::Star, 1),
            b'/' => (TokenKind::Slash, 1),
            b'%' => (TokenKind::Percent, 1),
            b'(' => (TokenKind::LParen, 1),
            b')' => (TokenKind::RParen, 1),
            b'{' => (TokenKind::LBrace, 1),
            b'}' => (TokenKind::RBrace, 1),
            b'[' => (TokenKind::LBrack, 1),
            b']' => (TokenKind::RBrack, 1),
            b'.' => (TokenKind::Dot, 1),
            b';' => (TokenKind::Semi, 1),
            b',' => (TokenKind::Comma, 1),
            _ => {
                // Unknown byte: emit a one-char Error token spanning the whole
                // UTF-8 character. Advance past any continuation bytes
                // (`0b10xx_xxxx`) to the next char boundary — the same rule as
                // `str::is_char_boundary` — so subsequent spans stay aligned to
                // char boundaries (the source is valid UTF-8).
                let mut end = start + 1;
                while end < n && self.src[end] & 0xC0 == 0x80 {
                    end += 1;
                }
                self.pos = end;
                return self.tok(TokenKind::Error, start, end);
            }
        };
        self.pos = start + len;
        self.tok(kind, start, start + len)
    }
}
