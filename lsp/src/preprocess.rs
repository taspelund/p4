// Copyright 2026 Oxide Computer Company

//! C-style preprocessor producing a source-mapped token stream.
//!
//! The preprocessor implements the subset of the C preprocessor that P4
//! programs use in practice: `#include` (spliced at the directive, so files
//! may be included in the middle of a declaration), object-like and
//! function-like macros with C expansion semantics (argument pre-expansion,
//! `#` stringification, `##` pasting, variadic macros, and hide sets that
//! prevent recursive self-expansion), and conditional compilation with
//! integer constant expressions.
//!
//! Source mapping: tokens read from a file keep their own spans. Tokens
//! produced by macro expansion carry the span of the outermost invocation
//! in user source, from the macro name through the closing parenthesis of
//! its arguments.
//!
//! Errors never abort preprocessing; they are reported in
//! [`Output::diagnostics`] and processing continues.

use crate::diag::Diagnostic;
use crate::lexer::{self, Token, TokenKind};
use crate::source::{FileId, SourceDb, Span};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

/// Maximum depth of nested `#include`s before reporting an error.
const MAX_INCLUDE_DEPTH: usize = 200;

/// Upper bound on the total number of tokens produced by macro expansion,
/// which guards against exponential blowup from pathological macros.
const MAX_EXPANDED_TOKENS: usize = 2_000_000;

/// Maximum nesting of macro invocations in macro arguments, which are
/// expanded recursively.
const MAX_ARG_NESTING: u32 = 256;

/// Preprocessor configuration, mirroring a compiler's `-I` and `-D` flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Directories searched for `#include` targets, in order.
    pub include_dirs: Vec<PathBuf>,
    /// Predefined macros as `(name, body)`. `-D NAME` corresponds to
    /// `("NAME", "1")`. A name such as `F(x)` defines a function-like macro.
    pub defines: Vec<(String, String)>,
}

/// One `#include` directive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Include {
    /// Span of the quoted or bracketed path in the directive, including the
    /// quotes or angle brackets.
    pub span: Span,
    /// The resolved file, if it was found.
    pub target: Option<FileId>,
}

/// A macro definition from a `#define` or from [`Options::defines`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroDef {
    pub name: String,
    /// Span of the name in its `#define`, or `None` for predefined macros.
    pub span: Option<Span>,
    /// Parameter names for function-like macros. For variadic macros the
    /// last entry names the variable arguments (`__VA_ARGS__`, or the name
    /// given in the GNU `name...` form).
    pub params: Option<Vec<String>>,
    /// Whether the macro takes variable arguments.
    pub variadic: bool,
    /// The replacement list, with tokens separated by single spaces where
    /// the source had whitespace.
    pub body: String,
}

#[derive(Debug, Default)]
pub struct Output {
    /// The expanded token stream with directives removed. Tokens produced by
    /// macro expansion carry the span of the outermost invocation.
    pub tokens: Vec<Token>,
    /// Every `#include` directive in active code, in processing order.
    pub includes: Vec<Include>,
    /// Every macro definition, in processing order. Predefined macros come
    /// first.
    pub macros: Vec<MacroDef>,
    /// Each macro name occurrence in source text (invocations, `#ifdef`,
    /// `#ifndef`, `#undef`, `defined(...)`) and the index of its definition
    /// in `macros`.
    pub macro_uses: Vec<(Span, usize)>,
    /// Regions skipped by false conditionals, from the start of the line
    /// after the controlling directive to the end of the line before the
    /// next directive at the same level.
    pub inactive: Vec<Span>,
    /// Each `#elif`, `#else`, and `#endif` (also in inactive code) with the
    /// opening `#if`, `#ifdef`, or `#ifndef` directive it belongs to.
    pub conditionals: Vec<(Span, Span)>,
    /// Comment spans per loaded file, from the lexer.
    pub comments: HashMap<FileId, Vec<Span>>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Preprocess `root` and every file it includes.
pub fn preprocess(db: &mut SourceDb, root: FileId, opts: &Options) -> Output {
    let root_dir = parent_dir(db.path(root));
    let mut pp = Pp {
        db,
        opts,
        root_dir,
        out: Output::default(),
        files: Vec::new(),
        lexed: HashMap::new(),
        canon: HashMap::new(),
        once: HashSet::new(),
        conds: Vec::new(),
        defs: HashMap::new(),
        name_ids: HashMap::new(),
        pending: Vec::new(),
        isolated: false,
        in_if: false,
        budget: 0,
        arg_depth: 0,
        too_deep: false,
    };
    if let Ok(c) = std::fs::canonicalize(pp.db.path(root)) {
        pp.canon.insert(c, root);
    }
    for (name, body) in &opts.defines {
        let text = format!("{name} {body}");
        let toks = lexer::lex(root, &text).tokens;
        pp.define(&toks, Span::new(root, 0, 0), true);
    }
    pp.enter_file(root);
    while let Some(t) = pp.next_expanded() {
        pp.out.tokens.push(t.t);
    }
    pp.out
}

/// A set of macro name ids, sorted. `None` is the empty set.
type Hide = Option<Rc<[u32]>>;

/// A token in flight through macro expansion.
#[derive(Clone, Debug)]
struct PTok {
    t: Token,
    /// Macros that must not expand this token (Prosser's hide set).
    hide: Hide,
    /// Whether the token was produced by macro expansion rather than read
    /// from source text.
    expanded: bool,
}

impl PTok {
    fn src(t: Token) -> Self {
        PTok {
            t,
            hide: None,
            expanded: false,
        }
    }
}

#[derive(Debug)]
struct Macro {
    /// Interned name, for hide sets.
    id: u32,
    /// Index into `Output::macros`.
    def: usize,
    params: Option<Vec<String>>,
    variadic: bool,
    body: Vec<Token>,
}

struct FileState {
    file: FileId,
    toks: Rc<[Token]>,
    pos: usize,
    /// Length of the conditional stack when the file was entered.
    cond_base: usize,
}

struct Cond {
    file: FileId,
    /// Span of the opening directive.
    dir: Span,
    /// Whether the enclosing code is active.
    parent_active: bool,
    /// Whether some branch has been taken.
    taken: bool,
    /// Whether the current branch is active.
    active: bool,
    seen_else: bool,
    /// Start offset of the current inactive region, if one is open.
    inactive_from: Option<u32>,
}

/// One element of a macro replacement during substitution.
enum Item {
    Tok(PTok),
    /// An empty argument adjacent to `##`.
    Placemarker,
    /// The `##` operator.
    Paste,
}

struct Pp<'a> {
    db: &'a mut SourceDb,
    opts: &'a Options,
    root_dir: PathBuf,
    out: Output,
    files: Vec<FileState>,
    lexed: HashMap<FileId, Rc<[Token]>>,
    /// Canonical paths of loaded files.
    canon: HashMap<PathBuf, FileId>,
    once: HashSet<FileId>,
    conds: Vec<Cond>,
    defs: HashMap<String, Rc<Macro>>,
    name_ids: HashMap<String, u32>,
    /// Tokens to read before any further file tokens; the top is next.
    pending: Vec<PTok>,
    /// When set, reading stops when `pending` is exhausted rather than
    /// continuing into the file (used for argument pre-expansion and
    /// directive lines).
    isolated: bool,
    /// Whether `defined` is an operator (inside `#if`/`#elif`).
    in_if: bool,
    budget: usize,
    /// Nesting of argument pre-expansion; see [`MAX_ARG_NESTING`].
    arg_depth: u32,
    /// Set when `arg_depth` hits its limit, until the expansion containing
    /// it has been read. Function-like macros are not expanded meanwhile,
    /// so the cut-off arguments are not expanded again and again.
    too_deep: bool,
}

impl Pp<'_> {
    // ----- File layer -----

    fn enter_file(&mut self, file: FileId) {
        let toks = match self.lexed.get(&file) {
            Some(t) => t.clone(),
            None => {
                let lexed = lexer::lex(file, self.db.text(file));
                self.out.diagnostics.extend(lexed.diagnostics);
                self.out.comments.insert(file, lexed.comments);
                let toks: Rc<[Token]> = lexed.tokens.into();
                self.lexed.insert(file, toks.clone());
                toks
            }
        };
        self.files.push(FileState {
            file,
            toks,
            pos: 0,
            cond_base: self.conds.len(),
        });
    }

    fn end_file(&mut self) {
        let Some(fs) = self.files.pop() else { return };
        let len = self.db.text(fs.file).len() as u32;
        while self.conds.len() > fs.cond_base {
            let c = self.conds.pop().unwrap();
            self.out
                .diagnostics
                .push(Diagnostic::error(c.dir, "unterminated conditional"));
            if let Some(from) = c.inactive_from {
                self.push_inactive(c.file, from, len);
            }
        }
    }

    fn active(&self) -> bool {
        self.conds.last().is_none_or(|c| c.active)
    }

    /// The next token from the include stack, processing directives and
    /// skipping inactive code.
    fn next_file_token(&mut self) -> Option<Token> {
        loop {
            let active = self.active();
            let fs = self.files.last_mut()?;
            if fs.pos >= fs.toks.len() {
                self.end_file();
                continue;
            }
            let idx = fs.pos;
            let tok = &fs.toks[idx];
            if tok.nl_before && tok.kind == TokenKind::Hash {
                let toks = fs.toks.clone();
                let start = fs.pos;
                let mut end = start + 1;
                while end < toks.len() && !toks[end].nl_before {
                    end += 1;
                }
                fs.pos = end;
                let file = fs.file;
                self.directive(file, &toks[start..end]);
                continue;
            }
            fs.pos += 1;
            if active {
                return Some(fs.toks[idx].clone());
            }
        }
    }

    fn next_raw(&mut self) -> Option<PTok> {
        if let Some(t) = self.pending.pop() {
            return Some(t);
        }
        if self.isolated {
            return None;
        }
        self.next_file_token().map(PTok::src)
    }

    // ----- Directives -----

    fn directive(&mut self, file: FileId, toks: &[Token]) {
        let hash = &toks[0];
        let dir_span = hash.span.to(toks[toks.len() - 1].span);
        let Some(name) = toks.get(1) else {
            return; // Null directive.
        };
        let rest = &toks[2..];
        let active = self.active();
        let dir_end = toks[toks.len() - 1].span.end;
        if name.kind != TokenKind::Ident {
            // `# 12 "file"` line markers are accepted and ignored.
            if active && name.kind != TokenKind::Int {
                self.error(name.span, "invalid preprocessing directive");
            }
            return;
        }
        match name.text.as_str() {
            "if" | "ifdef" | "ifndef" => {
                let value =
                    active && self.cond_value(&name.text, rest, dir_span);
                let inactive_from =
                    (active && !value).then(|| self.line_after(file, dir_end));
                self.conds.push(Cond {
                    file,
                    dir: dir_span,
                    parent_active: active,
                    taken: value,
                    active: value,
                    seen_else: false,
                    inactive_from,
                });
            }
            "elif" | "else" => {
                let is_else = name.text == "else";
                let base = self.files.last().map_or(0, |f| f.cond_base);
                if self.conds.len() <= base {
                    self.error(dir_span, format!("#{} without #if", name.text));
                    return;
                }
                let c = self.conds.last().unwrap();
                self.out.conditionals.push((dir_span, c.dir));
                let (seen_else, parent_active, taken, cur_active) =
                    (c.seen_else, c.parent_active, c.taken, c.active);
                if seen_else {
                    let msg = format!("#{} after #else", name.text);
                    self.error(dir_span, msg);
                }
                if !parent_active {
                    return;
                }
                if taken {
                    let from = self.line_after(file, dir_end);
                    let c = self.conds.last_mut().unwrap();
                    if cur_active {
                        c.active = false;
                        c.inactive_from = Some(from);
                    }
                    c.seen_else |= is_else;
                    return;
                }
                let value = is_else || self.cond_value("if", rest, dir_span);
                let end = self.line_before(file, hash.span.start);
                let c = self.conds.last_mut().unwrap();
                c.seen_else |= is_else;
                if value {
                    c.active = true;
                    c.taken = true;
                    if let Some(from) = c.inactive_from.take() {
                        self.push_inactive(file, from, end);
                    }
                }
            }
            "endif" => {
                let base = self.files.last().map_or(0, |f| f.cond_base);
                if self.conds.len() <= base {
                    self.error(dir_span, "#endif without #if");
                    return;
                }
                let c = self.conds.pop().unwrap();
                self.out.conditionals.push((dir_span, c.dir));
                if let Some(from) = c.inactive_from {
                    let end = self.line_before(file, hash.span.start);
                    self.push_inactive(file, from, end);
                }
            }
            _ if !active => {}
            "define" => self.define(rest, dir_span, false),
            "undef" => match rest.first() {
                Some(t) if t.kind == TokenKind::Ident => {
                    if let Some(m) = self.defs.remove(&t.text) {
                        self.out.macro_uses.push((t.span, m.def));
                    }
                }
                _ => self.error(dir_span, "macro name missing in #undef"),
            },
            "include" => self.include(file, rest, dir_span),
            "pragma" => {
                if rest.first().is_some_and(|t| t.is_word("once")) {
                    self.once.insert(file);
                }
            }
            "error" | "warning" => {
                let msg = match (rest.first(), rest.last()) {
                    (Some(a), Some(b)) => {
                        self.db.slice(a.span.to(b.span)).to_string()
                    }
                    _ => format!("#{}", name.text),
                };
                let d = if name.text == "error" {
                    Diagnostic::error(dir_span, msg)
                } else {
                    Diagnostic::warning(dir_span, msg)
                };
                self.out.diagnostics.push(d);
            }
            "line" | "ident" | "sccs" => {}
            other => {
                let msg = format!("unknown preprocessing directive `#{other}`");
                self.error(name.span, msg);
            }
        }
    }

    /// Evaluate the condition of an `#if`, `#ifdef`, or `#ifndef`.
    fn cond_value(&mut self, kind: &str, rest: &[Token], dir: Span) -> bool {
        if kind == "if" {
            return self.eval_if(rest, dir);
        }
        match rest.first() {
            Some(t) if t.kind == TokenKind::Ident => {
                let defined = match self.defs.get(&t.text) {
                    Some(m) => {
                        self.out.macro_uses.push((t.span, m.def));
                        true
                    }
                    None => false,
                };
                defined == (kind == "ifdef")
            }
            _ => {
                self.error(dir, format!("macro name missing in #{kind}"));
                false
            }
        }
    }

    fn eval_if(&mut self, rest: &[Token], dir: Span) -> bool {
        if rest.is_empty() {
            self.error(dir, "#if with no expression");
            return false;
        }
        let saved_if = std::mem::replace(&mut self.in_if, true);
        let toks = rest.iter().cloned().map(PTok::src).collect();
        let expanded: Vec<Token> = self
            .expand_isolated(toks)
            .into_iter()
            .map(|t| t.t)
            .collect();
        self.in_if = saved_if;
        let mut ev = Eval {
            toks: &expanded,
            pos: 0,
            dir,
            err: None,
            diags: Vec::new(),
            depth: 0,
        };
        let value = if expanded.is_empty() {
            ev.err = Some((dir, "#if expression is empty".into()));
            0
        } else {
            let v = ev.cond(true);
            if ev.err.is_none() {
                if let Some(t) = ev.toks.get(ev.pos) {
                    let msg =
                        format!("unexpected `{}` in #if expression", t.text);
                    ev.err = Some((t.span, msg));
                }
            }
            v
        };
        let err = ev.err.take();
        self.out.diagnostics.append(&mut ev.diags);
        match err {
            Some((span, msg)) => {
                self.error(span, msg);
                false
            }
            None => value != 0,
        }
    }

    fn define(&mut self, toks: &[Token], dir: Span, predefined: bool) {
        let Some(name) = toks.first() else {
            self.error(dir, "macro name missing in #define");
            return;
        };
        if name.kind != TokenKind::Ident {
            self.error(name.span, "macro name must be an identifier");
            return;
        }
        if name.text == "defined" {
            self.error(name.span, "`defined` cannot be used as a macro name");
            return;
        }
        let mut i = 1;
        let mut params = None;
        let mut variadic = false;
        if toks
            .get(1)
            .is_some_and(|t| t.is(TokenKind::LParen) && !t.ws_before)
        {
            match parse_params(toks, 2) {
                Ok((ps, v, next)) => {
                    params = Some(ps);
                    variadic = v;
                    i = next;
                }
                Err(span) => {
                    let span = span.unwrap_or(dir);
                    self.error(span, "invalid macro parameter list");
                    return;
                }
            }
        }
        let body = toks[i..].to_vec();
        if let Some(ps) = &params {
            for (j, t) in body.iter().enumerate() {
                if t.is(TokenKind::Hash)
                    && !body.get(j + 1).is_some_and(|n| {
                        n.kind == TokenKind::Ident && ps.contains(&n.text)
                    })
                {
                    self.error(
                        t.span,
                        "`#` is not followed by a macro parameter",
                    );
                }
            }
        }
        if body.first().is_some_and(|t| t.is(TokenKind::HashHash))
            || body.len() > 1
                && body.last().is_some_and(|t| t.is(TokenKind::HashHash))
            || body.len() == 1 && body[0].is(TokenKind::HashHash)
        {
            self.error(dir, "`##` cannot appear at either end of a macro body");
        }

        if let Some(old) = self.defs.get(&name.text) {
            let same = old.params == params
                && old.body.len() == body.len()
                && old.body.iter().zip(&body).enumerate().all(|(j, (a, b))| {
                    a.text == b.text && (j == 0 || a.ws_before == b.ws_before)
                });
            if !same {
                let prev = self.out.macros[old.def].span;
                let mut d = Diagnostic::warning(
                    name.span,
                    format!("`{}` redefined", name.text),
                );
                if let Some(prev) = prev {
                    d = d.with_related(prev, "previous definition");
                }
                if !predefined {
                    self.out.diagnostics.push(d);
                }
            }
        }

        let def = self.out.macros.len();
        self.out.macros.push(MacroDef {
            name: name.text.clone(),
            span: (!predefined).then_some(name.span),
            params: params.clone(),
            variadic,
            body: join_tokens(body.iter()),
        });
        let next_id = self.name_ids.len() as u32;
        let id = *self.name_ids.entry(name.text.clone()).or_insert(next_id);
        self.defs.insert(
            name.text.clone(),
            Rc::new(Macro {
                id,
                def,
                params,
                variadic,
                body,
            }),
        );
    }

    fn include(&mut self, file: FileId, rest: &[Token], dir: Span) {
        let Some((name, angle, span)) = self.include_path(file, rest) else {
            self.error(dir, "#include expects \"FILENAME\" or <FILENAME>");
            return;
        };
        if name.is_empty() {
            self.error(span, "empty filename in #include");
            return;
        }
        let cur_dir = parent_dir(self.db.path(file));
        let mut dirs: Vec<PathBuf> = Vec::new();
        if !angle {
            dirs.push(cur_dir.clone());
        }
        dirs.extend(self.opts.include_dirs.iter().cloned());
        dirs.push(cur_dir);
        dirs.push(self.root_dir.clone());
        let mut seen = HashSet::new();
        dirs.retain(|d| seen.insert(d.clone()));

        let mut target = None;
        if Path::new(&name).is_absolute() {
            target = self.resolve(Path::new(&name));
        } else {
            for d in &dirs {
                if let Some(id) = self.resolve(&d.join(&name)) {
                    target = Some(id);
                    break;
                }
            }
        }
        self.out.includes.push(Include { span, target });
        let Some(id) = target else {
            let searched: Vec<String> =
                dirs.iter().map(|d| d.display().to_string()).collect();
            self.error(
                span,
                format!(
                    "cannot find include file `{name}` (searched: {})",
                    searched.join(", ")
                ),
            );
            return;
        };
        if self.once.contains(&id) {
            return;
        }
        if self.files.len() >= MAX_INCLUDE_DEPTH {
            self.error(span, "#include nested too deeply");
            return;
        }
        self.enter_file(id);
    }

    /// Parse the operand of `#include`: the file name, whether it was
    /// bracketed, and the span of the operand.
    fn include_path(
        &mut self,
        file: FileId,
        rest: &[Token],
    ) -> Option<(String, bool, Span)> {
        let first = rest.first()?;
        match first.kind {
            TokenKind::Str => {
                let name = first.text.strip_prefix('"')?;
                let name = name.strip_suffix('"').unwrap_or(name);
                Some((name.to_string(), false, first.span))
            }
            TokenKind::Lt => {
                let gt = rest.iter().find(|t| t.is(TokenKind::Gt))?;
                let text = self.db.text(file);
                let name = text
                    .get(first.span.end as usize..gt.span.start as usize)?
                    .to_string();
                Some((name, true, first.span.to(gt.span)))
            }
            TokenKind::Ident => {
                // Computed include: expand, then reinterpret.
                let span = first.span.to(rest[rest.len() - 1].span);
                let toks = rest.iter().cloned().map(PTok::src).collect();
                let ex = self.expand_isolated(toks);
                let first = ex.first()?;
                match first.t.kind {
                    TokenKind::Str => {
                        let name = first.t.text.strip_prefix('"')?;
                        let name = name.strip_suffix('"').unwrap_or(name);
                        Some((name.to_string(), false, span))
                    }
                    TokenKind::Lt => {
                        let end =
                            ex.iter().position(|t| t.t.is(TokenKind::Gt))?;
                        let name = join_tokens(ex[1..end].iter().map(|t| &t.t));
                        Some((name, true, span))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Map a candidate include path to a loaded file, if it exists.
    fn resolve(&mut self, path: &Path) -> Option<FileId> {
        let lexical = normalize(path);
        if let Some(id) = self.db.file_id(&lexical) {
            return Some(id);
        }
        if !self.db.exists(&lexical) {
            return None;
        }
        if lexical.is_file() {
            if let Ok(c) = std::fs::canonicalize(&lexical) {
                if let Some(id) = self.canon.get(&c) {
                    return Some(*id);
                }
                let id = self.db.load(&c)?;
                self.canon.insert(c, id);
                return Some(id);
            }
        }
        self.db.load(&lexical)
    }

    // ----- Macro expansion -----

    /// The next fully macro-expanded token.
    fn next_expanded(&mut self) -> Option<PTok> {
        loop {
            if self.too_deep && self.arg_depth == 0 && self.pending.is_empty() {
                self.too_deep = false;
            }
            let tok = self.next_raw()?;
            if tok.t.kind != TokenKind::Ident {
                return Some(tok);
            }
            if self.in_if && tok.t.text == "defined" {
                return Some(self.defined_op(tok));
            }
            let Some(m) = self.defs.get(&tok.t.text).cloned() else {
                return Some(tok);
            };
            if hide_contains(&tok.hide, m.id)
                || self.budget > MAX_EXPANDED_TOKENS
            {
                return Some(tok);
            }
            if m.params.is_none() {
                let hs = hide_add(&tok.hide, m.id);
                let body = self.subst(&m, &[]);
                let span = tok.t.span;
                self.push_expansion(&tok, span, &hs, body, &m);
                continue;
            }
            if self.too_deep {
                return Some(tok);
            }
            let Some(next) = self.next_raw() else {
                return Some(tok);
            };
            if !next.t.is(TokenKind::LParen) {
                self.pending.push(next);
                return Some(tok);
            }
            match self.collect_args(&m, &tok, next) {
                Ok((args, rparen)) => {
                    let hs = hide_add(
                        &hide_intersect(&tok.hide, &rparen.hide),
                        m.id,
                    );
                    let span = tok.t.span.to(rparen.t.span);
                    let body = self.subst(&m, &args);
                    self.push_expansion(&tok, span, &hs, body, &m);
                }
                Err(consumed) => {
                    self.pending.extend(consumed.into_iter().rev());
                    return Some(tok);
                }
            }
        }
    }

    /// Replace a macro invocation with its expansion, to be rescanned.
    fn push_expansion(
        &mut self,
        name: &PTok,
        span: Span,
        hs: &Rc<[u32]>,
        mut body: Vec<PTok>,
        m: &Macro,
    ) {
        if !name.expanded {
            self.out.macro_uses.push((name.t.span, m.def));
        }
        let before = self.budget;
        self.budget += body.len() + 1;
        if before <= MAX_EXPANDED_TOKENS && self.budget > MAX_EXPANDED_TOKENS {
            self.error(span, "macro expansion limit exceeded");
        }
        for (i, t) in body.iter_mut().enumerate() {
            t.t.span = span;
            t.expanded = true;
            t.hide = hide_union(&t.hide, hs);
            if i == 0 {
                t.t.ws_before = name.t.ws_before;
                t.t.nl_before = name.t.nl_before;
            } else {
                t.t.nl_before = false;
            }
        }
        self.pending.extend(body.into_iter().rev());
    }

    /// Collect the arguments of a function-like macro invocation after its
    /// `(`. On failure, returns every consumed token (including `lparen`)
    /// so that it can be pushed back.
    fn collect_args(
        &mut self,
        m: &Macro,
        name: &PTok,
        lparen: PTok,
    ) -> Result<(Vec<Vec<PTok>>, PTok), Vec<PTok>> {
        let nparams = m.params.as_ref().map_or(0, |p| p.len());
        let mut all = vec![lparen];
        // Indices in `all` of argument-separating commas.
        let mut seps = Vec::new();
        let mut depth = 0usize;
        loop {
            let Some(t) = self.next_raw() else {
                let msg = format!(
                    "unterminated argument list invoking macro `{}`",
                    name.t.text
                );
                self.error(name.t.span, msg);
                return Err(all);
            };
            match t.t.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen if depth == 0 => {
                    all.push(t);
                    break;
                }
                TokenKind::RParen => depth -= 1,
                TokenKind::Comma
                    if depth == 0
                        && !(m.variadic && seps.len() + 1 >= nparams) =>
                {
                    seps.push(all.len());
                }
                _ => {}
            }
            all.push(t);
        }
        let mut args: Vec<Vec<PTok>> = Vec::with_capacity(seps.len() + 1);
        let mut start = 1;
        for &s in seps.iter().chain(std::iter::once(&(all.len() - 1))) {
            args.push(all[start..s].to_vec());
            start = s + 1;
        }
        if nparams == 0 && args.len() == 1 && args[0].is_empty() {
            args.clear();
        }
        if m.variadic && args.len() + 1 == nparams {
            args.push(Vec::new());
        }
        if args.len() != nparams {
            let rparen = &all[all.len() - 1];
            let msg = format!(
                "macro `{}` requires {} argument{}, but {} given",
                name.t.text,
                nparams,
                if nparams == 1 { "" } else { "s" },
                args.len()
            );
            self.error(name.t.span.to(rparen.t.span), msg);
            return Err(all);
        }
        let rparen = all.pop().unwrap();
        Ok((args, rparen))
    }

    /// Fully expand `toks` without reading beyond them.
    fn expand_isolated(&mut self, toks: Vec<PTok>) -> Vec<PTok> {
        if self.arg_depth >= MAX_ARG_NESTING {
            if !self.too_deep {
                self.too_deep = true;
                if let Some(t) = toks.first() {
                    let span = t.t.span;
                    self.error(span, "macro arguments are nested too deeply");
                }
            }
            return toks;
        }
        self.arg_depth += 1;
        let saved = std::mem::replace(
            &mut self.pending,
            toks.into_iter().rev().collect(),
        );
        let saved_iso = std::mem::replace(&mut self.isolated, true);
        let mut out = Vec::new();
        while let Some(t) = self.next_expanded() {
            out.push(t);
        }
        self.pending = saved;
        self.isolated = saved_iso;
        self.arg_depth -= 1;
        out
    }

    /// Evaluate `defined X` or `defined(X)` to an integer token.
    fn defined_op(&mut self, tok: PTok) -> PTok {
        let mut value = false;
        let mut next = self.next_raw();
        let paren = next.as_ref().is_some_and(|t| t.t.is(TokenKind::LParen));
        if paren {
            next = self.next_raw();
        }
        match next {
            Some(n) if n.t.kind == TokenKind::Ident => {
                if let Some(m) = self.defs.get(&n.t.text) {
                    value = true;
                    if !n.expanded {
                        self.out.macro_uses.push((n.t.span, m.def));
                    }
                }
                if paren {
                    match self.next_raw() {
                        Some(c) if c.t.is(TokenKind::RParen) => {}
                        other => {
                            self.error(n.t.span, "missing `)` after `defined`");
                            self.pending.extend(other);
                        }
                    }
                }
            }
            other => {
                self.error(
                    tok.t.span,
                    "operator `defined` requires an identifier",
                );
                self.pending.extend(other);
            }
        }
        let mut t = tok;
        t.t.kind = TokenKind::Int;
        t.t.text = if value { "1" } else { "0" }.into();
        t
    }

    /// Substitute arguments into a macro body and apply `#` and `##`.
    fn subst(&mut self, m: &Macro, args: &[Vec<PTok>]) -> Vec<PTok> {
        let empty = Vec::new();
        let params = m.params.as_ref().unwrap_or(&empty);
        let fnlike = m.params.is_some();
        let pidx = |t: &Token| {
            if t.kind == TokenKind::Ident {
                params.iter().position(|p| *p == t.text)
            } else {
                None
            }
        };
        let body = &m.body;
        let mut cache: Vec<Option<Vec<PTok>>> = vec![None; args.len()];
        let mut items = Vec::with_capacity(body.len());
        let mut i = 0;
        while i < body.len() {
            let bt = &body[i];
            // GNU `, ## __VA_ARGS__`: drop the comma if the variable
            // arguments are empty.
            if m.variadic
                && bt.is(TokenKind::Comma)
                && body.get(i + 1).is_some_and(|t| t.is(TokenKind::HashHash))
                && body.get(i + 2).and_then(pidx) == Some(params.len() - 1)
            {
                let va = &args[params.len() - 1];
                if !va.is_empty() {
                    items.push(Item::Tok(PTok::src(bt.clone())));
                    items.extend(va.iter().cloned().map(Item::Tok));
                }
                i += 3;
                continue;
            }
            if fnlike && bt.is(TokenKind::Hash) {
                if let Some(p) = body.get(i + 1).and_then(pidx) {
                    items.push(Item::Tok(PTok::src(stringize(&args[p], bt))));
                    i += 2;
                    continue;
                }
            }
            if bt.is(TokenKind::HashHash) {
                items.push(Item::Paste);
                i += 1;
                continue;
            }
            if let Some(p) = pidx(bt) {
                let adjacent = i > 0 && body[i - 1].is(TokenKind::HashHash)
                    || body
                        .get(i + 1)
                        .is_some_and(|t| t.is(TokenKind::HashHash));
                let mut toks = if adjacent {
                    args[p].clone()
                } else {
                    if cache[p].is_none() {
                        cache[p] = Some(self.expand_isolated(args[p].clone()));
                    }
                    cache[p].clone().unwrap()
                };
                match toks.first_mut() {
                    Some(f) => {
                        f.t.ws_before = bt.ws_before;
                        items.extend(toks.into_iter().map(Item::Tok));
                    }
                    None => items.push(Item::Placemarker),
                }
                i += 1;
                continue;
            }
            items.push(Item::Tok(PTok::src(bt.clone())));
            i += 1;
        }

        // Resolve `##`.
        let mut res: Vec<Item> = Vec::with_capacity(items.len());
        let mut k = 0;
        while k < items.len() {
            if !matches!(items[k], Item::Paste) {
                res.push(std::mem::replace(&mut items[k], Item::Placemarker));
                k += 1;
                continue;
            }
            let has_rhs =
                k + 1 < items.len() && !matches!(items[k + 1], Item::Paste);
            if !has_rhs {
                k += 1;
                continue;
            }
            let rhs = std::mem::replace(&mut items[k + 1], Item::Placemarker);
            k += 2;
            match (res.pop(), rhs) {
                (Some(Item::Tok(l)), Item::Tok(r)) => {
                    let pasted = self.paste(l, r);
                    res.extend(pasted.into_iter().map(Item::Tok));
                }
                (Some(Item::Tok(l)), _) => res.push(Item::Tok(l)),
                (_, r) => res.push(r),
            }
        }
        res.into_iter()
            .filter_map(|it| match it {
                Item::Tok(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    /// Paste two tokens. If the result is not a single token, reports an
    /// error and returns the operands unchanged.
    fn paste(&mut self, l: PTok, r: PTok) -> Vec<PTok> {
        let text = format!("{}{}", l.t.text, r.t.text);
        let lx = lexer::lex(l.t.span.file, &text);
        let hide = hide_intersect(&l.hide, &r.hide);
        let ok = lx.tokens.len() == 1
            && lx.diagnostics.is_empty()
            && lx.comments.is_empty();
        // `>` is lexed alone; `>>` and `>=` are valid pastes in P4.
        let gt_join = matches!(text.as_str(), ">>" | ">=" | ">>=");
        if !ok && !gt_join {
            let msg = format!(
                "pasting `{}` and `{}` does not give a valid token",
                l.t.text, r.t.text
            );
            self.error(l.t.span, msg);
            return vec![l, r];
        }
        lx.tokens
            .into_iter()
            .enumerate()
            .map(|(i, t)| PTok {
                t: Token {
                    span: l.t.span,
                    ws_before: i == 0 && l.t.ws_before,
                    nl_before: false,
                    ..t
                },
                hide: hide.clone(),
                expanded: l.expanded,
            })
            .collect()
    }

    // ----- Helpers -----

    fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.out.diagnostics.push(Diagnostic::error(span, msg));
    }

    /// Offset of the start of the line after the one containing `offset`.
    fn line_after(&self, file: FileId, offset: u32) -> u32 {
        let text = self.db.text(file);
        let off = (offset as usize).min(text.len());
        match text[off..].find('\n') {
            Some(n) => (off + n + 1) as u32,
            None => text.len() as u32,
        }
    }

    /// Offset of the end of the line before the one containing `offset`.
    fn line_before(&self, file: FileId, offset: u32) -> u32 {
        let text = self.db.text(file);
        let off = (offset as usize).min(text.len());
        match text[..off].rfind('\n') {
            Some(n) => n as u32,
            None => 0,
        }
    }

    fn push_inactive(&mut self, file: FileId, from: u32, end: u32) {
        if end > from {
            self.out.inactive.push(Span::new(file, from, end));
        }
    }
}

/// Parse a macro parameter list starting after `(` at index `i`. Returns
/// the parameters, whether the macro is variadic, and the index after `)`.
fn parse_params(
    toks: &[Token],
    mut i: usize,
) -> Result<(Vec<String>, bool, usize), Option<Span>> {
    let mut ps: Vec<String> = Vec::new();
    let mut variadic = false;
    if toks.get(i).is_some_and(|t| t.is(TokenKind::RParen)) {
        return Ok((ps, false, i + 1));
    }
    let ellipsis = |j: usize| {
        toks.get(j).is_some_and(|t| t.is(TokenKind::DotDot))
            && toks
                .get(j + 1)
                .is_some_and(|t| t.is(TokenKind::Dot) && !t.ws_before)
    };
    loop {
        match toks.get(i) {
            Some(t) if t.kind == TokenKind::Ident => {
                if ps.contains(&t.text) {
                    return Err(Some(t.span));
                }
                ps.push(t.text.clone());
                if ellipsis(i + 1) {
                    variadic = true;
                    i += 3;
                } else {
                    i += 1;
                }
            }
            Some(_) if ellipsis(i) => {
                ps.push("__VA_ARGS__".into());
                variadic = true;
                i += 2;
            }
            t => return Err(t.map(|t| t.span)),
        }
        match toks.get(i) {
            Some(t) if t.is(TokenKind::RParen) => {
                return Ok((ps, variadic, i + 1))
            }
            Some(t) if t.is(TokenKind::Comma) && !variadic => i += 1,
            t => return Err(t.map(|t| t.span)),
        }
    }
}

/// Spell a macro argument as a string literal for the `#` operator.
fn stringize(arg: &[PTok], hash: &Token) -> Token {
    let mut s = String::from("\"");
    for (i, t) in arg.iter().enumerate() {
        if i > 0 && (t.t.ws_before || t.t.nl_before) {
            s.push(' ');
        }
        if t.t.kind == TokenKind::Str {
            for c in t.t.text.chars() {
                if c == '"' || c == '\\' {
                    s.push('\\');
                }
                s.push(c);
            }
        } else {
            s.push_str(&t.t.text);
        }
    }
    s.push('"');
    Token {
        kind: TokenKind::Str,
        text: s,
        span: hash.span,
        ws_before: hash.ws_before,
        nl_before: false,
    }
}

/// Spell tokens with single spaces where the source had whitespace.
fn join_tokens<'t>(toks: impl Iterator<Item = &'t Token>) -> String {
    let mut s = String::new();
    for (i, t) in toks.enumerate() {
        if i > 0 && (t.ws_before || t.nl_before) {
            s.push(' ');
        }
        s.push_str(&t.text);
    }
    s
}

fn parent_dir(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// Lexically normalize a path, removing `.` and resolving `..` where
/// possible.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    out
}

fn hide_contains(h: &Hide, id: u32) -> bool {
    h.as_ref().is_some_and(|s| s.binary_search(&id).is_ok())
}

fn hide_add(h: &Hide, id: u32) -> Rc<[u32]> {
    match h {
        None => Rc::from([id]),
        Some(s) => match s.binary_search(&id) {
            Ok(_) => s.clone(),
            Err(pos) => {
                let mut v = s.to_vec();
                v.insert(pos, id);
                v.into()
            }
        },
    }
}

fn hide_union(a: &Hide, b: &Rc<[u32]>) -> Hide {
    let Some(a) = a else {
        return Some(b.clone());
    };
    if Rc::ptr_eq(a, b) {
        return Some(b.clone());
    }
    let mut v: Vec<u32> = a.iter().chain(b.iter()).copied().collect();
    v.sort_unstable();
    v.dedup();
    Some(v.into())
}

fn hide_intersect(a: &Hide, b: &Hide) -> Hide {
    let (Some(a), Some(b)) = (a, b) else {
        return None;
    };
    if Rc::ptr_eq(a, b) {
        return Some(a.clone());
    }
    let v: Vec<u32> = a
        .iter()
        .copied()
        .filter(|x| b.binary_search(x).is_ok())
        .collect();
    (!v.is_empty()).then(|| v.into())
}

#[derive(Clone, Copy)]
enum BinOp {
    OrOr,
    AndAnd,
    Or,
    Xor,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Shl,
    Shr,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// Integer constant expression evaluator for `#if`.
struct Eval<'t> {
    toks: &'t [Token],
    pos: usize,
    /// Directive span, for errors at the end of the expression.
    dir: Span,
    /// The first syntax error.
    err: Option<(Span, String)>,
    /// Non-fatal diagnostics such as division by zero.
    diags: Vec<Diagnostic>,
    /// Nesting of unary operators, parentheses, and conditionals, bounded so
    /// that a pathological expression cannot overflow the stack.
    depth: u32,
}

impl Eval<'_> {
    fn fail(&mut self, span: Span, msg: impl Into<String>) {
        if self.err.is_none() {
            self.err = Some((span, msg.into()));
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn cond(&mut self, live: bool) -> i64 {
        if self.depth >= 256 {
            self.fail(self.dir, "#if expression is nested too deeply");
            return 0;
        }
        self.depth += 1;
        let v = self.cond_inner(live);
        self.depth -= 1;
        v
    }

    fn cond_inner(&mut self, live: bool) -> i64 {
        let c = self.binary(1, live);
        if !self.peek().is_some_and(|t| t.is(TokenKind::Question)) {
            return c;
        }
        self.pos += 1;
        let a = self.cond(live && c != 0);
        match self.peek() {
            Some(t) if t.is(TokenKind::Colon) => self.pos += 1,
            Some(t) => {
                let span = t.span;
                self.fail(span, "expected `:` in #if expression");
                return 0;
            }
            None => {
                self.fail(self.dir, "expected `:` in #if expression");
                return 0;
            }
        }
        let b = self.cond(live && c == 0);
        if c != 0 {
            a
        } else {
            b
        }
    }

    /// The binary operator at the cursor: operator, precedence, and the
    /// number of tokens it spans.
    fn peek_op(&self) -> Option<(BinOp, u8, usize)> {
        use TokenKind as K;
        let t = self.peek()?;
        let next = self.toks.get(self.pos + 1).filter(|n| !n.ws_before);
        Some(match t.kind {
            K::PipePipe => (BinOp::OrOr, 1, 1),
            K::AmpAmp => (BinOp::AndAnd, 2, 1),
            K::Pipe => (BinOp::Or, 3, 1),
            K::Caret => (BinOp::Xor, 4, 1),
            K::Amp => (BinOp::And, 5, 1),
            K::EqEq => (BinOp::Eq, 6, 1),
            K::NotEq => (BinOp::Ne, 6, 1),
            K::Lt => (BinOp::Lt, 7, 1),
            K::LtEq => (BinOp::Le, 7, 1),
            K::Gt => match next.map(|n| n.kind) {
                Some(K::Gt) => (BinOp::Shr, 8, 2),
                Some(K::Eq) => (BinOp::Ge, 7, 2),
                _ => (BinOp::Gt, 7, 1),
            },
            K::Shl => (BinOp::Shl, 8, 1),
            K::Plus => (BinOp::Add, 9, 1),
            K::Minus => (BinOp::Sub, 9, 1),
            K::Star => (BinOp::Mul, 10, 1),
            K::Slash => (BinOp::Div, 10, 1),
            K::Percent => (BinOp::Rem, 10, 1),
            _ => return None,
        })
    }

    fn binary(&mut self, min_prec: u8, live: bool) -> i64 {
        let mut lhs = self.unary(live);
        while let Some((op, prec, n)) = self.peek_op() {
            if prec < min_prec || self.err.is_some() {
                break;
            }
            let span = self.toks[self.pos].span;
            self.pos += n;
            let rhs_live = match op {
                BinOp::AndAnd => live && lhs != 0,
                BinOp::OrOr => live && lhs == 0,
                _ => live,
            };
            let rhs = self.binary(prec + 1, rhs_live);
            lhs = match op {
                BinOp::OrOr => (lhs != 0 || rhs != 0) as i64,
                BinOp::AndAnd => (lhs != 0 && rhs != 0) as i64,
                BinOp::Or => lhs | rhs,
                BinOp::Xor => lhs ^ rhs,
                BinOp::And => lhs & rhs,
                BinOp::Eq => (lhs == rhs) as i64,
                BinOp::Ne => (lhs != rhs) as i64,
                BinOp::Lt => (lhs < rhs) as i64,
                BinOp::Le => (lhs <= rhs) as i64,
                BinOp::Gt => (lhs > rhs) as i64,
                BinOp::Ge => (lhs >= rhs) as i64,
                BinOp::Shl => lhs.wrapping_shl(rhs as u32),
                BinOp::Shr => lhs.wrapping_shr(rhs as u32),
                BinOp::Add => lhs.wrapping_add(rhs),
                BinOp::Sub => lhs.wrapping_sub(rhs),
                BinOp::Mul => lhs.wrapping_mul(rhs),
                BinOp::Div | BinOp::Rem if rhs == 0 => {
                    if live {
                        self.diags.push(Diagnostic::error(
                            span,
                            "division by zero in #if expression",
                        ));
                    }
                    0
                }
                BinOp::Div => lhs.wrapping_div(rhs),
                BinOp::Rem => lhs.wrapping_rem(rhs),
            };
        }
        lhs
    }

    fn unary(&mut self, live: bool) -> i64 {
        if self.depth >= 256 {
            self.fail(self.dir, "#if expression is nested too deeply");
            return 0;
        }
        self.depth += 1;
        let v = self.unary_inner(live);
        self.depth -= 1;
        v
    }

    fn unary_inner(&mut self, live: bool) -> i64 {
        let toks = self.toks;
        let Some(t) = toks.get(self.pos) else {
            self.fail(self.dir, "missing operand in #if expression");
            return 0;
        };
        let span = t.span;
        match t.kind {
            TokenKind::Bang => {
                self.pos += 1;
                (self.unary(live) == 0) as i64
            }
            TokenKind::Tilde => {
                self.pos += 1;
                !self.unary(live)
            }
            TokenKind::Minus => {
                self.pos += 1;
                self.unary(live).wrapping_neg()
            }
            TokenKind::Plus => {
                self.pos += 1;
                self.unary(live)
            }
            TokenKind::LParen => {
                self.pos += 1;
                let v = self.cond(live);
                match self.peek() {
                    Some(t) if t.is(TokenKind::RParen) => self.pos += 1,
                    _ => self.fail(span, "unclosed `(` in #if expression"),
                }
                v
            }
            TokenKind::Int => {
                self.pos += 1;
                match parse_int(&t.text) {
                    Some(v) => v,
                    None => {
                        let msg =
                            format!("invalid integer `{}` in #if", t.text);
                        self.fail(span, msg);
                        0
                    }
                }
            }
            TokenKind::Ident => {
                self.pos += 1;
                0
            }
            _ => {
                let msg = format!("unexpected `{}` in #if expression", t.text);
                self.fail(span, msg);
                0
            }
        }
    }
}

/// Parse a C integer literal (decimal, `0x` hex, `0b` binary, or octal),
/// ignoring `u`/`l` suffixes.
fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim_end_matches(['u', 'U', 'l', 'L']);
    let (digits, radix) = if let Some(h) =
        t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))
    {
        (h, 16)
    } else if let Some(b) =
        t.strip_prefix("0b").or_else(|| t.strip_prefix("0B"))
    {
        (b, 2)
    } else if t.len() > 1 && t.starts_with('0') {
        (&t[1..], 8)
    } else {
        (t, 10)
    };
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(digits, radix).ok().map(|v| v as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::Severity;
    use std::sync::Arc;

    fn run_with(files: &[(&str, &str)], opts: &Options) -> (SourceDb, Output) {
        let overlays = files
            .iter()
            .map(|(p, t)| (PathBuf::from(p), Arc::<str>::from(*t)))
            .collect();
        let mut db = SourceDb::new(overlays);
        let root = db.load(Path::new(files[0].0)).unwrap();
        let out = preprocess(&mut db, root, opts);
        (db, out)
    }

    fn run(files: &[(&str, &str)]) -> (SourceDb, Output) {
        run_with(files, &Options::default())
    }

    fn texts(out: &Output) -> String {
        let t: Vec<&str> = out.tokens.iter().map(|t| t.text.as_str()).collect();
        t.join(" ")
    }

    fn errors(out: &Output) -> Vec<&str> {
        out.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.message.as_str())
            .collect()
    }

    #[test]
    fn elif_chain_after_taken_branch() {
        let src = "#define A 2
#if A == 1
one
#elif A == 2
two
#elif 1
three
#else
four
#endif
#ifdef A
# if 0
x
# elif 0 / 0
y
# else
z
# endif
#endif
#ifndef A
  #if 1
  hidden
  #else
  #endif
#endif
";
        let (_, out) = run(&[("/virtual/a.p4", src)]);
        assert_eq!(texts(&out), "two z");
        // `0 / 0` is evaluated because no earlier branch of its chain was
        // taken; expressions after a taken branch are never evaluated.
        assert_eq!(errors(&out), vec!["division by zero in #if expression"]);
    }

    #[test]
    fn defined_inside_macro_and_expression_operators() {
        let src = "#define FOO
#define HAS_FOO defined(FOO)
#if HAS_FOO && !defined BAR && (1 << 4) >> 2 == 4 && 3 >= 2
yes
#endif
#if 0x10 == 020 && -1 < 0 && (0 ? 1 / 0 : 7) == 7 && ~0 == -1
ok
#endif
#if UNKNOWN || 5 % 3 != 2
no
#endif
";
        let (_, out) = run(&[("/virtual/a.p4", src)]);
        assert_eq!(texts(&out), "yes ok");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    #[test]
    fn function_like_requires_adjacent_paren() {
        let src = "#define F (x)
#define G(x) [x]
F(1) G (2) G;
";
        let (_, out) = run(&[("/virtual/a.p4", src)]);
        assert_eq!(texts(&out), "( x ) ( 1 ) [ 2 ] G ;");
        assert_eq!(out.macros[0].params, None);
        assert_eq!(out.macros[1].params, Some(vec!["x".to_string()]));
    }

    #[test]
    fn no_recursive_self_expansion() {
        let src = "#define X X+1
#define Y Z
#define Z Y
#define F(a) F(a) a
X Y F(F(2))
";
        let (_, out) = run(&[("/virtual/a.p4", src)]);
        assert_eq!(texts(&out), "X + 1 Y F ( F ( 2 ) 2 ) F ( 2 ) 2");
    }

    #[test]
    fn hash_operands_are_not_pre_expanded() {
        let src = "#define ONE 1
#define STR(x) #x
#define XSTR(x) STR(x)
#define CAT(a, b) a ## b
#define ID(x) x
STR(ONE) XSTR(ONE) STR( \"q\"  a ) CAT(ON, E) CAT(ONE, 2) ID(ONE) CAT(, x)
";
        let (_, out) = run(&[("/virtual/a.p4", src)]);
        assert_eq!(texts(&out), r#""ONE" "1" "\"q\" a" 1 ONE2 1 x"#);
    }

    #[test]
    fn variadic_macros() {
        let src = "#define V(f, ...) f(__VA_ARGS__)
#define W(f, args...) g(f, ## args)
V(a, 1, (2, 3)) W(b) W(c, 4, 5)
";
        let (_, out) = run(&[("/virtual/a.p4", src)]);
        assert_eq!(texts(&out), "a ( 1 , ( 2 , 3 ) ) g ( b ) g ( c , 4 , 5 )");
        assert!(out.macros[0].variadic);
    }

    #[test]
    fn include_spliced_mid_body() {
        let a = "control C() {\n    #include \"b.p4\"\n    apply {}\n}\n";
        let b = "action x() {}\n";
        let (db, out) = run(&[("/virtual/a.p4", a), ("/virtual/b.p4", b)]);
        assert_eq!(texts(&out), "control C ( ) { action x ( ) { } apply { } }");
        let a_id = db.file_id(Path::new("/virtual/a.p4")).unwrap();
        let b_id = db.file_id(Path::new("/virtual/b.p4")).unwrap();
        let action = out.tokens.iter().find(|t| t.text == "action").unwrap();
        assert_eq!(action.span.file, b_id);
        assert_eq!(db.slice(action.span), "action");
        let apply = out.tokens.iter().find(|t| t.text == "apply").unwrap();
        assert_eq!(apply.span.file, a_id);
        assert_eq!(out.includes.len(), 1);
        assert_eq!(db.slice(out.includes[0].span), "\"b.p4\"");
        assert_eq!(out.includes[0].target, Some(b_id));
        assert!(out.comments.contains_key(&b_id));
    }

    #[test]
    fn include_search_order() {
        let opts = Options {
            include_dirs: vec!["/inc".into()],
            ..Default::default()
        };
        let a = "#include \"x.p4\"\n#include <x.p4>\n#include \"sub/s.p4\"\n";
        let files = [
            ("/virtual/a.p4", a),
            ("/virtual/x.p4", "local\n"),
            ("/inc/x.p4", "system\n"),
            // Found only through the root-directory fallback.
            ("/virtual/sub/s.p4", "#include \"../sub/../y.p4\"\n"),
            ("/virtual/y.p4", "fallback\n"),
        ];
        let (db, out) = run_with(&files, &opts);
        assert_eq!(texts(&out), "local system fallback");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let y = out.includes[3].target.unwrap();
        assert_eq!(db.path(y), Path::new("/virtual/y.p4"));
    }

    #[test]
    fn missing_include_reports_path_span() {
        let a = "a\n#include <nope.p4>\nb\n";
        let (db, out) = run(&[("/virtual/a.p4", a)]);
        assert_eq!(texts(&out), "a b");
        assert_eq!(out.includes[0].target, None);
        assert_eq!(db.slice(out.includes[0].span), "<nope.p4>");
        let d = &out.diagnostics[0];
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.span, out.includes[0].span);
        assert!(d.message.contains("nope.p4"), "{}", d.message);
        assert!(d.message.contains("/virtual"), "{}", d.message);
    }

    #[test]
    fn pragma_once_guards_and_recursion() {
        let a = "#include \"o.p4\"\n#include \"o.p4\"\n#include \"g.p4\"\n\
                 #include \"g.p4\"\n#include \"r.p4\"\n";
        let files = [
            ("/virtual/a.p4", a),
            ("/virtual/o.p4", "#pragma once\nonce\n"),
            ("/virtual/g.p4", "#ifndef G\n#define G\nguarded\n#endif\n"),
            ("/virtual/r.p4", "#include \"r.p4\"\n"),
        ];
        let (_, out) = run(&files);
        assert_eq!(texts(&out), "once guarded");
        assert_eq!(errors(&out), vec!["#include nested too deeply"]);
    }

    #[test]
    fn inactive_region_spans() {
        let src =
            "#if 0\nskip1\nskip2\n#elif 1\nlive\n  #else\nskip3\n#endif\n\
                   #ifdef NOPE\n#if 1\nnested\n#endif\n#endif\n";
        let (db, out) = run(&[("/virtual/a.p4", src)]);
        assert_eq!(texts(&out), "live");
        let regions: Vec<&str> =
            out.inactive.iter().map(|s| db.slice(*s)).collect();
        assert_eq!(
            regions,
            vec!["skip1\nskip2", "skip3", "#if 1\nnested\n#endif"]
        );
    }

    #[test]
    fn expansion_spans_and_macro_uses() {
        let src = "#define F(a, b) a + G\n#define G 7\nx  F(1, 2) y\n\
                   #ifdef F\n#endif\n#undef G\nG\n";
        let (db, out) = run(&[("/virtual/a.p4", src)]);
        assert_eq!(texts(&out), "x 1 + 7 y G");
        let one = &out.tokens[1];
        assert_eq!(db.slice(one.span), "F(1, 2)");
        assert!(one.ws_before && !one.nl_before);
        // `G` expanded inside F's body carries F's invocation span.
        assert_eq!(db.slice(out.tokens[3].span), "F(1, 2)");
        assert_eq!(db.slice(out.tokens[4].span), "y");
        let uses: Vec<(&str, usize)> = out
            .macro_uses
            .iter()
            .map(|(s, i)| (db.slice(*s), *i))
            .collect();
        // Invocation, `#ifdef`, `#undef`; not `G` inside F's body.
        assert_eq!(uses, vec![("F", 0), ("F", 0), ("G", 1)]);
        assert_eq!(db.slice(out.macros[1].span.unwrap()), "G");
        assert_eq!(out.macros[0].body, "a + G");
    }

    #[test]
    fn predefines_redefinition_and_directive_messages() {
        let opts = Options {
            defines: vec![
                ("T".into(), "2".into()),
                ("SQ(x)".into(), "x*x".into()),
            ],
            ..Default::default()
        };
        let src = "#if T == 2\nt2 SQ(3)\n#endif\n\
                   #define A 1\n#define A  1\n#define A 2\n\
                   #warning careful now\n#error stop here\n#bogus\n";
        let (_, out) = run_with(&[("/virtual/a.p4", src)], &opts);
        assert_eq!(texts(&out), "t2 3 * 3");
        assert_eq!(out.macros[0].span, None);
        let msgs: Vec<(Severity, &str)> = out
            .diagnostics
            .iter()
            .map(|d| (d.severity, d.message.as_str()))
            .collect();
        assert_eq!(
            msgs,
            vec![
                (Severity::Warning, "`A` redefined"),
                (Severity::Warning, "careful now"),
                (Severity::Error, "stop here"),
                (Severity::Error, "unknown preprocessing directive `#bogus`"),
            ]
        );
    }

    #[test]
    fn conditional_structure_errors() {
        let (_, out) = run(&[("/virtual/a.p4", "#endif\n#else\n#if 1\nx\n")]);
        assert_eq!(texts(&out), "x");
        assert_eq!(
            errors(&out),
            vec![
                "#endif without #if",
                "#else without #if",
                "unterminated conditional"
            ]
        );
        // A conditional cannot be closed by an included file.
        let files = [
            ("/virtual/a.p4", "#if 1\n#include \"b.p4\"\n#endif\n"),
            ("/virtual/b.p4", "#endif\n#if 1\n"),
        ];
        let (_, out) = run(&files);
        assert_eq!(
            errors(&out),
            vec!["#endif without #if", "unterminated conditional"]
        );
    }

    #[test]
    fn malformed_input_does_not_panic() {
        let src = "#define\n#define 3\n#define F(x\n#define H(a, a) a\n\
                   #if\n#endif\n#if (1\n#endif\n#if 1 +\n#endif\n\
                   #ifdef\n#endif\n\
                   #include\n#include <a\n#undef\n#if defined(\n#endif\n\
                   #define K(x) #\n#define P ## x\n#define Q x ##\nP Q K(1)\n\
                   #define M(a) a\nM(1, 2) M\nM(unterminated (\n";
        let (_, out) = run(&[("/virtual/a.p4", src)]);
        assert!(errors(&out).len() >= 12, "{:?}", errors(&out));
        assert!(errors(&out)
            .contains(&"unterminated argument list invoking macro `M`"));
        // Deep nesting is an error, not a stack overflow.
        let deep =
            format!("#if {}1{}\n#endif\n", "(".repeat(5000), ")".repeat(5000));
        let (_, deep_out) = run(&[("/virtual/b.p4", deep.as_str())]);
        assert_eq!(errors(&deep_out), ["#if expression is nested too deeply"]);
        // Tokens of failed invocations are kept.
        assert!(
            texts(&out).ends_with("M ( unterminated ("),
            "{}",
            texts(&out)
        );
    }
}
