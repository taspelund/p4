// Copyright 2026 Oxide Computer Company

//! Error-recovering recursive-descent parser for P4-16.
//!
//! The grammar follows the P4-16 specification and p4c's Bison grammar.
//! Ambiguities that p4c resolves with a table of declared type names are
//! resolved here structurally, with bounded speculative parsing:
//!
//! - A statement is a declaration when it starts with a type followed by a
//!   name, or a type and a parenthesized argument list followed by a name.
//! - `e < ...` is a call with type arguments when the tokens after `<` parse
//!   as type arguments followed by `>` and `(`; otherwise it is less-than.
//! - `(T) e` is a cast when the parenthesized tokens parse as a type and the
//!   next token can only begin an operand. Before `-` or `+`, a lone name is
//!   a cast only if it was declared as a type earlier in the token stream.
//!
//! Recovery: every list loop consumes at least one token per iteration or
//! exits. A missing `;` at the end of a line is reported after the previous
//! token and parsing continues on the next line; other errors skip to the
//! next `;`, to a closing `}`, past a balanced `{ ... }`, or to a keyword
//! that begins a line. Blocks end early at keywords that cannot appear in
//! them (`action`, `table`, `control`, ...), and bodies of parsers, controls,
//! externs, and structs end early at top-level keywords that begin a line,
//! so a missing `}` does not swallow later declarations.

use std::collections::HashSet;

use super::ast::*;
use crate::diag::Diagnostic;
use crate::lexer::{Token, TokenKind as K};
use crate::source::{FileId, Span};

#[derive(Debug, Default)]
pub struct Parse {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
    /// Spans of all annotations. Their bodies are free-form, so characters
    /// the lexer rejected inside them are not errors.
    pub annotation_spans: Vec<Span>,
}

pub fn parse(tokens: &[Token]) -> Parse {
    let mut p = Parser::new(tokens);
    let program = p.program();
    Parse {
        program,
        diagnostics: p.diags,
        annotation_spans: p.annotation_spans,
    }
}

/// Words that can never be used as names. `actions`, `apply`, `entries`,
/// `key`, `list`, `priority`, `state`, and `type` are keywords only in some
/// positions.
const RESERVED: &[&str] = &[
    "_",
    "abstract",
    "action",
    "bit",
    "bool",
    "break",
    "const",
    "continue",
    "control",
    "default",
    "else",
    "enum",
    "error",
    "exit",
    "extern",
    "false",
    "for",
    "header",
    "header_union",
    "if",
    "in",
    "inout",
    "int",
    "match_kind",
    "out",
    "package",
    "parser",
    "return",
    "select",
    "string",
    "struct",
    "switch",
    "table",
    "this",
    "transition",
    "true",
    "tuple",
    "typedef",
    "value_set",
    "varbit",
    "void",
];

/// Keywords that begin a declaration that is only valid at the top level.
const TOP_LEVEL: &[&str] = &[
    "control",
    "parser",
    "extern",
    "package",
    "struct",
    "header",
    "header_union",
    "typedef",
    "enum",
    "match_kind",
];

/// Keywords at the start of a line where error recovery stops skipping.
const SYNC_WORDS: &[&str] = &[
    "if",
    "switch",
    "return",
    "exit",
    "for",
    "break",
    "continue",
    "const",
    "transition",
    "state",
    "action",
    "table",
    "apply",
    "value_set",
    "control",
    "parser",
    "extern",
    "package",
    "struct",
    "header",
    "header_union",
    "typedef",
    "enum",
    "match_kind",
];

/// Maximum nesting of expressions, types, and statements.
const MAX_DEPTH: u32 = 96;

/// Maximum length of operator, member, index, and call chains on the
/// parser's stack. Chains are parsed in loops but build trees as deep as they
/// are long, which later recursive passes (and `Drop`) walk; this bounds
/// their stack use.
const MAX_LINKS: u32 = 256;

#[derive(Clone, Copy)]
struct Mark {
    pos: usize,
    span: Span,
}

#[derive(Clone, Copy)]
struct Snapshot {
    pos: usize,
    diags: usize,
    last_err: usize,
    errors: usize,
}

struct Parser<'t> {
    toks: Vec<&'t Token>,
    pos: usize,
    diags: Vec<Diagnostic>,
    /// Token position of the last reported error; further errors at the
    /// same position are suppressed to avoid cascades.
    last_err: usize,
    /// Count of errors including suppressed ones, for speculation.
    errors: usize,
    /// Names declared as types so far (including type parameters).
    types: HashSet<String>,
    /// Inside `bit<...>`, where `>` closes the width rather than comparing.
    no_gt: bool,
    depth: u32,
    /// Chain links open on the stack; see [`MAX_LINKS`].
    links: u32,
    /// Inside a parser state, where nested blocks may contain transitions.
    in_state: bool,
    annotation_spans: Vec<Span>,
}

fn is_word(t: &Token, w: &str) -> bool {
    t.kind == K::Ident && t.text == w
}

fn is_name_token(t: &Token) -> bool {
    t.kind == K::Ident && !RESERVED.contains(&t.text.as_str())
}

impl<'t> Parser<'t> {
    fn new(tokens: &'t [Token]) -> Self {
        Parser {
            // The lexer already reported unknown characters.
            toks: tokens.iter().filter(|t| t.kind != K::Unknown).collect(),
            pos: 0,
            diags: Vec::new(),
            last_err: usize::MAX,
            errors: 0,
            types: HashSet::new(),
            no_gt: false,
            depth: 0,
            links: 0,
            in_state: false,
            annotation_spans: Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Token access

    fn tok(&self, n: usize) -> Option<&'t Token> {
        self.toks.get(self.pos + n).copied()
    }

    fn eof(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn at(&self, k: K) -> bool {
        self.nth(0, k)
    }

    fn nth(&self, n: usize, k: K) -> bool {
        self.tok(n).is_some_and(|t| t.kind == k)
    }

    fn at_word(&self, w: &str) -> bool {
        self.nth_word(0, w)
    }

    fn nth_word(&self, n: usize, w: &str) -> bool {
        self.tok(n).is_some_and(|t| is_word(t, w))
    }

    /// Whether token `n` is of kind `k` and directly follows token `n - 1`
    /// with no whitespace, for multi-token operators.
    fn joined(&self, n: usize, k: K) -> bool {
        self.tok(n).is_some_and(|t| t.kind == k && !t.ws_before)
    }

    fn at_name(&self) -> bool {
        self.tok(0).is_some_and(is_name_token)
    }

    fn word(&self) -> &'t str {
        match self.tok(0) {
            Some(t) if t.kind == K::Ident => t.text.as_str(),
            _ => "",
        }
    }

    fn bump(&mut self) {
        if self.pos < self.toks.len() {
            self.pos += 1;
        }
    }

    fn bump_n(&mut self, n: usize) {
        for _ in 0..n {
            self.bump();
        }
    }

    fn eat(&mut self, k: K) -> bool {
        if self.at(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.at_word(w) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn cur_span(&self) -> Span {
        match self.tok(0) {
            Some(t) => t.span,
            None => self.end_span(),
        }
    }

    /// A zero-width span just after the previous token.
    fn end_span(&self) -> Span {
        let prev = if self.pos > 0 {
            self.toks.get(self.pos - 1)
        } else {
            None
        };
        match prev {
            Some(t) => Span::new(t.span.file, t.span.end, t.span.end),
            None => match self.toks.first() {
                Some(t) => Span::new(t.span.file, t.span.start, t.span.start),
                None => Span::new(FileId(0), 0, 0),
            },
        }
    }

    fn mark(&self) -> Mark {
        Mark {
            pos: self.pos,
            span: self.cur_span(),
        }
    }

    /// The span from `m` to the last consumed token.
    fn finish(&self, m: Mark) -> Span {
        if self.pos > m.pos {
            m.span.to(self.toks[self.pos - 1].span)
        } else {
            Span::new(m.span.file, m.span.start, m.span.start)
        }
    }

    fn found(&self) -> String {
        match self.tok(0) {
            Some(t) => format!("`{}`", t.text),
            None => "end of file".into(),
        }
    }

    // -----------------------------------------------------------------------
    // Diagnostics and speculation

    fn error_at(&mut self, span: Span, msg: impl Into<String>) {
        self.push_error(Diagnostic::error(span, msg));
    }

    fn push_error(&mut self, d: Diagnostic) {
        self.errors += 1;
        if self.last_err == self.pos {
            return;
        }
        self.last_err = self.pos;
        self.diags.push(d);
    }

    /// Reports "expected X, found Y" at the current token.
    fn expected(&mut self, what: &str) {
        let msg = format!("expected {what}, found {}", self.found());
        let span = self.cur_span();
        self.error_at(span, msg);
    }

    fn expect(&mut self, k: K, text: &str) -> bool {
        if self.eat(k) {
            return true;
        }
        self.expected(&format!("`{text}`"));
        false
    }

    /// Expects a closing delimiter, pointing at the opener if it is missing.
    /// Skips tokens on the way to a matching closer when that is safe.
    fn close(&mut self, k: K, text: &str, open: Span) -> bool {
        if self.eat(k) {
            return true;
        }
        let msg = format!("expected `{text}`, found {}", self.found());
        let d = Diagnostic::error(self.cur_span(), msg)
            .with_related(open, format!("unclosed `{}`", opener(k)));
        self.push_error(d);
        // Look for the closer on the same line, without crossing statement
        // boundaries.
        let mut depth = 0i32;
        let mut i = 0;
        while let Some(t) = self.tok(i) {
            if i > 0 && t.nl_before {
                return false;
            }
            match t.kind {
                K::LParen | K::LBracket => depth += 1,
                K::RParen | K::RBracket if depth > 0 => depth -= 1,
                kind if kind == k && depth == 0 => {
                    self.bump_n(i + 1);
                    return true;
                }
                K::Semi | K::LBrace | K::RBrace => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            pos: self.pos,
            diags: self.diags.len(),
            last_err: self.last_err,
            errors: self.errors,
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.pos = s.pos;
        self.diags.truncate(s.diags);
        self.last_err = s.last_err;
        self.errors = s.errors;
    }

    /// Runs `f`, keeping its effects only if it returns `Some` without
    /// reporting errors.
    fn speculate<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Option<T> {
        let s = self.snapshot();
        self.last_err = usize::MAX;
        let r = f(self);
        if r.is_some() && self.errors == s.errors {
            self.last_err = s.last_err;
            r
        } else {
            self.restore(s);
            None
        }
    }

    /// Whether `f` succeeds without errors; never consumes tokens.
    fn lookahead(&mut self, f: impl FnOnce(&mut Self) -> bool) -> bool {
        let s = self.snapshot();
        self.last_err = usize::MAX;
        let r = f(self) && self.errors == s.errors;
        self.restore(s);
        r
    }

    /// Skips to a likely statement or declaration boundary: past the next
    /// `;` or balanced `{ ... }`, or before a `}` or a keyword or annotation
    /// that begins a line.
    fn sync(&mut self) {
        let start = self.pos;
        let mut depth = 0i32;
        while let Some(t) = self.tok(0) {
            if depth == 0 && self.pos > start && t.nl_before {
                if t.kind == K::At {
                    return;
                }
                if t.kind == K::Ident && SYNC_WORDS.contains(&t.text.as_str()) {
                    return;
                }
            }
            match t.kind {
                K::Semi if depth == 0 => {
                    self.bump();
                    return;
                }
                K::LBrace => depth += 1,
                K::RBrace => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        return;
                    }
                }
                _ => {}
            }
            self.bump();
        }
    }

    /// Expects the `;` ending a statement or declaration. A missing `;` is
    /// reported just after the previous token; if the next token is on the
    /// same line, the rest of the line is skipped.
    fn semi(&mut self, after: &str) {
        if self.eat(K::Semi) {
            return;
        }
        let span = self.end_span();
        self.error_at(span, format!("expected `;` after {after}"));
        match self.tok(0) {
            None => {}
            Some(t) if t.nl_before || t.kind == K::RBrace => {}
            Some(_) => self.sync(),
        }
    }

    fn enter(&mut self) -> bool {
        if self.depth >= MAX_DEPTH {
            let span = self.cur_span();
            self.error_at(span, "nesting is too deep");
            self.skip_rest();
            return false;
        }
        self.depth += 1;
        true
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Extend a chain by one level. Callers restore `links` when the chain
    /// ends.
    fn link(&mut self) -> bool {
        if self.links >= MAX_LINKS {
            let span = self.cur_span();
            self.error_at(span, "expression is too long");
            self.skip_rest();
            return false;
        }
        self.links += 1;
        true
    }

    /// After a limit error, skip the rest of the expression or statement
    /// (up to `;` or an unbalanced closing bracket) rather than reporting
    /// every remaining operator or nested group.
    fn skip_rest(&mut self) {
        let mut depth = 0u32;
        while let Some(t) = self.tok(0) {
            match t.kind {
                K::LParen | K::LBracket | K::LBrace => depth += 1,
                K::RParen | K::RBracket | K::RBrace if depth == 0 => break,
                K::RParen | K::RBracket | K::RBrace => depth -= 1,
                K::Semi if depth == 0 => break,
                _ => {}
            }
            self.bump();
        }
        // Enclosing parts of the expression expecting more tokens here
        // would only repeat the error.
        self.last_err = self.pos;
    }

    /// A top-level-only keyword at the start of a line, which ends any
    /// unclosed body before it.
    fn at_top_level_keyword(&self) -> bool {
        match self.tok(0) {
            Some(t) if t.nl_before && t.kind == K::Ident => {
                TOP_LEVEL.contains(&t.text.as_str())
                    || (t.text == "error" && self.nth(1, K::LBrace))
            }
            _ => false,
        }
    }

    /// A token that cannot appear in a statement block and so ends it.
    fn at_block_end(&self) -> bool {
        let Some(t) = self.tok(0) else { return true };
        if t.kind == K::RBrace {
            return true;
        }
        if t.kind != K::Ident {
            return false;
        }
        match t.text.as_str() {
            "action" | "table" | "value_set" | "abstract" => true,
            "transition" => !self.in_state,
            "apply" => self.nth(1, K::LBrace) || self.nth(1, K::At),
            "state" => {
                self.tok(1).is_some_and(is_name_token) && self.nth(2, K::LBrace)
            }
            "error" => self.nth(1, K::LBrace),
            w => TOP_LEVEL.contains(&w),
        }
    }

    /// Reports an unclosed `{` at a point where the body must end.
    fn unclosed_brace(&mut self, open: Span) {
        let msg = format!("expected `}}`, found {}", self.found());
        let span = if self.eof() {
            self.end_span()
        } else {
            self.cur_span()
        };
        let d = Diagnostic::error(span, msg)
            .with_related(open, "unclosed `{`".to_string());
        self.push_error(d);
    }

    // -----------------------------------------------------------------------
    // Names and annotations

    fn missing_name(&self) -> Name {
        let s = self.cur_span();
        Name {
            text: String::new(),
            span: Span::new(s.file, s.start, s.start),
        }
    }

    fn name(&mut self, what: &str) -> Name {
        match self.tok(0) {
            Some(t) if is_name_token(t) => {
                self.bump();
                Name {
                    text: t.text.clone(),
                    span: t.span,
                }
            }
            _ => {
                let n = self.missing_name();
                self.expected(what);
                n
            }
        }
    }

    /// Any identifier, including keywords: member and property names.
    fn any_name(&mut self, what: &str) -> Name {
        match self.tok(0) {
            Some(t) if t.kind == K::Ident => {
                self.bump();
                Name {
                    text: t.text.clone(),
                    span: t.span,
                }
            }
            _ => {
                let n = self.missing_name();
                self.expected(what);
                n
            }
        }
    }

    fn path(&mut self, what: &str) -> Path {
        let m = self.mark();
        let global = self.eat(K::Dot);
        let name = self.name(what);
        Path {
            global,
            name,
            span: self.finish(m),
        }
    }

    fn annotations(&mut self) -> Vec<Annotation> {
        let mut v = Vec::new();
        while self.at(K::At) {
            v.push(self.annotation());
        }
        v
    }

    fn annotation(&mut self) -> Annotation {
        let m = self.mark();
        self.bump(); // @
        let mut name = self.any_name("annotation name");
        let mut kind = AnnotationKind::Plain;
        let mut body = Vec::new();
        let mut body_span = None;
        if name.text == "pragma" {
            kind = AnnotationKind::Pragma;
            if self
                .tok(0)
                .is_some_and(|t| t.kind == K::Ident && !t.nl_before)
            {
                name = self.any_name("pragma name");
            }
            let bm = self.mark();
            while let Some(t) = self.tok(0) {
                if t.nl_before {
                    break;
                }
                body.push(t.clone());
                self.bump();
            }
            if !body.is_empty() {
                body_span = Some(self.finish(bm));
            }
        } else if self.at(K::LParen) || self.at(K::LBracket) {
            let (open_k, close_k) = if self.at(K::LParen) {
                kind = AnnotationKind::Unstructured;
                (K::LParen, K::RParen)
            } else {
                kind = AnnotationKind::Structured;
                (K::LBracket, K::RBracket)
            };
            let open = self.cur_span();
            self.bump();
            let bm = self.mark();
            let mut depth = 0u32;
            let mut closed = false;
            while let Some(t) = self.tok(0) {
                if t.kind == close_k && depth == 0 {
                    closed = true;
                    break;
                }
                if self.at_top_level_keyword() {
                    break;
                }
                if t.kind == open_k {
                    depth += 1;
                } else if t.kind == close_k {
                    depth -= 1;
                }
                body.push(t.clone());
                self.bump();
            }
            body_span = Some(self.finish(bm));
            if closed {
                self.bump();
            } else {
                let text = if close_k == K::RParen { ")" } else { "]" };
                self.close(close_k, text, open);
            }
        }
        let span = self.finish(m);
        self.annotation_spans.push(span);
        Annotation {
            name,
            kind,
            body,
            body_span,
            span,
        }
    }

    // -----------------------------------------------------------------------
    // Top level

    fn program(&mut self) -> Program {
        let mut decls = Vec::new();
        while !self.eof() {
            if self.eat(K::Semi) {
                continue;
            }
            if self.at(K::RBrace) {
                self.expected("declaration");
                self.bump();
                continue;
            }
            let before = self.pos;
            let d = self.decl();
            let failed = matches!(d, Decl::Error(_));
            decls.push(d);
            if failed {
                self.sync_top();
            }
            if self.pos == before {
                self.bump();
            }
        }
        Program { decls }
    }

    /// Skips to the next top-level declaration after an error.
    fn sync_top(&mut self) {
        let mut depth = 0i32;
        let start = self.pos;
        while let Some(t) = self.tok(0) {
            if depth == 0 && self.pos > start {
                if t.kind == K::Ident
                    && (TOP_LEVEL.contains(&t.text.as_str())
                        || (t.nl_before
                            && matches!(t.text.as_str(), "const" | "action")))
                {
                    return;
                }
                if t.kind == K::At && t.nl_before {
                    return;
                }
            }
            match t.kind {
                K::Semi if depth == 0 => {
                    self.bump();
                    return;
                }
                K::LBrace => depth += 1,
                K::RBrace => {
                    depth -= 1;
                    if depth <= 0 {
                        self.bump();
                        return;
                    }
                }
                _ => {}
            }
            self.bump();
        }
    }

    /// Parses any declaration. Returns [`Decl::Error`] (after reporting) if
    /// the current tokens cannot begin one.
    fn decl(&mut self) -> Decl {
        let m = self.mark();
        let annos = self.annotations();
        match self.word() {
            "const" => Decl::Const(self.const_decl(m, annos)),
            "typedef" => self.typedef(m, annos, TypedefKind::Typedef),
            "type" if self.tok(1).is_some_and(|t| t.kind == K::Ident) => {
                self.typedef(m, annos, TypedefKind::Type)
            }
            "struct" | "header" | "header_union" => {
                Decl::Struct(self.struct_decl(m, annos))
            }
            "enum" => Decl::Enum(self.enum_decl(m, annos)),
            "error" if self.nth(1, K::LBrace) => {
                self.bump();
                Decl::Errors(self.member_list(m))
            }
            "match_kind" if self.nth(1, K::LBrace) => {
                self.bump();
                Decl::MatchKinds(self.member_list(m))
            }
            "extern" => self.extern_decl(m, annos),
            "action" => Decl::Action(self.action(m, annos)),
            "parser" => self.parser_decl(m, annos),
            "control" => self.control_decl(m, annos),
            "package" => {
                self.bump();
                let d = self.block_type(m, annos, "package name");
                self.semi("package declaration");
                Decl::Package(BlockTypeDecl {
                    span: self.finish(m),
                    ..d
                })
            }
            "table" => Decl::Table(self.table(m, annos)),
            "value_set" => Decl::ValueSet(self.value_set(m, annos)),
            _ if self.can_start_type() => self.typed_decl(m, annos),
            _ => {
                self.expected("declaration");
                Decl::Error(self.finish(m))
            }
        }
    }

    fn const_decl(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
    ) -> ConstDecl {
        self.bump(); // const
        let ty = self.ty();
        let name = self.name("constant name");
        let value = if self.expect(K::Eq, "=") {
            self.expr()
        } else {
            self.error_expr()
        };
        self.semi("constant declaration");
        ConstDecl {
            annotations,
            ty,
            name,
            value,
            span: self.finish(m),
        }
    }

    fn typedef(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
        kind: TypedefKind,
    ) -> Decl {
        self.bump(); // typedef / type
        let mut inline_decl = None;
        let ty = if matches!(
            self.word(),
            "struct" | "header" | "header_union" | "enum"
        ) {
            let im = self.mark();
            let d = if self.at_word("enum") {
                Decl::Enum(self.enum_decl(im, Vec::new()))
            } else {
                Decl::Struct(self.struct_decl(im, Vec::new()))
            };
            let name = d.name().cloned().unwrap_or_else(|| self.missing_name());
            let span = name.span;
            inline_decl = Some(Box::new(d));
            Type {
                kind: TypeKind::Named {
                    path: Path {
                        global: false,
                        name,
                        span,
                    },
                    args: Vec::new(),
                },
                span,
            }
        } else {
            self.ty()
        };
        let name = self.name("type name");
        self.types.insert(name.text.clone());
        self.semi("type declaration");
        Decl::Typedef(TypedefDecl {
            annotations,
            kind,
            ty,
            inline_decl,
            name,
            span: self.finish(m),
        })
    }

    fn type_params(&mut self) -> Vec<Name> {
        let mut v = Vec::new();
        if !self.at(K::Lt) {
            return v;
        }
        let open = self.cur_span();
        self.bump();
        while !self.eof() && !self.at(K::Gt) {
            let before = self.pos;
            let n = self.name("type parameter name");
            if !n.is_missing() {
                self.types.insert(n.text.clone());
                v.push(n);
            }
            if !self.eat(K::Comma) || self.pos == before {
                break;
            }
        }
        self.close(K::Gt, ">", open);
        v
    }

    fn struct_decl(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
    ) -> StructDecl {
        let kind = match self.word() {
            "header" => StructKind::Header,
            "header_union" => StructKind::HeaderUnion,
            _ => StructKind::Struct,
        };
        self.bump();
        let name = self.name("type name");
        self.types.insert(name.text.clone());
        let type_params = self.type_params();
        let mut fields = Vec::new();
        if self.at(K::LBrace) {
            let open = self.cur_span();
            self.bump();
            loop {
                if self.eat(K::RBrace) {
                    break;
                }
                if self.eof() || self.at_top_level_keyword() {
                    self.unclosed_brace(open);
                    break;
                }
                let before = self.pos;
                let fm = self.mark();
                let annotations = self.annotations();
                if self.can_start_type() {
                    let ty = self.ty();
                    let name = self.name("field name");
                    let ty = self.declarator_suffix(ty);
                    self.semi("field");
                    fields.push(Field {
                        annotations,
                        ty,
                        name,
                        span: self.finish(fm),
                    });
                } else {
                    self.expected("field");
                    self.sync();
                }
                if self.pos == before {
                    self.bump();
                }
            }
        } else {
            self.expected("`{`");
        }
        StructDecl {
            annotations,
            kind,
            name,
            type_params,
            fields,
            span: self.finish(m),
        }
    }

    /// `name[N]` declarators, an old form of header stack declarations.
    fn declarator_suffix(&mut self, mut ty: Type) -> Type {
        let saved = self.links;
        while self.at(K::LBracket) && self.link() {
            let open = self.cur_span();
            self.bump();
            let size = self.nested_expr();
            self.close(K::RBracket, "]", open);
            let span = ty.span.to(self.toks[self.pos - 1].span);
            ty = Type {
                kind: TypeKind::Stack {
                    elem: Box::new(ty),
                    size: Box::new(size),
                },
                span,
            };
        }
        self.links = saved;
        ty
    }

    fn enum_decl(&mut self, m: Mark, annotations: Vec<Annotation>) -> EnumDecl {
        self.bump(); // enum
        let repr = if self.at_name() && self.nth(1, K::LBrace) {
            None
        } else {
            Some(self.ty())
        };
        let name = self.name("enum name");
        self.types.insert(name.text.clone());
        let mut members = Vec::new();
        if self.at(K::LBrace) {
            let open = self.cur_span();
            self.bump();
            while !self.eof() && !self.at(K::RBrace) {
                if self.at_top_level_keyword() {
                    break;
                }
                let before = self.pos;
                let mm = self.mark();
                let name = self.name("enum member");
                let value = if self.eat(K::Eq) {
                    Some(self.expr())
                } else {
                    None
                };
                if !name.is_missing() {
                    members.push(EnumMember {
                        name,
                        value,
                        span: self.finish(mm),
                    });
                }
                if !self.eat(K::Comma) {
                    if self.pos == before || self.at(K::RBrace) {
                        break;
                    }
                    self.expected("`,` or `}`");
                    if !self.at_name() {
                        break;
                    }
                }
            }
            if !self.eat(K::RBrace) {
                self.unclosed_brace(open);
            }
        } else {
            self.expected("`{`");
        }
        EnumDecl {
            annotations,
            repr,
            name,
            members,
            span: self.finish(m),
        }
    }

    /// `{ A, B, ... }` after `error` or `match_kind`.
    fn member_list(&mut self, m: Mark) -> MemberListDecl {
        let open = self.cur_span();
        self.bump(); // {
        let mut members = Vec::new();
        while !self.eof() && !self.at(K::RBrace) {
            if self.at_top_level_keyword() {
                break;
            }
            let before = self.pos;
            let n = self.name("name");
            if !n.is_missing() {
                members.push(n);
            }
            if !self.eat(K::Comma) {
                if self.pos == before || self.at(K::RBrace) {
                    break;
                }
                self.expected("`,` or `}`");
                if !self.at_name() {
                    break;
                }
            }
        }
        if !self.eat(K::RBrace) {
            self.unclosed_brace(open);
        }
        MemberListDecl {
            members,
            span: self.finish(m),
        }
    }

    fn extern_decl(&mut self, m: Mark, annotations: Vec<Annotation>) -> Decl {
        self.bump(); // extern
                     // `extern E<T> {` or `extern E;` declare an object; anything else is
                     // a function prototype.
        let is_object = self.lookahead(|p| {
            if !p.at_name() {
                return false;
            }
            p.bump();
            if p.at(K::Lt) {
                p.bump();
                while p.at_name() || p.at(K::Comma) {
                    p.bump();
                }
                if !p.eat(K::Gt) {
                    return false;
                }
            }
            p.at(K::LBrace) || p.at(K::Semi)
        });
        if !is_object {
            let proto = self.function_proto(m, annotations);
            self.semi("function declaration");
            return Decl::ExternFunction(FunctionProto {
                span: self.finish(m),
                ..proto
            });
        }
        let name = self.name("extern name");
        self.types.insert(name.text.clone());
        let type_params = self.type_params();
        let mut methods = Vec::new();
        let mut forward = false;
        if self.eat(K::Semi) {
            forward = true;
        } else {
            let open = self.cur_span();
            self.bump(); // {
            loop {
                if self.eat(K::RBrace) {
                    break;
                }
                if self.eof() || self.at_top_level_keyword() {
                    self.unclosed_brace(open);
                    break;
                }
                if self.eat(K::Semi) {
                    continue;
                }
                let before = self.pos;
                match self.method(&name.text) {
                    Some(md) => methods.push(md),
                    None => self.sync(),
                }
                if self.pos == before {
                    self.bump();
                }
            }
        }
        Decl::Extern(ExternDecl {
            annotations,
            name,
            type_params,
            methods,
            forward,
            span: self.finish(m),
        })
    }

    fn method(&mut self, extern_name: &str) -> Option<MethodDecl> {
        let m = self.mark();
        let annotations = self.annotations();
        let is_abstract = self.eat_word("abstract");
        if self.at_word(extern_name) && self.nth(1, K::LParen) {
            let name = self.name("constructor name");
            let params = self.params();
            self.semi("constructor declaration");
            return Some(MethodDecl {
                annotations,
                is_abstract,
                ret: None,
                name,
                type_params: Vec::new(),
                params,
                span: self.finish(m),
            });
        }
        if !self.can_start_type() {
            self.expected("method declaration");
            return None;
        }
        let proto = self.function_proto(m, annotations);
        self.semi("method declaration");
        Some(MethodDecl {
            annotations: proto.annotations,
            is_abstract,
            ret: Some(proto.ret),
            name: proto.name,
            type_params: proto.type_params,
            params: proto.params,
            span: self.finish(m),
        })
    }

    /// `T name<P>(params)`.
    fn function_proto(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
    ) -> FunctionProto {
        let ret = self.ty();
        let name = self.name("function name");
        self.function_rest(m, annotations, ret, name)
    }

    fn function_rest(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
        ret: Type,
        name: Name,
    ) -> FunctionProto {
        let type_params = self.type_params();
        let params = self.params();
        FunctionProto {
            annotations,
            ret,
            name,
            type_params,
            params,
            span: self.finish(m),
        }
    }

    fn params(&mut self) -> Vec<Param> {
        let mut v = Vec::new();
        if !self.at(K::LParen) {
            self.expected("`(`");
            return v;
        }
        let open = self.cur_span();
        self.bump();
        while !self.eof() && !self.at(K::RParen) {
            let before = self.pos;
            v.push(self.param());
            if !self.eat(K::Comma) || self.pos == before {
                break;
            }
        }
        self.close(K::RParen, ")", open);
        v
    }

    fn param(&mut self) -> Param {
        let m = self.mark();
        let annotations = self.annotations();
        let direction = match self.word() {
            "in" => Direction::In,
            "out" => Direction::Out,
            "inout" => Direction::InOut,
            _ => Direction::None,
        };
        if direction != Direction::None {
            self.bump();
        }
        let ty = self.ty();
        let name = self.name("parameter name");
        let default = if self.eat(K::Eq) {
            Some(self.expr())
        } else {
            None
        };
        Param {
            annotations,
            direction,
            ty,
            name,
            default,
            span: self.finish(m),
        }
    }

    fn action(&mut self, m: Mark, annotations: Vec<Annotation>) -> ActionDecl {
        self.bump(); // action
        let name = self.name("action name");
        let params = self.params();
        let body = self.block(Vec::new());
        ActionDecl {
            annotations,
            name,
            params,
            body,
            span: self.finish(m),
        }
    }

    /// The common head of parser, control, and package declarations, after
    /// the keyword.
    fn block_type(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
        what: &str,
    ) -> BlockTypeDecl {
        let name = self.name(what);
        self.types.insert(name.text.clone());
        let type_params = self.type_params();
        let params = self.params();
        BlockTypeDecl {
            annotations,
            name,
            type_params,
            params,
            span: self.finish(m),
        }
    }

    fn ctor_params(&mut self) -> Vec<Param> {
        if self.at(K::LParen) {
            self.params()
        } else {
            Vec::new()
        }
    }

    fn parser_decl(&mut self, m: Mark, annotations: Vec<Annotation>) -> Decl {
        self.bump(); // parser
        let head = self.block_type(m, annotations, "parser name");
        if self.eat(K::Semi) {
            return Decl::ParserType(BlockTypeDecl {
                span: self.finish(m),
                ..head
            });
        }
        let ctor_params = self.ctor_params();
        let mut locals = Vec::new();
        let mut states = Vec::new();
        if self.at(K::LBrace) {
            let open = self.cur_span();
            self.bump();
            loop {
                if self.eat(K::RBrace) {
                    break;
                }
                if self.eof() || self.at_top_level_keyword() {
                    self.unclosed_brace(open);
                    break;
                }
                if self.eat(K::Semi) {
                    continue;
                }
                let before = self.pos;
                let lm = self.mark();
                let annos = self.annotations();
                if self.at_word("state")
                    && self.tok(1).is_some_and(|t| t.kind == K::Ident)
                {
                    states.push(self.state(lm, annos));
                } else {
                    let d = self.local_decl(lm, annos);
                    locals.push(d);
                }
                if self.pos == before {
                    self.bump();
                }
            }
        } else {
            self.expected("`{` or `;`");
        }
        let BlockTypeDecl {
            annotations,
            name,
            type_params,
            params,
            ..
        } = head;
        Decl::Parser(ParserDecl {
            annotations,
            name,
            type_params,
            params,
            ctor_params,
            locals,
            states,
            span: self.finish(m),
        })
    }

    /// A parser or control local declaration.
    fn local_decl(&mut self, m: Mark, annos: Vec<Annotation>) -> Decl {
        match self.word() {
            "const" => Decl::Const(self.const_decl(m, annos)),
            "action" => Decl::Action(self.action(m, annos)),
            "table" => Decl::Table(self.table(m, annos)),
            "value_set" => Decl::ValueSet(self.value_set(m, annos)),
            _ if self.can_start_type() && self.looks_like_decl() => {
                self.typed_decl(m, annos)
            }
            _ => {
                self.expected("declaration");
                self.sync();
                Decl::Error(self.finish(m))
            }
        }
    }

    fn state(&mut self, m: Mark, annotations: Vec<Annotation>) -> ParserState {
        self.bump(); // state
        let name = self.any_name("state name");
        let mut body = Vec::new();
        let mut transition = None;
        if self.at(K::LBrace) {
            let open = self.cur_span();
            self.bump();
            let in_state = std::mem::replace(&mut self.in_state, true);
            loop {
                if self.eat(K::RBrace) {
                    break;
                }
                if self.at_word("transition") {
                    let t = self.transition();
                    if transition.is_some() {
                        self.error_at(t.span, "duplicate transition");
                    }
                    transition = Some(t);
                    continue;
                }
                if self.at_block_end() {
                    self.unclosed_brace(open);
                    break;
                }
                let before = self.pos;
                if transition.is_some() {
                    let span = self.cur_span();
                    self.error_at(span, "statement after transition");
                }
                body.push(self.stmt());
                if self.pos == before {
                    self.bump();
                }
            }
            self.in_state = in_state;
        } else {
            self.expected("`{`");
        }
        ParserState {
            annotations,
            name,
            body,
            transition,
            span: self.finish(m),
        }
    }

    fn transition(&mut self) -> Transition {
        let m = self.mark();
        self.bump(); // transition
        let kind = if self.at_word("select") {
            TransitionKind::Select(self.select())
        } else {
            let name = self.any_name("state name");
            self.semi("transition");
            TransitionKind::Direct(name)
        };
        Transition {
            kind,
            span: self.finish(m),
        }
    }

    fn select(&mut self) -> Select {
        let m = self.mark();
        self.bump(); // select
        let mut exprs = Vec::new();
        if self.at(K::LParen) {
            let open = self.cur_span();
            self.bump();
            exprs = self.expr_list(K::RParen);
            self.close(K::RParen, ")", open);
        } else {
            self.expected("`(`");
        }
        let mut cases = Vec::new();
        if self.at(K::LBrace) {
            let open = self.cur_span();
            self.bump();
            loop {
                if self.eat(K::RBrace) {
                    break;
                }
                if self.at_block_end() {
                    self.unclosed_brace(open);
                    break;
                }
                let before = self.pos;
                let cm = self.mark();
                let keyset = self.keyset();
                let state = if self.expect(K::Colon, ":") {
                    self.any_name("state name")
                } else {
                    self.missing_name()
                };
                self.semi("select case");
                cases.push(SelectCase {
                    keyset,
                    state,
                    span: self.finish(cm),
                });
                if self.pos == before {
                    self.bump();
                }
            }
            self.eat(K::Semi);
        } else {
            self.expected("`{`");
        }
        Select {
            exprs,
            cases,
            span: self.finish(m),
        }
    }

    fn keyset(&mut self) -> Keyset {
        if self.at(K::LParen) {
            let tuple = self.speculate(|p| {
                let m = p.mark();
                p.bump();
                let mut v = Vec::new();
                loop {
                    v.push(p.simple_keyset());
                    if !p.eat(K::Comma) {
                        break;
                    }
                }
                if !p.eat(K::RParen) || !p.at(K::Colon) {
                    return None;
                }
                let span = p.finish(m);
                if v.len() == 1 {
                    // `(e)` is a parenthesized expression; `(a &&& b)` is a
                    // one-element tuple in p4c, but means the same.
                    return v.pop().map(|k| Keyset { span, ..k });
                }
                Some(Keyset {
                    kind: KeysetKind::Tuple(v),
                    span,
                })
            });
            if let Some(k) = tuple {
                return k;
            }
        }
        self.simple_keyset()
    }

    fn simple_keyset(&mut self) -> Keyset {
        let m = self.mark();
        if self.eat_word("default") {
            return Keyset {
                kind: KeysetKind::Default,
                span: self.finish(m),
            };
        }
        if self.at_word("_")
            && (self.nth(1, K::Comma)
                || self.nth(1, K::RParen)
                || self.nth(1, K::Colon))
        {
            self.bump();
            return Keyset {
                kind: KeysetKind::DontCare,
                span: self.finish(m),
            };
        }
        let e = self.expr();
        let kind = if self.eat(K::AmpAmpAmp) {
            KeysetKind::Mask(e, self.expr())
        } else if self.at(K::DotDot) && !self.joined(1, K::Dot) {
            self.bump();
            KeysetKind::Range(e, self.expr())
        } else {
            KeysetKind::Expr(e)
        };
        Keyset {
            kind,
            span: self.finish(m),
        }
    }

    fn value_set(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
    ) -> ValueSetDecl {
        self.bump(); // value_set
        let elem_ty = if self.at(K::Lt) {
            let open = self.cur_span();
            self.bump();
            let t = self.ty();
            self.close(K::Gt, ">", open);
            t
        } else {
            self.expected("`<`");
            self.error_type()
        };
        let size = if self.at(K::LParen) {
            let open = self.cur_span();
            self.bump();
            let e = self.nested_expr();
            self.close(K::RParen, ")", open);
            e
        } else {
            self.expected("`(`");
            self.error_expr()
        };
        let name = self.name("value set name");
        self.semi("value set declaration");
        ValueSetDecl {
            annotations,
            elem_ty,
            size,
            name,
            span: self.finish(m),
        }
    }

    fn control_decl(&mut self, m: Mark, annotations: Vec<Annotation>) -> Decl {
        self.bump(); // control
        let head = self.block_type(m, annotations, "control name");
        if self.eat(K::Semi) {
            return Decl::ControlType(BlockTypeDecl {
                span: self.finish(m),
                ..head
            });
        }
        let ctor_params = self.ctor_params();
        let mut locals = Vec::new();
        let mut apply = None;
        if self.at(K::LBrace) {
            let open = self.cur_span();
            self.bump();
            loop {
                if self.eat(K::RBrace) {
                    break;
                }
                if self.eof() || self.at_top_level_keyword() {
                    self.unclosed_brace(open);
                    break;
                }
                if self.eat(K::Semi) {
                    continue;
                }
                let before = self.pos;
                let lm = self.mark();
                let mut annos = self.annotations();
                if self.at_word("apply")
                    && (self.nth(1, K::LBrace) || self.nth(1, K::At))
                {
                    self.bump();
                    annos.extend(self.annotations());
                    let b = self.block(annos);
                    if apply.is_some() {
                        self.error_at(b.span, "duplicate `apply` block");
                    } else {
                        apply = Some(b);
                    }
                } else {
                    locals.push(self.local_decl(lm, annos));
                }
                if self.pos == before {
                    self.bump();
                }
            }
        } else {
            self.expected("`{` or `;`");
        }
        let BlockTypeDecl {
            annotations,
            name,
            type_params,
            params,
            ..
        } = head;
        Decl::Control(ControlDecl {
            annotations,
            name,
            type_params,
            params,
            ctor_params,
            locals,
            apply,
            span: self.finish(m),
        })
    }

    fn table(&mut self, m: Mark, annotations: Vec<Annotation>) -> TableDecl {
        self.bump(); // table
        let name = self.name("table name");
        let mut properties = Vec::new();
        if self.at(K::LBrace) {
            let open = self.cur_span();
            self.bump();
            loop {
                if self.eat(K::RBrace) {
                    break;
                }
                if self.at_block_end() && !self.at_word("abstract") {
                    self.unclosed_brace(open);
                    break;
                }
                if self.eat(K::Semi) {
                    continue;
                }
                let before = self.pos;
                match self.table_property() {
                    Some(p) => properties.push(p),
                    None => self.sync(),
                }
                if self.pos == before {
                    self.bump();
                }
            }
        } else {
            self.expected("`{`");
        }
        TableDecl {
            annotations,
            name,
            properties,
            span: self.finish(m),
        }
    }

    fn table_property(&mut self) -> Option<TableProperty> {
        let m = self.mark();
        let annotations = self.annotations();
        let is_const = self.eat_word("const");
        if !self.at(K::Ident) {
            self.expected("table property");
            return None;
        }
        let name = self.any_name("property name");
        if !self.expect(K::Eq, "=") {
            return None;
        }
        let braced = self.at(K::LBrace);
        let value = match name.text.as_str() {
            "key" if braced => PropertyValue::Key(self.key_elements()),
            "actions" if braced => PropertyValue::Actions(self.action_list()),
            "entries" if braced => PropertyValue::Entries(self.entries()),
            _ => {
                let e = self.expr();
                self.semi("table property");
                PropertyValue::Expr(e)
            }
        };
        if braced && !matches!(value, PropertyValue::Expr(_)) {
            self.eat(K::Semi);
        }
        Some(TableProperty {
            annotations,
            is_const,
            name,
            value,
            span: self.finish(m),
        })
    }

    /// Parses `{ item* }` where each item is parsed by `f`, which returns
    /// `None` after reporting an error.
    fn braced_items<T>(
        &mut self,
        mut f: impl FnMut(&mut Self) -> Option<T>,
    ) -> Vec<T> {
        let mut v = Vec::new();
        let open = self.cur_span();
        self.bump(); // {
        loop {
            if self.eat(K::RBrace) {
                break;
            }
            if self.at_block_end() {
                self.unclosed_brace(open);
                break;
            }
            if self.eat(K::Semi) {
                continue;
            }
            let before = self.pos;
            match f(self) {
                Some(x) => v.push(x),
                None => self.sync(),
            }
            if self.pos == before {
                self.bump();
            }
        }
        v
    }

    fn key_elements(&mut self) -> Vec<KeyElement> {
        self.braced_items(|p| {
            let m = p.mark();
            let expr = p.expr();
            if matches!(expr.kind, ExprKind::Error) {
                return None;
            }
            let match_kind = if p.expect(K::Colon, ":") {
                p.any_name("match kind")
            } else {
                p.missing_name()
            };
            let annotations = p.annotations();
            p.semi("key element");
            Some(KeyElement {
                expr,
                match_kind,
                annotations,
                span: p.finish(m),
            })
        })
    }

    fn action_ref(
        &mut self,
        annotations: Vec<Annotation>,
        m: Mark,
    ) -> ActionRef {
        let name = self.path("action name");
        let args = if self.at(K::LParen) {
            Some(self.args())
        } else {
            None
        };
        ActionRef {
            annotations,
            name,
            args,
            span: self.finish(m),
        }
    }

    fn action_list(&mut self) -> Vec<ActionRef> {
        self.braced_items(|p| {
            let m = p.mark();
            let annotations = p.annotations();
            if !p.at_name() && !p.at(K::Dot) {
                p.expected("action name");
                return None;
            }
            let r = p.action_ref(annotations, m);
            p.semi("action");
            Some(r)
        })
    }

    fn entries(&mut self) -> Vec<Entry> {
        self.braced_items(|p| {
            let m = p.mark();
            let is_const = p.eat_word("const");
            let priority = if p.at_word("priority") && p.nth(1, K::Eq) {
                p.bump_n(2);
                let e = p.unary();
                p.expect(K::Colon, ":");
                Some(e)
            } else {
                None
            };
            let keyset = p.keyset();
            if matches!(&keyset.kind, KeysetKind::Expr(e) if matches!(e.kind, ExprKind::Error))
            {
                return None;
            }
            p.expect(K::Colon, ":");
            let am = p.mark();
            let action = p.action_ref(Vec::new(), am);
            let annotations = p.annotations();
            p.semi("entry");
            Some(Entry {
                annotations,
                is_const,
                priority,
                keyset,
                action,
                span: p.finish(m),
            })
        })
    }

    /// Whether the tokens ahead form a declaration that starts with a type:
    /// `T x ...` or `T(args) x ...`.
    fn looks_like_decl(&mut self) -> bool {
        self.lookahead(|p| {
            if !p.can_start_type() {
                return false;
            }
            p.ty();
            if p.at(K::LParen) {
                p.skip_balanced();
            }
            p.at_name()
        })
    }

    /// Skips a balanced parenthesized group at the current `(`.
    fn skip_balanced(&mut self) {
        let mut depth = 0i32;
        while let Some(t) = self.tok(0) {
            match t.kind {
                K::LParen => depth += 1,
                K::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        return;
                    }
                }
                K::LBrace | K::LBracket => depth += 1,
                K::RBrace | K::RBracket => {
                    depth -= 1;
                    if depth <= 0 {
                        return;
                    }
                }
                K::Semi => return,
                _ => {}
            }
            self.bump();
        }
    }

    /// A declaration starting with a type: a variable, an instantiation, or
    /// a function.
    fn typed_decl(&mut self, m: Mark, annotations: Vec<Annotation>) -> Decl {
        let ty = self.ty();
        if self.at(K::LParen) {
            return Decl::Instantiation(self.instantiation(m, annotations, ty));
        }
        let name = self.name("name");
        if name.is_missing() {
            self.sync();
            return Decl::Error(self.finish(m));
        }
        if self.at(K::LParen) || self.at(K::Lt) {
            let proto = self.function_rest(m, annotations, ty, name);
            let body = if self.at(K::LBrace) {
                self.block(Vec::new())
            } else {
                self.expected("function body");
                if self.at(K::Semi) {
                    self.bump();
                }
                Block {
                    annotations: Vec::new(),
                    stmts: Vec::new(),
                    span: self.end_span(),
                }
            };
            return Decl::Function(FunctionDecl {
                proto,
                body,
                span: self.finish(m),
            });
        }
        Decl::Var(self.var_rest(m, annotations, ty, name, true))
    }

    fn var_rest(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
        ty: Type,
        name: Name,
        semi: bool,
    ) -> VarDecl {
        let ty = self.declarator_suffix(ty);
        let init = if self.eat(K::Eq) {
            Some(self.expr())
        } else {
            None
        };
        if semi {
            self.semi("variable declaration");
        }
        VarDecl {
            annotations,
            ty,
            name,
            init,
            span: self.finish(m),
        }
    }

    fn instantiation(
        &mut self,
        m: Mark,
        annotations: Vec<Annotation>,
        ty: Type,
    ) -> Instantiation {
        let args = self.args();
        let name = self.name("instance name");
        // `E() a[N];` declares an array of instances.
        let ty = self.declarator_suffix(ty);
        let initializer = if self.at(K::Eq) && self.nth(1, K::LBrace) {
            self.bump();
            let open = self.cur_span();
            self.bump();
            let mut decls = Vec::new();
            loop {
                if self.eat(K::RBrace) {
                    break;
                }
                if self.eof() || self.at_top_level_keyword() {
                    self.unclosed_brace(open);
                    break;
                }
                if self.eat(K::Semi) {
                    continue;
                }
                let before = self.pos;
                let dm = self.mark();
                let annos = self.annotations();
                if self.can_start_type() {
                    decls.push(self.typed_decl(dm, annos));
                } else {
                    self.expected("function or instance declaration");
                    self.sync();
                }
                if self.pos == before {
                    self.bump();
                }
            }
            Some(decls)
        } else {
            None
        };
        self.semi("instantiation");
        Instantiation {
            annotations,
            ty,
            args,
            name,
            initializer,
            span: self.finish(m),
        }
    }

    // -----------------------------------------------------------------------
    // Statements

    fn block(&mut self, annotations: Vec<Annotation>) -> Block {
        let m = self.mark();
        let mut stmts = Vec::new();
        if !self.at(K::LBrace) {
            self.expected("`{`");
            return Block {
                annotations,
                stmts,
                span: self.finish(m),
            };
        }
        let open = self.cur_span();
        self.bump();
        loop {
            if self.eat(K::RBrace) {
                break;
            }
            if self.at_block_end() {
                self.unclosed_brace(open);
                break;
            }
            let before = self.pos;
            stmts.push(self.stmt());
            if self.pos == before {
                self.bump();
            }
        }
        Block {
            annotations,
            stmts,
            span: self.finish(m),
        }
    }

    fn stmt(&mut self) -> Stmt {
        let m = self.mark();
        let start = self.pos;
        if !self.enter() {
            // Make progress at a `;`; a closing brace belongs to the caller.
            if self.pos == start && !self.at(K::RBrace) {
                self.bump();
            }
            return Stmt {
                kind: StmtKind::Error,
                span: self.finish(m),
            };
        }
        let kind = self.stmt_kind(m);
        self.leave();
        Stmt {
            kind,
            span: self.finish(m),
        }
    }

    fn stmt_kind(&mut self, m: Mark) -> StmtKind {
        let annos = self.annotations();
        if self.eat(K::Semi) {
            return StmtKind::Empty;
        }
        if self.at(K::LBrace) {
            return StmtKind::Block(self.block(annos));
        }
        match self.word() {
            "if" => {
                self.bump();
                let cond = self.paren_cond();
                let then = Box::new(self.sub_stmt());
                let els = if self.eat_word("else") {
                    Some(Box::new(self.sub_stmt()))
                } else {
                    None
                };
                StmtKind::If { cond, then, els }
            }
            "switch" => self.switch(),
            "return" => {
                self.bump();
                let value = if self.at(K::Semi) || self.at(K::RBrace) {
                    None
                } else {
                    Some(self.expr())
                };
                self.semi("return statement");
                StmtKind::Return(value)
            }
            "exit" => {
                self.bump();
                self.semi("exit statement");
                StmtKind::Exit
            }
            "break" => {
                self.bump();
                self.semi("break statement");
                StmtKind::Break
            }
            "continue" => {
                self.bump();
                self.semi("continue statement");
                StmtKind::Continue
            }
            "for" => self.for_stmt(annos),
            "transition" if self.in_state => {
                StmtKind::Transition(self.transition())
            }
            "const" => StmtKind::Decl(Decl::Const(self.const_decl(m, annos))),
            _ => {
                if self.at(K::RParen) || self.at(K::RBracket) {
                    // A stray closer: skip just it.
                    self.expected("statement");
                    self.bump();
                    return StmtKind::Error;
                }
                if self.can_start_type() && self.looks_like_decl() {
                    return StmtKind::Decl(self.typed_decl(m, annos));
                }
                let kind = self.simple_stmt_kind();
                if matches!(kind, StmtKind::Error) {
                    self.sync();
                } else {
                    self.semi("statement");
                }
                kind
            }
        }
    }

    /// The statement after `if (...)`, `else`, or a loop header.
    fn sub_stmt(&mut self) -> Stmt {
        if self.at_block_end() {
            let m = self.mark();
            self.expected("statement");
            return Stmt {
                kind: StmtKind::Error,
                span: self.finish(m),
            };
        }
        self.stmt()
    }

    /// `( expr )` after `if` or `switch`.
    fn paren_cond(&mut self) -> Expr {
        if !self.at(K::LParen) {
            self.expected("`(`");
            return self.expr();
        }
        let open = self.cur_span();
        self.bump();
        let e = self.nested_expr();
        self.close(K::RParen, ")", open);
        e
    }

    /// An assignment or call without the trailing `;`.
    fn simple_stmt_kind(&mut self) -> StmtKind {
        let lhs = self.expr();
        if matches!(lhs.kind, ExprKind::Error) {
            return StmtKind::Error;
        }
        if let Some((op, n)) = self.peek_assign() {
            self.bump_n(n);
            let rhs = self.expr();
            return StmtKind::Assign { lhs, op, rhs };
        }
        if matches!(lhs.kind, ExprKind::Call { .. }) {
            return StmtKind::Call(lhs);
        }
        if self.at(K::Semi) || self.tok(0).is_none_or(|t| t.nl_before) {
            self.error_at(lhs.span, "expected assignment or function call");
        } else {
            self.expected("`=`, `(`, or `;`");
        }
        StmtKind::Expr(lhs)
    }

    fn peek_assign(&self) -> Option<(AssignOp, usize)> {
        let t = self.tok(0)?;
        let op = match t.kind {
            K::Eq => return Some((AssignOp::Assign, 1)),
            K::Gt if self.joined(1, K::Gt) && self.joined(2, K::Eq) => {
                return Some((AssignOp::Shr, 3))
            }
            K::Plus => AssignOp::Add,
            K::Minus => AssignOp::Sub,
            K::PlusSat => AssignOp::AddSat,
            K::MinusSat => AssignOp::SubSat,
            K::Star => AssignOp::Mul,
            K::Slash => AssignOp::Div,
            K::Percent => AssignOp::Mod,
            K::Shl => AssignOp::Shl,
            K::Amp => AssignOp::BitAnd,
            K::Pipe => AssignOp::BitOr,
            K::Caret => AssignOp::BitXor,
            _ => return None,
        };
        self.joined(1, K::Eq).then_some((op, 2))
    }

    fn switch(&mut self) -> StmtKind {
        self.bump(); // switch
        let expr = self.paren_cond();
        let mut cases = Vec::new();
        if !self.at(K::LBrace) {
            self.expected("`{`");
            return StmtKind::Switch { expr, cases };
        }
        let open = self.cur_span();
        self.bump();
        loop {
            if self.eat(K::RBrace) {
                break;
            }
            if self.at_block_end() {
                self.unclosed_brace(open);
                break;
            }
            let before = self.pos;
            let m = self.mark();
            let label = if self.at_word("default") {
                let s = self.cur_span();
                self.bump();
                SwitchLabel::Default(s)
            } else {
                SwitchLabel::Expr(self.expr())
            };
            if self.pos == before {
                self.expected("switch label");
                self.sync();
                continue;
            }
            if !self.expect(K::Colon, ":") {
                self.sync();
            }
            let body = if self.at(K::LBrace) {
                Some(self.block(Vec::new()))
            } else {
                None
            };
            cases.push(SwitchCase {
                label,
                body,
                span: self.finish(m),
            });
            if self.pos == before {
                self.bump();
            }
        }
        StmtKind::Switch { expr, cases }
    }

    fn for_stmt(&mut self, annotations: Vec<Annotation>) -> StmtKind {
        self.bump(); // for
        let open = self.cur_span();
        if !self.expect(K::LParen, "(") {
            self.sync();
            return StmtKind::Error;
        }
        let is_in = self.lookahead(|p| {
            p.annotations();
            p.can_start_type()
                && {
                    p.ty();
                    p.at_name()
                }
                && {
                    p.bump();
                    p.at_word("in")
                }
        });
        if is_in {
            let m = self.mark();
            let annos = self.annotations();
            let ty = self.ty();
            let name = self.name("loop variable");
            let var = VarDecl {
                annotations: annos,
                ty,
                name,
                init: None,
                span: self.finish(m),
            };
            self.bump(); // in
            let em = self.mark();
            let mut collection = self.nested_expr();
            if self.at(K::DotDot) {
                self.bump();
                let hi = self.nested_expr();
                collection = Expr {
                    kind: ExprKind::Binary {
                        op: BinOp::Range,
                        lhs: Box::new(collection),
                        rhs: Box::new(hi),
                    },
                    span: self.finish(em),
                };
            }
            self.close(K::RParen, ")", open);
            let body = Box::new(self.sub_stmt());
            return StmtKind::ForIn {
                annotations,
                var,
                collection,
                body,
            };
        }
        let init = self.for_clauses(K::Semi);
        self.expect(K::Semi, ";");
        let cond = if self.at(K::Semi) {
            None
        } else {
            Some(self.nested_expr())
        };
        self.expect(K::Semi, ";");
        let update = self.for_clauses(K::RParen);
        self.close(K::RParen, ")", open);
        let body = Box::new(self.sub_stmt());
        StmtKind::For {
            annotations,
            init,
            cond,
            update,
            body,
        }
    }

    fn for_clauses(&mut self, end: K) -> Vec<Stmt> {
        let mut v = Vec::new();
        while !self.eof() && !self.at(end) {
            let before = self.pos;
            let m = self.mark();
            let kind = if self.can_start_type() && self.looks_like_decl() {
                let annos = Vec::new();
                let ty = self.ty();
                let name = self.name("variable name");
                StmtKind::Decl(Decl::Var(
                    self.var_rest(m, annos, ty, name, false),
                ))
            } else {
                self.simple_stmt_kind()
            };
            v.push(Stmt {
                kind,
                span: self.finish(m),
            });
            if !self.eat(K::Comma) || self.pos == before {
                break;
            }
        }
        v
    }

    // -----------------------------------------------------------------------
    // Types

    fn error_type(&self) -> Type {
        let s = self.cur_span();
        Type {
            kind: TypeKind::Error,
            span: Span::new(s.file, s.start, s.start),
        }
    }

    fn can_start_type(&self) -> bool {
        match self.tok(0) {
            Some(t) if t.kind == K::Dot => {
                self.tok(1).is_some_and(is_name_token)
            }
            Some(t) if t.kind == K::Ident => {
                is_name_token(t)
                    || matches!(
                        t.text.as_str(),
                        "bit"
                            | "int"
                            | "varbit"
                            | "bool"
                            | "string"
                            | "void"
                            | "error"
                            | "match_kind"
                            | "tuple"
                            | "_"
                    )
            }
            _ => false,
        }
    }

    fn ty(&mut self) -> Type {
        let m = self.mark();
        if !self.enter() {
            return self.error_type();
        }
        let mut t = self.base_type();
        let saved = self.links;
        while self.at(K::LBracket)
            && !matches!(t.kind, TypeKind::Error)
            && self.link()
        {
            let open = self.cur_span();
            self.bump();
            let size = self.nested_expr();
            self.close(K::RBracket, "]", open);
            t = Type {
                kind: TypeKind::Stack {
                    elem: Box::new(t),
                    size: Box::new(size),
                },
                span: self.finish(m),
            };
        }
        self.links = saved;
        self.leave();
        t
    }

    fn base_type(&mut self) -> Type {
        let m = self.mark();
        let kind = match self.word() {
            w @ ("bit" | "int") => {
                self.bump();
                let width = if self.at(K::Lt) {
                    Some(Box::new(self.width()))
                } else {
                    None
                };
                if w == "bit" {
                    TypeKind::Bit(width)
                } else {
                    TypeKind::Int(width)
                }
            }
            "varbit" => {
                self.bump();
                if self.at(K::Lt) {
                    TypeKind::Varbit(Box::new(self.width()))
                } else {
                    self.expected("`<`");
                    TypeKind::Varbit(Box::new(self.error_expr()))
                }
            }
            "bool" => self.bump_kind(TypeKind::Bool),
            "string" => self.bump_kind(TypeKind::String),
            "void" => self.bump_kind(TypeKind::Void),
            "error" => self.bump_kind(TypeKind::ErrorType),
            "match_kind" => self.bump_kind(TypeKind::MatchKind),
            "_" => self.bump_kind(TypeKind::DontCare),
            "tuple" => {
                self.bump();
                TypeKind::Tuple(self.type_args())
            }
            "list" if self.nth(1, K::Lt) => {
                self.bump();
                let mut args = self.type_args();
                if args.len() != 1 {
                    let span = self.finish(m);
                    self.error_at(span, "`list` takes one type argument");
                }
                match args.pop() {
                    Some(a) if args.is_empty() => TypeKind::List(Box::new(a)),
                    _ => TypeKind::Error,
                }
            }
            _ if self.at(K::Dot) || self.at_name() => {
                let path = self.path("type name");
                let args = if self.at(K::Lt) {
                    self.type_args()
                } else {
                    Vec::new()
                };
                TypeKind::Named { path, args }
            }
            _ => {
                self.expected("type");
                return self.error_type();
            }
        };
        Type {
            kind,
            span: self.finish(m),
        }
    }

    fn bump_kind(&mut self, k: TypeKind) -> TypeKind {
        self.bump();
        k
    }

    /// `<W>` after `bit`, `int`, or `varbit`.
    fn width(&mut self) -> Expr {
        let open = self.cur_span();
        self.bump(); // <
        let saved = std::mem::replace(&mut self.no_gt, true);
        let e = self.expr();
        self.no_gt = saved;
        self.close(K::Gt, ">", open);
        e
    }

    /// `<T, ...>`, at the `<`.
    fn type_args(&mut self) -> Vec<Type> {
        let mut v = Vec::new();
        if !self.at(K::Lt) {
            self.expected("`<`");
            return v;
        }
        let open = self.cur_span();
        self.bump();
        while !self.eof() && !self.at(K::Gt) {
            let before = self.pos;
            v.push(self.ty());
            if !self.eat(K::Comma) || self.pos == before {
                break;
            }
        }
        self.close(K::Gt, ">", open);
        v
    }

    // -----------------------------------------------------------------------
    // Expressions

    fn error_expr(&self) -> Expr {
        let s = self.cur_span();
        Expr {
            kind: ExprKind::Error,
            span: Span::new(s.file, s.start, s.start),
        }
    }

    fn expr(&mut self) -> Expr {
        self.expr_bp(0)
    }

    /// An expression inside delimiters, where `>` compares again.
    fn nested_expr(&mut self) -> Expr {
        let saved = std::mem::replace(&mut self.no_gt, false);
        let e = self.expr();
        self.no_gt = saved;
        e
    }

    fn expr_list(&mut self, close: K) -> Vec<Expr> {
        let saved = std::mem::replace(&mut self.no_gt, false);
        let mut v = Vec::new();
        while !self.eof() && !self.at(close) {
            let before = self.pos;
            v.push(self.expr());
            if !self.eat(K::Comma) || self.pos == before {
                break;
            }
        }
        self.no_gt = saved;
        v
    }

    /// The binary operator at the current position: operator, precedence,
    /// and number of tokens.
    fn peek_binop(&self) -> Option<(BinOp, u8, usize)> {
        let t = self.tok(0)?;
        // `op=` is a compound assignment, not a binary operator.
        let compound = self.joined(1, K::Eq);
        let r = match t.kind {
            K::PipePipe => (BinOp::Or, 2, 1),
            K::AmpAmp => (BinOp::And, 3, 1),
            K::EqEq => (BinOp::Eq, 4, 1),
            K::NotEq => (BinOp::Ne, 4, 1),
            K::Lt => (BinOp::Lt, 5, 1),
            K::LtEq => (BinOp::Le, 5, 1),
            K::Gt if self.no_gt => return None,
            K::Gt if self.joined(1, K::Gt) => {
                if self.joined(2, K::Eq) {
                    return None;
                }
                (BinOp::Shr, 9, 2)
            }
            K::Gt if compound => (BinOp::Ge, 5, 2),
            K::Gt => (BinOp::Gt, 5, 1),
            _ if compound => return None,
            K::Pipe => (BinOp::BitOr, 6, 1),
            K::Caret => (BinOp::BitXor, 7, 1),
            K::Amp => (BinOp::BitAnd, 8, 1),
            K::Shl => (BinOp::Shl, 9, 1),
            K::PlusPlus => (BinOp::Concat, 10, 1),
            // `+:` in `e[i +: w]`.
            K::Plus if self.joined(1, K::Colon) => return None,
            K::Plus => (BinOp::Add, 10, 1),
            K::Minus => (BinOp::Sub, 10, 1),
            K::PlusSat => (BinOp::AddSat, 10, 1),
            K::MinusSat => (BinOp::SubSat, 10, 1),
            K::Star => (BinOp::Mul, 11, 1),
            K::Slash => (BinOp::Div, 11, 1),
            K::Percent => (BinOp::Mod, 11, 1),
            _ => return None,
        };
        Some(r)
    }

    fn expr_bp(&mut self, min: u8) -> Expr {
        let saved = self.links;
        let e = self.expr_bp_chain(min);
        self.links = saved;
        e
    }

    fn expr_bp_chain(&mut self, min: u8) -> Expr {
        let m = self.mark();
        let mut lhs = self.unary();
        loop {
            if self.at(K::Question) && min <= 1 {
                if !self.link() {
                    break;
                }
                self.bump();
                let then = self.nested_expr();
                self.expect(K::Colon, ":");
                let els = self.expr_bp(1);
                lhs = Expr {
                    kind: ExprKind::Ternary {
                        cond: Box::new(lhs),
                        then: Box::new(then),
                        els: Box::new(els),
                    },
                    span: self.finish(m),
                };
                continue;
            }
            let Some((op, prec, n)) = self.peek_binop() else {
                break;
            };
            if prec < min || !self.link() {
                break;
            }
            self.bump_n(n);
            let rhs = self.expr_bp(prec + 1);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span: self.finish(m),
            };
        }
        lhs
    }

    fn unary(&mut self) -> Expr {
        let m = self.mark();
        if !self.enter() {
            return self.error_expr();
        }
        let op = match self.tok(0).map(|t| t.kind) {
            Some(K::Bang) => Some(UnOp::Not),
            Some(K::Tilde) => Some(UnOp::BitNot),
            Some(K::Minus) => Some(UnOp::Neg),
            Some(K::Plus) => Some(UnOp::Plus),
            _ => None,
        };
        let e = if let Some(op) = op {
            self.bump();
            let e = self.unary();
            Expr {
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(e),
                },
                span: self.finish(m),
            }
        } else if let Some(ty) = self.cast_prefix() {
            let e = self.unary();
            Expr {
                kind: ExprKind::Cast {
                    ty,
                    expr: Box::new(e),
                },
                span: self.finish(m),
            }
        } else {
            let p = self.primary();
            self.postfix(m, p)
        };
        self.leave();
        e
    }

    /// `(T)` when it is a cast, consuming it.
    fn cast_prefix(&mut self) -> Option<Type> {
        if !self.at(K::LParen) || self.tok(1).is_none() {
            return None;
        }
        self.speculate(|p| {
            p.bump();
            if !p.can_start_type() {
                return None;
            }
            let ty = p.ty();
            if !p.eat(K::RParen) {
                return None;
            }
            let next = p.tok(0)?;
            let operand = match next.kind {
                K::Int
                | K::Str
                | K::LParen
                | K::LBrace
                | K::Bang
                | K::Tilde => true,
                K::Dot => p.tok(1).is_some_and(is_name_token),
                K::Ident => {
                    is_name_token(next)
                        || matches!(
                            next.text.as_str(),
                            "true" | "false" | "this" | "error"
                        )
                }
                _ => false,
            };
            let sign = matches!(next.kind, K::Minus | K::Plus);
            let ok = match &ty.kind {
                TypeKind::Named { path, args } if args.is_empty() => {
                    operand || (sign && p.types.contains(&path.name.text))
                }
                // `(H[2]) { ... }`; otherwise `(a[i])` is an index.
                TypeKind::Stack { .. } => next.kind == K::LBrace,
                _ => operand || sign,
            };
            ok.then_some(ty)
        })
    }

    fn primary(&mut self) -> Expr {
        let m = self.mark();
        let Some(t) = self.tok(0) else {
            self.expected("expression");
            return self.error_expr();
        };
        let kind = match t.kind {
            K::Int => {
                self.bump();
                match parse_int(&t.text) {
                    Some(lit) => ExprKind::Int(lit),
                    None => {
                        self.error_at(t.span, "invalid integer literal");
                        ExprKind::Int(IntLit {
                            text: t.text.clone(),
                            value: None,
                            width: None,
                            signed: false,
                        })
                    }
                }
            }
            K::Str => {
                self.bump();
                let s = t.text.strip_prefix('"').unwrap_or(&t.text);
                let s = s.strip_suffix('"').unwrap_or(s);
                ExprKind::Str(s.to_string())
            }
            K::Ident => match t.text.as_str() {
                "true" => self.bump_expr(ExprKind::Bool(true)),
                "false" => self.bump_expr(ExprKind::Bool(false)),
                "this" => self.bump_expr(ExprKind::This),
                "_" => self.bump_expr(ExprKind::DontCare),
                "error" => {
                    self.bump();
                    ExprKind::Path(Path {
                        global: false,
                        name: Name {
                            text: t.text.clone(),
                            span: t.span,
                        },
                        span: t.span,
                    })
                }
                _ if is_name_token(t) => ExprKind::Path(self.path("name")),
                _ => {
                    self.expected("expression");
                    return self.error_expr();
                }
            },
            K::Dot if self.tok(1).is_some_and(is_name_token) => {
                ExprKind::Path(self.path("name"))
            }
            K::DotDot if self.joined(1, K::Dot) => {
                self.bump_n(2);
                ExprKind::Dots
            }
            K::LBrace => self.brace_expr(),
            K::LParen => {
                let open = t.span;
                self.bump();
                let e = self.nested_expr();
                self.close(K::RParen, ")", open);
                // Parentheses only group; the inner expression is kept with
                // its own span.
                return e;
            }
            _ => {
                self.expected("expression");
                return self.error_expr();
            }
        };
        Expr {
            kind,
            span: self.finish(m),
        }
    }

    fn bump_expr(&mut self, k: ExprKind) -> ExprKind {
        self.bump();
        k
    }

    /// `{a, b}`, `{f = 1, ...}`, or `{#}`.
    fn brace_expr(&mut self) -> ExprKind {
        if self.nth(1, K::Hash) && self.nth(2, K::RBrace) {
            self.bump_n(3);
            return ExprKind::Invalid;
        }
        let open = self.cur_span();
        self.bump(); // {
        let saved = std::mem::replace(&mut self.no_gt, false);
        let is_struct = self.tok(0).is_some_and(|t| t.kind == K::Ident)
            && self.nth(1, K::Eq);
        let kind = if is_struct {
            let mut fields = Vec::new();
            let mut dots = false;
            while !self.eof() && !self.at(K::RBrace) {
                let before = self.pos;
                if self.at(K::DotDot) && self.joined(1, K::Dot) {
                    self.bump_n(2);
                    dots = true;
                } else {
                    let m = self.mark();
                    let name = self.any_name("field name");
                    self.expect(K::Eq, "=");
                    let value = self.expr();
                    fields.push(NamedExpr {
                        name,
                        value,
                        span: self.finish(m),
                    });
                }
                if !self.eat(K::Comma) || self.pos == before {
                    break;
                }
            }
            ExprKind::Struct { fields, dots }
        } else {
            ExprKind::List(self.expr_list(K::RBrace))
        };
        self.no_gt = saved;
        self.close(K::RBrace, "}", open);
        kind
    }

    fn postfix(&mut self, m: Mark, e: Expr) -> Expr {
        let saved = self.links;
        let e = self.postfix_chain(m, e);
        self.links = saved;
        e
    }

    fn postfix_chain(&mut self, m: Mark, mut e: Expr) -> Expr {
        if matches!(e.kind, ExprKind::Error) {
            return e;
        }
        loop {
            let next = self.tok(0).map(|t| t.kind);
            if matches!(next, Some(K::Dot | K::LBracket | K::LParen | K::Lt))
                && !self.link()
            {
                break;
            }
            let kind = match next {
                Some(K::Dot) => {
                    self.bump();
                    let member = self.any_name("member name");
                    ExprKind::Member {
                        base: Box::new(e),
                        member,
                    }
                }
                Some(K::LBracket) => {
                    let open = self.cur_span();
                    self.bump();
                    let first = self.nested_expr();
                    let kind = if self.eat(K::Colon) {
                        let lo = self.nested_expr();
                        ExprKind::Slice {
                            base: Box::new(e),
                            hi: Box::new(first),
                            lo: Box::new(lo),
                        }
                    } else if self.at(K::Plus) && self.joined(1, K::Colon) {
                        self.bump_n(2);
                        let width = self.nested_expr();
                        ExprKind::PlusSlice {
                            base: Box::new(e),
                            start: Box::new(first),
                            width: Box::new(width),
                        }
                    } else {
                        ExprKind::Index {
                            base: Box::new(e),
                            index: Box::new(first),
                        }
                    };
                    self.close(K::RBracket, "]", open);
                    kind
                }
                Some(K::LParen) => ExprKind::Call {
                    callee: Box::new(e),
                    type_args: Vec::new(),
                    args: self.args(),
                },
                Some(K::Lt)
                    if matches!(
                        e.kind,
                        ExprKind::Path(_) | ExprKind::Member { .. }
                    ) =>
                {
                    let targs = self.speculate(|p| {
                        let first_ok = p.tok(1).is_some_and(|t| {
                            t.kind == K::Dot || t.kind == K::Ident
                        });
                        if !first_ok {
                            return None;
                        }
                        let v = p.type_args();
                        (p.at(K::LParen) || p.at(K::Dot)).then_some(v)
                    });
                    let Some(type_args) = targs else { break };
                    if self.at(K::Dot) {
                        ExprKind::Specialized {
                            base: Box::new(e),
                            type_args,
                        }
                    } else {
                        ExprKind::Call {
                            callee: Box::new(e),
                            type_args,
                            args: self.args(),
                        }
                    }
                }
                _ => break,
            };
            e = Expr {
                kind,
                span: self.finish(m),
            };
        }
        e
    }

    fn args(&mut self) -> Vec<Argument> {
        let open = self.cur_span();
        self.bump(); // (
        let saved = std::mem::replace(&mut self.no_gt, false);
        let mut v = Vec::new();
        while !self.eof() && !self.at(K::RParen) {
            let before = self.pos;
            let m = self.mark();
            let name = if self.tok(0).is_some_and(|t| t.kind == K::Ident)
                && self.nth(1, K::Eq)
            {
                let n = self.any_name("argument name");
                self.bump(); // =
                Some(n)
            } else {
                None
            };
            let value = self.expr();
            v.push(Argument {
                name,
                value,
                span: self.finish(m),
            });
            if !self.eat(K::Comma) || self.pos == before {
                break;
            }
        }
        self.no_gt = saved;
        self.close(K::RParen, ")", open);
        v
    }
}

fn opener(close: K) -> &'static str {
    match close {
        K::RParen => "(",
        K::RBracket => "[",
        K::RBrace => "{",
        K::Gt => "<",
        _ => "",
    }
}

/// Parses an integer literal such as `42`, `0x2A`, `8w0b101`, or `16s-`.
fn parse_int(text: &str) -> Option<IntLit> {
    let bytes = text.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    let (width, signed, rest) = match bytes.get(digits) {
        Some(b'w' | b's') if digits > 0 && digits + 1 < bytes.len() => (
            Some(text[..digits].parse::<u32>().ok()?),
            bytes[digits] == b's',
            &text[digits + 1..],
        ),
        _ => (None, false, text),
    };
    let lower = rest.get(..2).map(|p| p.to_ascii_lowercase());
    let (radix, body) = match lower.as_deref() {
        Some("0x") => (16, &rest[2..]),
        Some("0b") => (2, &rest[2..]),
        Some("0o") => (8, &rest[2..]),
        Some("0d") => (10, &rest[2..]),
        _ => (10, rest),
    };
    let mut value: Option<u128> = Some(0);
    let mut any = false;
    for c in body.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(radix)?;
        any = true;
        value = value
            .and_then(|v| v.checked_mul(radix as u128))
            .and_then(|v| v.checked_add(d as u128));
    }
    if !any {
        return None;
    }
    Some(IntLit {
        text: text.to_string(),
        value,
        width,
        signed,
    })
}
