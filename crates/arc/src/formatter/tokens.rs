//! A lexer that finds the brackets, separators, and comments of Argon source.

/// The role of a token in line breaking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TokenKind {
    Open(u8),
    Close(u8),
    Comma,
    Semi,
    /// A `//` comment, up to the end of its line.
    Comment,
    /// Any other run of text without whitespace, including a string literal.
    Other,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Token {
    pub(super) kind: TokenKind,
    pub(super) start: usize,
    pub(super) end: usize,
}

/// Splits `text` into tokens. Like the compiler's lexer, a string literal has
/// no escapes and ends at its line, so no token spans two lines.
pub(super) fn tokenize(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let byte = bytes[index];
        index += 1;
        let kind = match byte {
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            b'/' if bytes.get(index) == Some(&b'/') => {
                index = line_end(bytes, index);
                TokenKind::Comment
            }
            b'"' => {
                while index < bytes.len() && !matches!(bytes[index], b'"' | b'\n') {
                    index += 1;
                }
                if bytes.get(index) == Some(&b'"') {
                    index += 1;
                }
                TokenKind::Other
            }
            b'(' | b'[' | b'{' => TokenKind::Open(byte),
            b')' | b']' | b'}' => TokenKind::Close(byte),
            b',' => TokenKind::Comma,
            b';' => TokenKind::Semi,
            _ => {
                while index < bytes.len() && !starts_token(bytes, index) {
                    index += 1;
                }
                TokenKind::Other
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
    }
    tokens
}

/// Whether a token other than [`TokenKind::Other`] starts at `index`, or
/// whitespace ends one there.
fn starts_token(bytes: &[u8], index: usize) -> bool {
    match bytes[index] {
        b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'(' | b'[' | b'{' | b')' | b']' | b'}' | b','
        | b';' => true,
        b'/' => bytes.get(index + 1) == Some(&b'/'),
        _ => false,
    }
}

fn line_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

/// Whether `close` closes `open`.
pub(super) fn closes(open: u8, close: u8) -> bool {
    matches!((open, close), (b'(', b')') | (b'[', b']') | (b'{', b'}'))
}
