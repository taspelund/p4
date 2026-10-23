// Copyright 2026 Oxide Computer Company

//! P4-16 lexer.
//!
//! Every word is lexed as [`TokenKind::Ident`]; keywords are recognized by
//! the parser, because P4 lets several keywords (`apply`, `key`, `state`,
//! `type`, ...) appear as names in some positions. `>` is always lexed as a
//! single character so that nested type arguments such as `tuple<bit<8>>`
//! need no special casing; the parser joins adjacent `>` tokens into `>>`
//! or `>=` using [`Token::ws_before`].

use crate::diag::Diagnostic;
use crate::source::{FileId, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// An identifier or keyword.
    Ident,
    /// An integer literal, including width-prefixed forms such as `8w0xff`
    /// and `16s5`. The parser interprets the text.
    Int,
    /// A string literal including its quotes.
    Str,

    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    /// `<`
    Lt,
    /// `>`; see the module documentation.
    Gt,
    /// `<=`
    LtEq,
    /// `<<`
    Shl,
    Semi,
    Comma,
    Colon,
    Dot,
    /// `..`
    DotDot,
    Question,
    /// `=`
    Eq,
    /// `==`
    EqEq,
    /// `!=`
    NotEq,
    Plus,
    /// `++` (bit-string concatenation)
    PlusPlus,
    /// `|+|`
    PlusSat,
    Minus,
    /// `|-|`
    MinusSat,
    Star,
    Slash,
    Percent,
    Amp,
    AmpAmp,
    /// `&&&` (ternary mask)
    AmpAmpAmp,
    Pipe,
    PipePipe,
    Caret,
    Tilde,
    Bang,
    At,
    /// `#`, which begins a preprocessor directive at the start of a line.
    Hash,
    /// `##` (token pasting in macro bodies).
    HashHash,
    /// `\` not followed by a newline.
    Backslash,
    /// A character that begins no valid token.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// The token's spelling. For tokens produced by the preprocessor through
    /// token pasting this may not match the source under `span`.
    pub text: String,
    /// Where the token appears in source. Tokens produced by macro expansion
    /// carry the span of the macro invocation.
    pub span: Span,
    /// Whether whitespace or a comment precedes the token.
    pub ws_before: bool,
    /// Whether the token is the first on its line. Backslash-newline line
    /// continuations do not start a new line.
    pub nl_before: bool,
}

impl Token {
    pub fn is(&self, kind: TokenKind) -> bool {
        self.kind == kind
    }

    /// Whether the token is the identifier or keyword `word`.
    pub fn is_word(&self, word: &str) -> bool {
        self.kind == TokenKind::Ident && self.text == word
    }
}

#[derive(Debug, Default)]
pub struct LexOutput {
    pub tokens: Vec<Token>,
    /// Spans of all comments, in source order, for documentation lookup.
    pub comments: Vec<Span>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn lex(file: FileId, text: &str) -> LexOutput {
    let mut out = LexOutput::default();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut ws_before = true;
    let mut nl_before = true;

    while i < bytes.len() {
        let c = bytes[i];

        // Whitespace and line continuations.
        if c == b'\n' {
            nl_before = true;
            ws_before = true;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            ws_before = true;
            i += 1;
            continue;
        }
        if c == b'\\' {
            let mut j = i + 1;
            while j < bytes.len()
                && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r')
            {
                j += 1;
            }
            if j >= bytes.len() || bytes[j] == b'\n' {
                ws_before = true;
                i = (j + 1).min(bytes.len());
                continue;
            }
        }

        // Comments.
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let end =
                text[i..].find('\n').map(|n| i + n).unwrap_or(bytes.len());
            out.comments.push(Span::new(file, i as u32, end as u32));
            ws_before = true;
            i = end;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let (end, closed) = match text[i + 2..].find("*/") {
                Some(n) => (i + 2 + n + 2, true),
                None => (bytes.len(), false),
            };
            let span = Span::new(file, i as u32, end as u32);
            if !closed {
                out.diagnostics.push(Diagnostic::error(
                    span,
                    "unterminated block comment",
                ));
            }
            if text[i..end].contains('\n') {
                nl_before = true;
            }
            out.comments.push(span);
            ws_before = true;
            i = end;
            continue;
        }

        let start = i;
        let kind = if c.is_ascii_alphabetic() || c == b'_' {
            i = scan_word(bytes, i);
            TokenKind::Ident
        } else if c.is_ascii_digit() {
            i = scan_number(bytes, i);
            TokenKind::Int
        } else if c == b'"' {
            i += 1;
            let mut closed = false;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        i += 1;
                        closed = true;
                        break;
                    }
                    _ => i += 1,
                }
            }
            i = i.min(bytes.len());
            if !closed {
                // P4 strings may span lines, but a quote left open while
                // typing should not swallow the rest of the file.
                i = text[start..].find('\n').map_or(i, |n| start + n);
                out.diagnostics.push(Diagnostic::error(
                    Span::new(file, start as u32, i as u32),
                    "unterminated string literal",
                ));
            }
            TokenKind::Str
        } else {
            let (kind, len) = punct(&bytes[i..]);
            i += len;
            if kind == TokenKind::Unknown {
                // Consume a whole UTF-8 character.
                while i < bytes.len() && !text.is_char_boundary(i) {
                    i += 1;
                }
                out.diagnostics.push(Diagnostic::error(
                    Span::new(file, start as u32, i as u32),
                    format!("unexpected character `{}`", &text[start..i]),
                ));
            }
            kind
        };

        out.tokens.push(Token {
            kind,
            text: text[start..i].to_string(),
            span: Span::new(file, start as u32, i as u32),
            ws_before,
            nl_before,
        });
        ws_before = false;
        nl_before = false;
    }
    out
}

fn scan_word(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
    {
        i += 1;
    }
    i
}

/// Scans an integer literal: an optional width prefix (`8w`, `16s`) followed
/// by a decimal, or a `0x`/`0o`/`0b`/`0d` prefixed number. Underscores are
/// allowed as separators. Trailing alphanumerics are included so that a
/// malformed literal such as `12abc` is one token the parser can reject.
fn scan_number(bytes: &[u8], i: usize) -> usize {
    scan_word(bytes, i)
}

fn punct(s: &[u8]) -> (TokenKind, usize) {
    use TokenKind::*;
    let at = |n: usize| s.get(n).copied();
    match s[0] {
        b'{' => (LBrace, 1),
        b'}' => (RBrace, 1),
        b'(' => (LParen, 1),
        b')' => (RParen, 1),
        b'[' => (LBracket, 1),
        b']' => (RBracket, 1),
        b';' => (Semi, 1),
        b',' => (Comma, 1),
        b':' => (Colon, 1),
        b'?' => (Question, 1),
        b'~' => (Tilde, 1),
        b'^' => (Caret, 1),
        b'*' => (Star, 1),
        b'/' => (Slash, 1),
        b'%' => (Percent, 1),
        b'@' => (At, 1),
        b'\\' => (Backslash, 1),
        b'>' => (Gt, 1),
        b'.' if at(1) == Some(b'.') => (DotDot, 2),
        b'.' => (Dot, 1),
        b'<' if at(1) == Some(b'<') => (Shl, 2),
        b'<' if at(1) == Some(b'=') => (LtEq, 2),
        b'<' => (Lt, 1),
        b'=' if at(1) == Some(b'=') => (EqEq, 2),
        b'=' => (Eq, 1),
        b'!' if at(1) == Some(b'=') => (NotEq, 2),
        b'!' => (Bang, 1),
        b'+' if at(1) == Some(b'+') => (PlusPlus, 2),
        b'+' => (Plus, 1),
        b'-' => (Minus, 1),
        b'&' if at(1) == Some(b'&') && at(2) == Some(b'&') => (AmpAmpAmp, 3),
        b'&' if at(1) == Some(b'&') => (AmpAmp, 2),
        b'&' => (Amp, 1),
        b'|' if at(1) == Some(b'+') && at(2) == Some(b'|') => (PlusSat, 3),
        b'|' if at(1) == Some(b'-') && at(2) == Some(b'|') => (MinusSat, 3),
        b'|' if at(1) == Some(b'|') => (PipePipe, 2),
        b'|' => (Pipe, 1),
        b'#' if at(1) == Some(b'#') => (HashHash, 2),
        b'#' => (Hash, 1),
        _ => (Unknown, 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(src: &str) -> Vec<(TokenKind, String)> {
        lex(FileId(0), src)
            .tokens
            .into_iter()
            .map(|t| (t.kind, t.text))
            .collect()
    }

    #[test]
    fn nested_angles_stay_separate() {
        let k: Vec<_> =
            kinds("tuple<bit<8>>").into_iter().map(|t| t.0).collect();
        assert_eq!(k, vec![Ident, Lt, Ident, Lt, Int, Gt, Gt]);
    }

    #[test]
    fn literals_and_operators() {
        assert_eq!(
            kinds("8w0xff |+| x &&& 16s5 .. \"a\\\"b\""),
            vec![
                (Int, "8w0xff".into()),
                (PlusSat, "|+|".into()),
                (Ident, "x".into()),
                (AmpAmpAmp, "&&&".into()),
                (Int, "16s5".into()),
                (DotDot, "..".into()),
                (Str, "\"a\\\"b\"".into()),
            ]
        );
    }

    #[test]
    fn line_flags_and_continuations() {
        let out = lex(FileId(0), "#define A \\\n  1\nx  y/*c*/z");
        let flags: Vec<_> = out
            .tokens
            .iter()
            .map(|t| (t.text.as_str(), t.nl_before, t.ws_before))
            .collect();
        assert_eq!(
            flags,
            vec![
                ("#", true, true),
                ("define", false, false),
                ("A", false, true),
                ("1", false, true),
                ("x", true, true),
                ("y", false, true),
                ("z", false, true),
            ]
        );
        assert_eq!(out.comments.len(), 1);
    }

    #[test]
    fn strings_may_span_lines() {
        let out = lex(FileId(0), "@name(\"a\nb \\\nc\") x");
        let texts: Vec<_> =
            out.tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["@", "name", "(", "\"a\nb \\\nc\"", ")", "x"]);
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn errors_are_reported_and_recovered() {
        let out = lex(FileId(0), "a $ b \"open\nc /* never");
        let texts: Vec<_> =
            out.tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "$", "b", "\"open", "c"]);
        assert_eq!(out.diagnostics.len(), 3);
    }
}
