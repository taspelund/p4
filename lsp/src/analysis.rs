// Copyright 2026 Oxide Computer Company

//! Editor-facing queries over one analyzed compilation root.
//!
//! An [`Analysis`] is an immutable snapshot: the server builds a new one
//! whenever a file in the root's include closure changes. All positions are
//! byte offsets into [`SourceDb`] files; the server converts them to LSP
//! positions.

use crate::diag::{Diagnostic, Severity};
use crate::preprocess::{self, Options};
use crate::sema::{self, DefId, Sema};
use crate::source::{FileId, SourceDb, Span};
use crate::syntax;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Constant,
    Variable,
    Parameter,
    TypeParameter,
    Typedef,
    Struct,
    Header,
    HeaderUnion,
    Field,
    Enum,
    EnumMember,
    Error,
    MatchKind,
    Extern,
    Method,
    Function,
    Action,
    Table,
    Control,
    Parser,
    State,
    Package,
    Instance,
    ValueSet,
    Macro,
    Keyword,
    /// Completion only: a snippet, a file, or a directory.
    Snippet,
    File,
    Folder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hover {
    /// The range the hover applies to.
    pub span: Span,
    pub markdown: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    /// The whole declaration.
    pub span: Span,
    /// The declared name.
    pub selection: Span,
    pub children: Vec<DocumentSymbol>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// The declared name.
    pub span: Span,
    pub container: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightKind {
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub kind: SymbolKind,
    pub detail: Option<String>,
    pub documentation: Option<String>,
    /// Edits applied along with the completion, such as an `#include` for
    /// a name from a header the program does not include yet. Always in
    /// the file being completed.
    pub additional_edits: Vec<(Span, String)>,
    /// Text to insert instead of the label.
    pub insert: Option<String>,
    /// Whether `insert` uses snippet syntax (`$1`, `${1:name}`, `$0`).
    /// The server drops snippets for clients without snippet support.
    pub snippet: bool,
    /// The range `insert` replaces, when it is not the word at the cursor.
    pub replace: Option<Span>,
}

impl Completion {
    fn new(label: impl Into<String>, kind: SymbolKind) -> Self {
        Completion {
            label: label.into(),
            kind,
            detail: None,
            documentation: None,
            additional_edits: Vec::new(),
            insert: None,
            snippet: false,
            replace: None,
        }
    }
}

/// Declaration and statement skeletons offered with keywords:
/// `(label, detail, snippet)`.
const SNIPPETS: &[(&str, &str, &str)] = &[
    (
        "action",
        "action name(...) { }",
        "action ${1:name}($2) {\n\t$0\n}",
    ),
    (
        "control",
        "control Name(...) { apply { } }",
        "control ${1:Name}($2) {\n\tapply {\n\t\t$0\n\t}\n}",
    ),
    (
        "header",
        "header name { }",
        "header ${1:name} {\n\t$0\n}",
    ),
    (
        "if",
        "if (...) { }",
        "if (${1:cond}) {\n\t$0\n}",
    ),
    (
        "parser",
        "parser Name(packet_in pkt, ...) { state start { } }",
        "parser ${1:Name}(packet_in ${2:pkt}$3) {\n\tstate start {\n\t\t$0\n\t\ttransition accept;\n\t}\n}",
    ),
    (
        "state",
        "state name { transition ...; }",
        "state ${1:name} {\n\t$0\n\ttransition ${2:accept};\n}",
    ),
    (
        "struct",
        "struct name { }",
        "struct ${1:name} {\n\t$0\n}",
    ),
    (
        "table",
        "table name { key actions size default_action }",
        "table ${1:name} {\n\tkey = {\n\t\t${2:field}: ${3:exact};\n\t}\n\tactions = {\n\t\t${4:NoAction};\n\t}\n\tsize = ${5:1024};\n\tdefault_action = ${6:NoAction};\n}$0",
    ),
    (
        "transition select",
        "transition select(...) { ... }",
        "transition select(${1:expr}) {\n\t${2:value}: ${3:state};\n\tdefault: ${4:accept};\n}$0",
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeActionKind {
    QuickFix,
    /// Generates code (fill `switch` cases, deparser `emit`s, ...).
    Generate,
    /// Replaces code with equivalent code.
    Rewrite,
    /// Inlines a definition, such as a macro.
    Inline,
    /// Extracts code into a new declaration, such as a constant.
    Extract,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeAction {
    pub title: String,
    pub kind: CodeActionKind,
    /// Replacements, possibly in several files. Insertions have empty
    /// spans.
    pub edits: Vec<(Span, String)>,
    /// The span of the diagnostic this fixes, if any.
    pub fixes: Option<Span>,
    /// The fix the editor may apply without asking.
    pub preferred: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub label: String,
    /// Byte ranges of each parameter within `label`.
    pub params: Vec<(u32, u32)>,
    pub documentation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureHelp {
    pub signatures: Vec<Signature>,
    pub active_signature: u32,
    pub active_param: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlayHintKind {
    /// `name:` before an argument, padded on the right.
    Parameter,
    /// What a closing `}` or `#endif` ends, padded on the left.
    Closing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlayHint {
    /// Offset the hint is displayed at.
    pub file: FileId,
    pub offset: u32,
    pub label: String,
    pub kind: InlayHintKind,
}

/// Blocks and conditionals shorter than this many lines get no closing
/// hint.
const CLOSING_HINT_LINES: u32 = 15;

/// What an identifier occurrence refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Def(DefId),
    Macro(usize),
    Include(usize),
}

/// Where completion was requested; see [`Analysis::completion_context`].
enum Context {
    /// Anywhere else: everything in scope, keywords, and snippets.
    Any,
    /// A state name after `transition` (`select` too) or in a `select` case.
    State { select: bool },
    /// After `:` in a table's `key`.
    MatchKind,
    /// An entry of a table's `actions` list, with the names already listed.
    TableActions(Vec<String>),
    /// After `default_action =`, with the actions the table lists (offered
    /// first).
    DefaultAction(Vec<String>),
    /// A property of a table body, with the properties already present.
    TableProperty {
        present: Vec<String>,
        after_const: bool,
    },
}

/// Table properties: name, detail, and snippet.
const TABLE_PROPERTIES: &[(&str, &str, &str)] = &[
    (
        "key",
        "key = { ... }",
        "key = {\n\t${1:field}: ${2:exact};\n}",
    ),
    (
        "actions",
        "actions = { ... }",
        "actions = {\n\t${1:NoAction};\n}",
    ),
    (
        "default_action",
        "default_action = action(...);",
        "default_action = ${1:NoAction}();",
    ),
    ("size", "size = N;", "size = ${1:1024};"),
    ("entries", "entries = { ... }", "entries = {\n\t$0\n}"),
    (
        "implementation",
        "implementation = instance;",
        "implementation = ${1:instance};",
    ),
    (
        "counters",
        "counters = instance;",
        "counters = ${1:instance};",
    ),
    ("meters", "meters = instance;", "meters = ${1:instance};"),
    (
        "largest_priority_wins",
        "largest_priority_wins = bool;",
        "largest_priority_wins = ${1:true};",
    ),
    (
        "priority_delta",
        "priority_delta = N;",
        "priority_delta = ${1:1};",
    ),
];

const KEYWORDS: &[&str] = &[
    "abstract",
    "action",
    "actions",
    "apply",
    "bit",
    "bool",
    "const",
    "control",
    "default",
    "default_action",
    "else",
    "entries",
    "enum",
    "error",
    "exit",
    "extern",
    "false",
    "header",
    "header_union",
    "if",
    "in",
    "inout",
    "int",
    "key",
    "match_kind",
    "out",
    "package",
    "parser",
    "return",
    "select",
    "size",
    "state",
    "string",
    "struct",
    "switch",
    "table",
    "transition",
    "true",
    "tuple",
    "type",
    "typedef",
    "varbit",
    "void",
];

pub struct Analysis {
    pub db: SourceDb,
    pub root: FileId,
    pub pp: preprocess::Output,
    pub parse: syntax::Parse,
    pub sema: Sema,
    /// The preprocessor options the root was analyzed with.
    pub opts: Options,
    diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    pub fn new(mut db: SourceDb, root: FileId, opts: &Options) -> Self {
        let pp = preprocess::preprocess(&mut db, root, opts);
        let parse = syntax::parse(&pp.tokens);
        // A missing include makes every name it declares unresolved;
        // report the include alone rather than a cascade.
        let complete = pp.includes.iter().all(|i| i.target.is_some());
        let mut sema = sema::analyze(&parse.program, &db, complete);
        // A declaration used only in code the preprocessor skipped (another
        // build configuration) or in a macro body is not unused.
        let hidden_use = |name: &str| {
            let is_word = |text: &str| {
                text.match_indices(name).any(|(i, _)| {
                    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
                    !text[..i].ends_with(ident)
                        && !text[i + name.len()..].starts_with(ident)
                })
            };
            pp.inactive.iter().any(|&s| is_word(db.slice(s)))
                || pp.macros.iter().any(|m| is_word(&m.body))
        };
        let hidden: Vec<Span> = sema
            .unused
            .iter()
            .filter(|&&d| hidden_use(&sema.def(d).name))
            .filter_map(|&d| sema.def(d).name_span)
            .collect();
        sema.unused.retain(|&d| {
            !sema.defs[d.0 as usize]
                .name_span
                .is_some_and(|s| hidden.contains(&s))
        });
        sema.diagnostics
            .retain(|d| d.code != Some("unused") || !hidden.contains(&d.span));
        // Lexical errors in skipped conditional blocks (an apostrophe in
        // `#if 0` prose) do not matter, as in C, and neither do odd
        // characters in annotation bodies.
        let mut diagnostics: Vec<Diagnostic> = pp
            .diagnostics
            .iter()
            .filter(|d| {
                let within = |i: &Span| {
                    i.file == d.span.file
                        && i.start <= d.span.start
                        && d.span.end <= i.end
                };
                // Annotation bodies are free-form token lists.
                let in_annotation =
                    d.message.starts_with("unexpected character")
                        && parse.annotation_spans.iter().any(within);
                !pp.inactive.iter().any(within) && !in_annotation
            })
            .cloned()
            .collect();
        diagnostics.extend(parse.diagnostics.iter().cloned());
        // x4c widens and truncates between fixed widths implicitly, so for
        // programs using its `SoftNPU` package width mismatches are only
        // warnings.
        let x4c = sema.refs.iter().any(|r| {
            let d = sema.def(r.def);
            d.kind == SymbolKind::Package && d.name == "SoftNPU"
        });
        diagnostics.extend(sema.diagnostics.iter().cloned().map(|mut d| {
            if x4c && d.code == Some("width") {
                d.severity = Severity::Warning;
                d.message
                    .push_str(" (x4c converts implicitly; p4c rejects this)");
            }
            d
        }));
        for &span in &pp.inactive {
            let mut d = Diagnostic::new(
                span,
                Severity::Hint,
                "inactive preprocessor block",
            )
            .with_code("inactive");
            d.unnecessary = true;
            diagnostics.push(d);
        }
        diagnostics.sort_by_key(|d| (d.span.file, d.span.start));
        Self {
            db,
            root,
            pp,
            parse,
            sema,
            opts: opts.clone(),
            diagnostics,
        }
    }

    /// Every file loaded while analyzing the root, including headers.
    pub fn files(&self) -> impl Iterator<Item = FileId> + '_ {
        self.db.files()
    }

    /// Diagnostics for all files in the root's include closure.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The occurrence under the cursor. A cursor on the first character of
    /// a name or just past its last character selects it; the former wins
    /// when two names are adjacent.
    fn target_at(&self, file: FileId, offset: u32) -> Option<(Span, Target)> {
        let pp = self
            .pp
            .includes
            .iter()
            .enumerate()
            .map(|(i, inc)| (inc.span, Target::Include(i)))
            .chain(
                self.pp
                    .macros
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| Some((m.span?, Target::Macro(i)))),
            )
            .chain(
                self.pp
                    .macro_uses
                    .iter()
                    .map(|&(s, i)| (s, Target::Macro(i))),
            );
        // Names expanded from macros carry the invocation's span, which the
        // macro occurrence already covers.
        if let Some(best) = best_at(pp, file, offset) {
            return Some(best);
        }
        let sema = self
            .sema
            .refs
            .iter()
            .map(|r| (r.span, Target::Def(r.def)))
            .chain(self.sema.defs.iter().enumerate().filter_map(|(i, d)| {
                Some((d.name_span?, Target::Def(DefId(i as u32))))
            }));
        best_at(sema, file, offset)
    }

    pub fn goto_definition(&self, file: FileId, offset: u32) -> Vec<Span> {
        match self.target_at(file, offset) {
            Some((span, Target::Def(d))) => {
                // A fragment included in several places resolves to a
                // different declaration at each; offer them all.
                let mut out: Vec<Span> =
                    self.sema.def(d).name_span.into_iter().collect();
                for r in &self.sema.refs {
                    if r.span == span {
                        if let Some(s) = self.sema.def(r.def).name_span {
                            if !out.contains(&s) {
                                out.push(s);
                            }
                        }
                    }
                }
                out.sort();
                out
            }
            Some((_, Target::Macro(m))) => {
                self.pp.macros[m].span.into_iter().collect()
            }
            Some((_, Target::Include(i))) => self.pp.includes[i]
                .target
                .map(|f| Span::new(f, 0, 0))
                .into_iter()
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn goto_type_definition(&self, file: FileId, offset: u32) -> Vec<Span> {
        let Some((_, Target::Def(d))) = self.target_at(file, offset) else {
            return Vec::new();
        };
        let def = self.sema.def(d);
        let ty = match def.kind {
            SymbolKind::Typedef => sema::Ty::Def(d, Vec::new()),
            _ => def.ty.clone(),
        };
        self.sema
            .type_def(&ty)
            .and_then(|t| self.sema.def(t).name_span)
            .into_iter()
            .collect()
    }

    /// Where the declaration at the cursor is implemented or used as a
    /// type:
    /// - an architecture's parser, control, or package type (or a package
    ///   parameter of one): the blocks and instances the program passes
    ///   for it;
    /// - an action: the tables that list it;
    /// - a package or extern: its instances.
    pub fn goto_implementation(&self, file: FileId, offset: u32) -> Vec<Span> {
        let Some((_, Target::Def(d))) = self.target_at(file, offset) else {
            return Vec::new();
        };
        let def = self.sema.def(d);
        let d = match def.kind {
            SymbolKind::Parameter => match self.sema.type_def(&def.ty) {
                Some(t) => t,
                None => return Vec::new(),
            },
            _ => d,
        };
        let kind = self.sema.def(d).kind;
        let mut out: Vec<DefId> = self
            .sema
            .implementations
            .iter()
            .filter(|&&(abs, _)| abs == d)
            .map(|&(_, c)| c)
            .collect();
        if kind == SymbolKind::Action {
            let mut tables: Vec<DefId> = self
                .sema
                .table_actions
                .iter()
                .filter(|(_, acts)| acts.contains(&d))
                .map(|(&t, _)| t)
                .collect();
            tables.sort();
            out.extend(tables);
        }
        if matches!(kind, SymbolKind::Package | SymbolKind::Extern) {
            out.extend(self.sema.defs.iter().enumerate().filter_map(
                |(i, x)| {
                    (x.kind == SymbolKind::Instance
                        && self.sema.type_def(&x.ty) == Some(d))
                    .then_some(DefId(i as u32))
                },
            ));
        }
        let mut spans: Vec<Span> = out
            .into_iter()
            .filter_map(|c| self.sema.def(c).name_span)
            .collect();
        let mut seen = std::collections::HashSet::new();
        spans.retain(|s| seen.insert(*s));
        spans
    }

    /// The comment documenting a declaration: comments directly above it
    /// (with no blank line between), or else a comment trailing it on the
    /// same line.
    fn doc(&self, span: Span) -> Option<String> {
        let comments = self.pp.comments.get(&span.file)?;
        let text = self.db.text(span.file);
        let idx = comments.partition_point(|c| c.end <= span.start);
        let mut lines = Vec::new();
        let mut next_start = span.start as usize;
        for c in comments[..idx].iter().rev() {
            let gap = &text[c.end as usize..next_start];
            if !gap.trim().is_empty() || gap.matches('\n').count() > 1 {
                break;
            }
            // A comment ending a line of code belongs to that code.
            let line_start =
                text[..c.start as usize].rfind('\n').map_or(0, |n| n + 1);
            if !text[line_start..c.start as usize].trim().is_empty() {
                break;
            }
            lines.push(clean_comment(&text[c.start as usize..c.end as usize]));
            next_start = c.start as usize;
        }
        if lines.is_empty() {
            let line_end = text[span.end as usize..]
                .find('\n')
                .map_or(text.len(), |n| span.end as usize + n);
            let trailing = comments[idx..].iter().find(|c| {
                c.start as usize >= span.end as usize
                    && (c.start as usize) < line_end
                    && text[span.end as usize..c.start as usize]
                        .trim_matches(|ch: char| {
                            ch.is_whitespace() || ch == ';' || ch == ','
                        })
                        .is_empty()
            })?;
            return Some(clean_comment(
                &text[trailing.start as usize..trailing.end as usize],
            ))
            .filter(|s| !s.is_empty());
        }
        lines.reverse();
        let doc = lines.join("\n");
        (!doc.trim().is_empty()).then_some(doc)
    }

    fn def_doc(&self, d: DefId) -> Option<String> {
        let def = self.sema.def(d);
        let span = def.span?;
        // Declarations spanning files start in the file of their name.
        let span = match def.name_span {
            Some(n) if n.file != span.file => n,
            _ => span,
        };
        self.doc(span)
    }

    fn def_markdown(&self, d: DefId) -> String {
        let def = self.sema.def(d);
        let mut md = format!("```p4\n{}\n```", def.detail);
        if let Some(parent) = def.parent {
            let p = self.sema.def(parent);
            if !p.name.is_empty() && def.kind != SymbolKind::EnumMember {
                md.push_str(&format!("\n\nin `{}`", p.detail));
            }
        }
        if let Some(&v) = self.sema.const_values.get(&d) {
            let width = match self.sema.resolve_alias(&def.ty) {
                sema::Ty::Bits { width, .. } => width,
                _ => None,
            };
            md.push_str(&format!("\n\nvalue: {v} · {}", radixes(v, width)));
        }
        md.push_str(&self.layout_markdown(d));
        if let Some(doc) = self.def_doc(d) {
            md.push_str("\n\n---\n\n");
            md.push_str(&doc);
        }
        md
    }

    /// The size in bits of a fixed-width type, and whether it contains a
    /// `varbit` (the size is then the minimum).
    fn width_of(&self, ty: &sema::Ty, depth: u32) -> Option<(u32, bool)> {
        use sema::Ty;
        if depth > 16 {
            return None;
        }
        match self.sema.resolve_alias(ty) {
            Ty::Bits { width, .. } => Some((width?, false)),
            Ty::Varbit(_) => Some((0, true)),
            Ty::Bool => Some((1, false)),
            Ty::Stack(elem, n) => {
                let (w, v) = self.width_of(&elem, depth + 1)?;
                Some((w.checked_mul(n?)?, v))
            }
            Ty::Def(id, _) => {
                let def = self.sema.def(id);
                match def.kind {
                    SymbolKind::Enum => {
                        self.width_of(self.sema.enum_reprs.get(&id)?, depth + 1)
                    }
                    SymbolKind::Struct | SymbolKind::Header => {
                        let mut total = (0u32, false);
                        for &m in &def.members {
                            let (w, v) =
                                self.width_of(&self.sema.def(m).ty, depth + 1)?;
                            total = (total.0.checked_add(w)?, total.1 || v);
                        }
                        Some(total)
                    }
                    SymbolKind::HeaderUnion => {
                        let mut max = (0u32, false);
                        for &m in &def.members {
                            let (w, v) =
                                self.width_of(&self.sema.def(m).ty, depth + 1)?;
                            max = (max.0.max(w), max.1 || v);
                        }
                        Some(max)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// For headers (and structs of plain fields): the size, and each
    /// field's bit offset and width. For a header field: its position.
    fn layout_markdown(&self, d: DefId) -> String {
        let def = self.sema.def(d);
        let size = |(w, v): (u32, bool)| {
            let bytes = if w.is_multiple_of(8) {
                let b = w / 8;
                format!(" ({b} byte{})", if b == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            let min = if v { "at least " } else { "" };
            let plural = if w == 1 { "" } else { "s" };
            format!("{min}{w} bit{plural}{bytes}")
        };
        let plain = |d: &sema::Def| {
            d.members.iter().all(|&m| {
                let ty = self.sema.resolve_alias(&self.sema.def(m).ty);
                self.sema.type_def(&ty).is_none_or(|t| {
                    let k = self.sema.def(t).kind;
                    k == SymbolKind::Enum
                        || (k == SymbolKind::Struct
                            && !matches!(ty, sema::Ty::Stack(..)))
                })
            })
        };
        match def.kind {
            SymbolKind::Header | SymbolKind::Struct
                if def.kind == SymbolKind::Header || plain(def) =>
            {
                let Some(total) = self.width_of(&def.ty, 0) else {
                    return String::new();
                };
                let mut md = format!("\n\nsize: {}", size(total));
                if def.members.is_empty() || def.members.len() > 64 {
                    return md;
                }
                let mut rows = Vec::new();
                let mut at = Some(0u32);
                for &m in &def.members {
                    let f = self.sema.def(m);
                    let w = self.width_of(&f.ty, 0);
                    let pos = match (at, w) {
                        (Some(a), Some((1, false))) => a.to_string(),
                        (Some(a), Some((w, false))) if w > 0 => {
                            format!("{a}..{}", a + w - 1)
                        }
                        (Some(a), _) => format!("{a}.."),
                        _ => "?".into(),
                    };
                    let width = match w {
                        Some((_, true)) => "var".into(),
                        Some((w, _)) => w.to_string(),
                        None => "?".into(),
                    };
                    rows.push((pos, width, f.name.clone()));
                    at = match (at, w) {
                        (Some(a), Some((w, false))) => Some(a + w),
                        _ => None,
                    };
                }
                let pw =
                    rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(4);
                let ww =
                    rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(5);
                md.push_str(&format!(
                    "\n\n```text\n{:<pw$}  {:>ww$}  field\n",
                    "bits", "width"
                ));
                for (pos, width, name) in rows {
                    md.push_str(&format!("{pos:<pw$}  {width:>ww$}  {name}\n"));
                }
                md.push_str("```");
                md
            }
            SymbolKind::Field => {
                let Some(parent) = def.parent else {
                    return String::new();
                };
                let p = self.sema.def(parent);
                let Some((w, var)) = self.width_of(&def.ty, 0) else {
                    return String::new();
                };
                if p.kind != SymbolKind::Header || var {
                    return if var {
                        String::new()
                    } else {
                        format!("\n\n{}", size((w, false)))
                    };
                }
                let mut at = 0u32;
                for &m in &p.members {
                    if m == d {
                        let bits = if w <= 1 {
                            format!("bit {at}")
                        } else {
                            format!("bits {at}..{}", at + w - 1)
                        };
                        let byte = if at.is_multiple_of(8) {
                            format!(", byte {}", at / 8)
                        } else {
                            String::new()
                        };
                        return format!(
                            "\n\n{}, {bits} of `{}`{byte}",
                            size((w, false)),
                            p.name
                        );
                    }
                    match self.width_of(&self.sema.def(m).ty, 0) {
                        Some((mw, false)) => at += mw,
                        _ => return String::new(),
                    }
                }
                String::new()
            }
            _ => String::new(),
        }
    }

    /// What a macro invocation expands to, from the preprocessed tokens.
    fn macro_expansion(&self, use_span: Span) -> Option<String> {
        // Function-like invocations carry the span of the whole call.
        let from_use = |t: &crate::lexer::Token| {
            t.span.file == use_span.file
                && t.span.start == use_span.start
                && t.span.end >= use_span.end
        };
        let start = self.pp.tokens.iter().position(from_use)?;
        let mut out = String::new();
        for (i, t) in self.pp.tokens[start..]
            .iter()
            .take_while(|t| from_use(t))
            .enumerate()
        {
            if i > 0 && t.ws_before {
                out.push(' ');
            }
            out.push_str(&t.text);
            if out.len() > 400 {
                out.push_str(" …");
                break;
            }
        }
        Some(out)
    }

    /// Hover for an integer literal: its value in several radixes.
    fn literal_hover(&self, file: FileId, offset: u32) -> Option<Hover> {
        let text = self.db.text(file);
        let o = text.floor_char_boundary(offset as usize);
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let start = text[..o].trim_end_matches(word).len();
        let end =
            o + text[o..].find(|c: char| !word(c)).unwrap_or(text.len() - o);
        let lit = &text[start..end];
        if !lit.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let parsed = syntax::parser::parse_int(lit)?;
        let v = parsed.value?;
        Some(Hover {
            span: Span::new(file, start as u32, end as u32),
            markdown: format!("{v} · {}", radixes(v, parsed.width)),
        })
    }

    fn macro_signature(&self, m: usize) -> String {
        let m = &self.pp.macros[m];
        let params = m
            .params
            .as_ref()
            .map(|p| format!("({})", p.join(", ")))
            .unwrap_or_default();
        let body = m.body.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("#define {}{} {}", m.name, params, body)
    }

    pub fn hover(&self, file: FileId, offset: u32) -> Option<Hover> {
        let Some((span, target)) = self.target_at(file, offset) else {
            return self.literal_hover(file, offset);
        };
        let markdown = match target {
            Target::Def(d) => self.def_markdown(d),
            Target::Macro(m) => {
                let mut md = format!("```c\n{}\n```", self.macro_signature(m));
                let body: String =
                    self.pp.macros[m].body.split_whitespace().collect();
                if let Some(e) = self.macro_expansion(span).filter(|e| {
                    e.split_whitespace().collect::<String>() != body
                }) {
                    md.push_str(&format!("\n\nexpands to\n```p4\n{e}\n```"));
                }
                match self.pp.macros[m].span {
                    Some(s) => {
                        // Documentation precedes the `#define` line.
                        let text = self.db.text(s.file);
                        let line = text[..s.start as usize]
                            .rfind('\n')
                            .map_or(0, |n| n + 1);
                        if let Some(doc) =
                            self.doc(Span::new(s.file, line as u32, s.end))
                        {
                            md.push_str("\n\n---\n\n");
                            md.push_str(&doc);
                        }
                    }
                    None => md.push_str("\n\npredefined"),
                }
                md
            }
            Target::Include(i) => match self.pp.includes[i].target {
                Some(f) => format!("`{}`", self.db.path(f).display()),
                None => "file not found".into(),
            },
        };
        Some(Hover { span, markdown })
    }

    /// Every occurrence of a target, declaration first. Occurrences that
    /// come from macro expansion are skipped: their spans cover the macro
    /// invocation, not the name.
    fn occurrences(&self, target: Target) -> Vec<(Span, HighlightKind)> {
        let mut out = Vec::new();
        match target {
            Target::Def(d) => {
                let def = self.sema.def(d);
                if let Some(s) = def.name_span {
                    out.push((s, HighlightKind::Write));
                }
                for r in &self.sema.refs {
                    if r.def == d && self.db.slice(r.span) == def.name {
                        let kind = if r.write {
                            HighlightKind::Write
                        } else {
                            HighlightKind::Read
                        };
                        out.push((r.span, kind));
                    }
                }
            }
            Target::Macro(m) => {
                if let Some(s) = self.pp.macros[m].span {
                    out.push((s, HighlightKind::Write));
                }
                for &(s, i) in &self.pp.macro_uses {
                    if i == m {
                        out.push((s, HighlightKind::Read));
                    }
                }
            }
            Target::Include(i) => {
                out.push((self.pp.includes[i].span, HighlightKind::Read))
            }
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|(s, _)| seen.insert(*s));
        out
    }

    pub fn references(
        &self,
        file: FileId,
        offset: u32,
        include_declaration: bool,
    ) -> Vec<Span> {
        let Some((_, target)) = self.target_at(file, offset) else {
            return Vec::new();
        };
        let decl = match target {
            Target::Def(d) => self.sema.def(d).name_span,
            Target::Macro(m) => self.pp.macros[m].span,
            Target::Include(_) => None,
        };
        self.occurrences(target)
            .into_iter()
            .map(|(s, _)| s)
            .filter(|s| include_declaration || Some(*s) != decl)
            .collect()
    }

    /// Occurrences of the symbol under the cursor within `file`.
    pub fn document_highlight(
        &self,
        file: FileId,
        offset: u32,
    ) -> Vec<(Span, HighlightKind)> {
        let Some((_, target)) = self.target_at(file, offset) else {
            return Vec::new();
        };
        let mut out = self.occurrences(target);
        out.retain(|(s, _)| s.file == file);
        out
    }

    /// Whether a declaration is shown in symbol lists.
    fn is_outline_symbol(&self, d: &sema::Def) -> bool {
        d.name_span.is_some()
            && !d.local
            && !d.name.is_empty()
            && !matches!(
                d.kind,
                SymbolKind::Parameter | SymbolKind::TypeParameter
            )
    }

    fn container_name(&self, d: &sema::Def) -> Option<String> {
        d.parent
            .map(|p| self.sema.def(p).name.clone())
            .filter(|n| !n.is_empty())
    }

    pub fn document_symbols(&self, file: FileId) -> Vec<DocumentSymbol> {
        let in_file = |d: &sema::Def| {
            self.is_outline_symbol(d)
                && d.name_span.is_some_and(|s| s.file == file)
        };
        let mut children: std::collections::HashMap<Option<DefId>, Vec<DefId>> =
            Default::default();
        for (i, d) in self.sema.defs.iter().enumerate() {
            if !in_file(d) {
                continue;
            }
            // Members of declarations in other files (a control body
            // fragment) are roots in this file.
            let parent = d.parent.filter(|&p| in_file(self.sema.def(p)));
            children.entry(parent).or_default().push(DefId(i as u32));
        }
        fn build(
            a: &Analysis,
            children: &std::collections::HashMap<Option<DefId>, Vec<DefId>>,
            parent: Option<DefId>,
        ) -> Vec<DocumentSymbol> {
            let mut out: Vec<DocumentSymbol> = children
                .get(&parent)
                .into_iter()
                .flatten()
                .map(|&id| {
                    let d = a.sema.def(id);
                    let selection = d.name_span.unwrap();
                    let span = d
                        .span
                        .filter(|s| {
                            s.file == selection.file
                                && s.start <= selection.start
                                && selection.end <= s.end
                        })
                        .unwrap_or(selection);
                    DocumentSymbol {
                        name: d.name.clone(),
                        detail: Some(d.detail.clone()),
                        kind: d.kind,
                        span,
                        selection,
                        children: build(a, children, Some(id)),
                    }
                })
                .collect();
            out.sort_by_key(|s| s.selection.start);
            out
        }
        let mut out = build(self, &children, None);
        for m in &self.pp.macros {
            if let Some(span) = m.span.filter(|s| s.file == file) {
                out.push(DocumentSymbol {
                    name: m.name.clone(),
                    detail: Some(m.body.clone()),
                    kind: SymbolKind::Macro,
                    span,
                    selection: span,
                    children: Vec::new(),
                });
            }
        }
        out.sort_by_key(|s| s.selection.start);
        out
    }

    /// Symbols whose names fuzzily match `query`.
    pub fn workspace_symbols(&self, query: &str) -> Vec<WorkspaceSymbol> {
        let mut out = Vec::new();
        for d in &self.sema.defs {
            if self.is_outline_symbol(d) && fuzzy_match(query, &d.name) {
                out.push(WorkspaceSymbol {
                    name: d.name.clone(),
                    kind: d.kind,
                    span: d.name_span.unwrap(),
                    container: self.container_name(d),
                });
            }
        }
        for m in &self.pp.macros {
            if let Some(span) = m.span {
                if fuzzy_match(query, &m.name) {
                    out.push(WorkspaceSymbol {
                        name: m.name.clone(),
                        kind: SymbolKind::Macro,
                        span,
                        container: None,
                    });
                }
            }
        }
        out
    }

    fn completion_for(&self, d: DefId) -> Completion {
        let def = self.sema.def(d);
        Completion {
            detail: Some(def.detail.clone()),
            documentation: self.def_doc(d),
            ..Completion::new(def.name.clone(), def.kind)
        }
    }

    /// `name(${1:param}, ...)` for a callable completed where a call is
    /// expected. `overloaded` callables get an empty argument list instead
    /// (signature help shows the overloads).
    fn call_snippet(&self, d: DefId, overloaded: bool) -> Option<String> {
        let def = self.sema.def(d);
        if !matches!(
            def.kind,
            SymbolKind::Action | SymbolKind::Function | SymbolKind::Method
        ) || def.name_span.is_none() && def.params.is_empty()
        {
            return None;
        }
        if overloaded {
            return Some(format!("{}($0)", def.name));
        }
        let params: Vec<String> = def
            .params
            .iter()
            .take(def.required_params)
            .enumerate()
            .map(|(i, &p)| format!("${{{}:{}}}", i + 1, self.sema.def(p).name))
            .collect();
        Some(format!("{}({})$0", def.name, params.join(", ")))
    }

    /// Whether the cursor is where a callable is named rather than called:
    /// a table's `actions` list, a `switch` label, or before an existing
    /// argument list.
    fn names_callable(&self, file: FileId, offset: u32) -> bool {
        let text = self.db.text(file);
        let o = text.floor_char_boundary(offset as usize);
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let after = text[o..].trim_start_matches(word).trim_start();
        if after.starts_with('(') || after.starts_with(':') {
            return true;
        }
        // The innermost unclosed `{`.
        let mut depth = 0u32;
        let mut open = None;
        for (i, c) in text[..o].char_indices().rev() {
            match c {
                '}' => depth += 1,
                '{' if depth == 0 => {
                    open = Some(i);
                    break;
                }
                '{' => depth -= 1,
                _ => {}
            }
        }
        let Some(open) = open else { return false };
        let before = text[..open].trim_end();
        if let Some(name) = before.strip_suffix('=') {
            return name.trim_end().ends_with("actions");
        }
        // `switch (...) {`
        if let Some(head) = before.strip_suffix(')') {
            let mut depth = 0u32;
            for (i, c) in head.char_indices().rev() {
                match c {
                    ')' => depth += 1,
                    '(' if depth == 0 => {
                        return head[..i].trim_end().ends_with("switch");
                    }
                    '(' => depth -= 1,
                    _ => {}
                }
            }
        }
        false
    }

    /// Add call snippets to completions of callables.
    fn add_call_snippets(
        &self,
        file: FileId,
        offset: u32,
        items: &mut [Completion],
        defs: &[DefId],
    ) {
        if self.names_callable(file, offset) {
            return;
        }
        for (item, &d) in items.iter_mut().zip(defs) {
            let overloaded = defs
                .iter()
                .filter(|&&o| self.sema.def(o).name == item.label)
                .count()
                > 1
                || self.sema.def(d).parent.is_some_and(|p| {
                    self.sema
                        .def(p)
                        .members
                        .iter()
                        .filter(|&&m| self.sema.def(m).name == item.label)
                        .count()
                        > 1
                });
            if let Some(sn) = self.call_snippet(d, overloaded) {
                item.insert = Some(sn);
                item.snippet = true;
            }
        }
    }

    /// Paths for an `#include` being typed at the cursor, from the include
    /// directories (and, for quoted includes, the file's own directory).
    fn include_completions(
        &self,
        file: FileId,
        offset: u32,
    ) -> Option<Vec<Completion>> {
        let text = self.db.text(file);
        let o = text.floor_char_boundary(offset as usize);
        let line_start = text[..o].rfind('\n').map_or(0, |i| i + 1);
        let line = &text[line_start..o];
        let rest = line.trim_start().strip_prefix('#')?.trim_start();
        let rest = rest.strip_prefix("include")?.trim_start();
        let (close, typed) = match rest.chars().next()? {
            '<' => ('>', &rest[1..]),
            '"' => ('"', &rest[1..]),
            _ => return None,
        };
        if typed.contains(close) {
            return None;
        }
        let (dir_part, name_part) = match typed.rfind('/') {
            Some(i) => (&typed[..=i], &typed[i + 1..]),
            None => ("", typed),
        };
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        if close == '"' {
            if let Some(d) = self.db.path(file).parent() {
                dirs.push(d.to_path_buf());
            }
        }
        dirs.extend(self.opts.include_dirs.iter().cloned());
        let line_end = text[o..].find('\n').map_or(text.len(), |i| o + i);
        let closed = text[o..line_end].contains(close);
        let replace = Span::new(file, (o - name_part.len()) as u32, o as u32);
        let this = self.db.path(file).to_path_buf();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for d in dirs {
            let Ok(entries) = std::fs::read_dir(d.join(dir_part)) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || e.path() == this {
                    continue;
                }
                let is_dir = e.path().is_dir();
                if !is_dir && !name.ends_with(".p4") {
                    continue;
                }
                let label = if is_dir { format!("{name}/") } else { name };
                if !seen.insert(label.clone()) {
                    continue;
                }
                let insert = if is_dir || closed {
                    label.clone()
                } else {
                    format!("{label}{close}")
                };
                out.push(Completion {
                    detail: Some(
                        d.join(dir_part).join(&label).display().to_string(),
                    ),
                    insert: Some(insert),
                    replace: Some(replace),
                    ..Completion::new(
                        label,
                        if is_dir {
                            SymbolKind::Folder
                        } else {
                            SymbolKind::File
                        },
                    )
                });
            }
        }
        Some(out)
    }

    /// The member access whose name is being typed at `offset`, if any:
    /// the cursor is in or right after a member name, or right after a `.`
    /// (the parser may have taken the next line's first word as the
    /// member).
    fn member_site_at(
        &self,
        file: FileId,
        offset: u32,
    ) -> Option<&sema::MemberSite> {
        let text = self.db.text(file);
        let bytes = text.as_bytes();
        let mut word = offset as usize;
        while word > 0
            && (bytes[word - 1].is_ascii_alphanumeric()
                || bytes[word - 1] == b'_')
        {
            word -= 1;
        }
        let mut dot = word;
        while dot > 0 && bytes[dot - 1].is_ascii_whitespace() {
            dot -= 1;
        }
        if dot == 0 || bytes[dot - 1] != b'.' {
            return None;
        }
        self.sema.members.iter().find(|m| {
            m.span.file == file
                && m.span.start as usize >= dot
                && text[dot..m.span.start as usize].trim().is_empty()
        })
    }

    pub fn completions(&self, file: FileId, offset: u32) -> Vec<Completion> {
        if let Some(items) = self.include_completions(file, offset) {
            return items;
        }
        if let Some(site) = self.member_site_at(file, offset) {
            let all: Vec<DefId> = self
                .sema
                .members_of(&site.base)
                .into_iter()
                .map(|(d, _)| d)
                .collect();
            let mut seen = std::collections::HashSet::new();
            let defs: Vec<DefId> = all
                .iter()
                .copied()
                .filter(|&d| seen.insert(self.sema.def(d).name.clone()))
                .collect();
            let mut items: Vec<Completion> =
                defs.iter().map(|&d| self.completion_for(d)).collect();
            self.add_call_snippets(file, offset, &mut items, &all);
            return items;
        }
        match self.completion_context(file, offset) {
            Context::Any => {}
            ctx => return self.context_completions(file, offset, ctx),
        }
        let mut seen = std::collections::HashSet::new();
        let visible = self.sema.visible_at(Span::new(file, offset, offset));
        let defs: Vec<DefId> = visible
            .iter()
            .copied()
            .filter(|&d| seen.insert(self.sema.def(d).name.clone()))
            .collect();
        let mut out: Vec<Completion> =
            defs.iter().map(|&d| self.completion_for(d)).collect();
        self.add_call_snippets(file, offset, &mut out, &defs);
        for m in &self.pp.macros {
            if seen.insert(m.name.clone()) {
                out.push(Completion {
                    detail: Some(m.body.clone()),
                    ..Completion::new(m.name.clone(), SymbolKind::Macro)
                });
            }
        }
        for &k in KEYWORDS {
            if seen.insert(k.to_string()) {
                out.push(Completion::new(k, SymbolKind::Keyword));
            }
        }
        for &(label, detail, body) in SNIPPETS {
            out.push(Completion {
                detail: Some(detail.into()),
                insert: Some(body.into()),
                snippet: true,
                ..Completion::new(label, SymbolKind::Snippet)
            });
        }
        out.extend(self.import_completions(file, offset, &seen));
        out
    }

    /// Where the cursor is, for completion that offers only what fits
    /// there. Works on the file's raw tokens, so it does not depend on the
    /// (possibly broken) code around the cursor parsing.
    fn completion_context(&self, file: FileId, offset: u32) -> Context {
        use crate::lexer::TokenKind as K;
        let text = self.db.text(file);
        let end = text.floor_char_boundary(offset as usize);
        let word = text[..end]
            .trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_')
            .len() as u32;
        let toks = crate::lexer::lex(file, text).tokens;
        let n = toks.partition_point(|t| t.span.start < word);
        // The token being completed, if a word has been typed.
        let current = ((word as usize) < end).then_some(n);
        let kind = |i: usize| toks[i].kind;
        let word_is =
            |i: usize, w: &str| toks[i].kind == K::Ident && toks[i].text == w;
        let Some(prev) = n.checked_sub(1) else {
            return Context::Any;
        };
        if word_is(prev, "transition") {
            return Context::State { select: true };
        }
        // The innermost unclosed `{` before the cursor.
        let mut depth = 0u32;
        let mut open = None;
        for i in (0..n).rev() {
            match kind(i) {
                K::RBrace => depth += 1,
                K::LBrace if depth == 0 => {
                    open = Some(i);
                    break;
                }
                K::LBrace => depth -= 1,
                _ => {}
            }
        }
        let Some(open) = open else {
            return Context::Any;
        };
        // Identifiers at the top level of the braces, other than the word
        // being completed, and whether each is followed by `=`.
        let names = || {
            let mut out = Vec::new();
            let mut depth = 0u32;
            for i in open + 1..toks.len() {
                match kind(i) {
                    K::LBrace | K::LParen => depth += 1,
                    K::RBrace | K::RParen if depth == 0 => break,
                    K::RBrace | K::RParen => depth -= 1,
                    K::Ident if depth == 0 && Some(i) != current => {
                        let eq =
                            toks.get(i + 1).is_some_and(|t| t.kind == K::Eq);
                        out.push((toks[i].text.clone(), eq));
                    }
                    _ => {}
                }
            }
            out
        };
        let item_start = matches!(kind(prev), K::LBrace | K::Semi | K::RBrace);
        if open >= 2 && kind(open - 1) == K::Eq {
            if word_is(open - 2, "key") && kind(prev) == K::Colon {
                return Context::MatchKind;
            }
            if word_is(open - 2, "actions") && item_start {
                return Context::TableActions(
                    names().into_iter().map(|(n, _)| n).collect(),
                );
            }
        }
        if open >= 2 && kind(open - 1) == K::Ident && word_is(open - 2, "table")
        {
            if kind(prev) == K::Eq
                && prev >= 1
                && word_is(prev - 1, "default_action")
            {
                // The actions listed in the table, wherever the list is.
                let mut listed = Vec::new();
                let mut depth = 0u32;
                let mut i = open + 1;
                while i < toks.len() {
                    match kind(i) {
                        K::LBrace
                            if depth == 0
                                && i >= 2
                                && kind(i - 1) == K::Eq
                                && word_is(i - 2, "actions") =>
                        {
                            i += 1;
                            while i < toks.len() && kind(i) != K::RBrace {
                                if kind(i) == K::Ident {
                                    listed.push(toks[i].text.clone());
                                }
                                i += 1;
                            }
                        }
                        K::LBrace => depth += 1,
                        K::RBrace if depth == 0 => break,
                        K::RBrace => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                return Context::DefaultAction(listed);
            }
            let after_const = word_is(prev, "const");
            if item_start || after_const {
                let present = names()
                    .into_iter()
                    .filter(|&(_, eq)| eq)
                    .map(|(n, _)| n)
                    .collect();
                return Context::TableProperty {
                    present,
                    after_const,
                };
            }
        }
        // `select (...) { ...: <cursor>`
        if open >= 1 && kind(open - 1) == K::RParen && kind(prev) == K::Colon {
            let mut depth = 0u32;
            for i in (0..open - 1).rev() {
                match kind(i) {
                    K::RParen => depth += 1,
                    K::LParen if depth == 0 => {
                        if i >= 1 && word_is(i - 1, "select") {
                            return Context::State { select: false };
                        }
                        break;
                    }
                    K::LParen => depth -= 1,
                    _ => {}
                }
            }
        }
        Context::Any
    }

    /// Completions for a [`Context`] other than [`Context::Any`].
    fn context_completions(
        &self,
        file: FileId,
        offset: u32,
        ctx: Context,
    ) -> Vec<Completion> {
        let mut defs = self.sema.visible_at(Span::new(file, offset, offset));
        defs.extend(self.sema.implicit_names());
        let mut seen = std::collections::HashSet::new();
        let mut of_kind =
            |kind: SymbolKind, skip: &[String]| -> Vec<Completion> {
                defs.iter()
                    .filter(|&&d| {
                        let def = self.sema.def(d);
                        def.kind == kind
                            && !skip.contains(&def.name)
                            && seen.insert(def.name.clone())
                    })
                    .map(|&d| self.completion_for(d))
                    .collect()
            };
        match ctx {
            Context::Any => Vec::new(),
            Context::MatchKind => of_kind(SymbolKind::MatchKind, &[]),
            Context::TableActions(listed) => {
                of_kind(SymbolKind::Action, &listed)
            }
            Context::DefaultAction(listed) => {
                let mut actions: Vec<DefId> = defs
                    .iter()
                    .copied()
                    .filter(|&d| {
                        let def = self.sema.def(d);
                        def.kind == SymbolKind::Action
                            && seen.insert(def.name.clone())
                    })
                    .collect();
                // Listed actions first; the others may be on their way into
                // the list (or get the quick fix that adds them).
                actions
                    .sort_by_key(|&d| !listed.contains(&self.sema.def(d).name));
                let mut out: Vec<Completion> =
                    actions.iter().map(|&d| self.completion_for(d)).collect();
                self.add_call_snippets(file, offset, &mut out, &actions);
                out
            }
            Context::State { select } => {
                let mut out = of_kind(SymbolKind::State, &[]);
                for name in ["accept", "reject"] {
                    if seen.insert(name.to_string()) {
                        out.push(Completion {
                            detail: Some(format!("state {name}")),
                            ..Completion::new(name, SymbolKind::State)
                        });
                    }
                }
                if select {
                    out.push(Completion {
                        detail: Some("transition select (...) { ... }".into()),
                        insert: Some(
                            "select (${1:key}) {\n\t${2:value}: ${3:state};\n\tdefault: ${4:accept};\n}"
                                .into(),
                        ),
                        snippet: true,
                        ..Completion::new("select", SymbolKind::Keyword)
                    });
                }
                out
            }
            Context::TableProperty {
                present,
                after_const,
            } => TABLE_PROPERTIES
                .iter()
                .filter(|(name, _, _)| !present.iter().any(|p| p == name))
                .filter(|(name, _, _)| {
                    !after_const
                        || matches!(*name, "entries" | "default_action")
                })
                .map(|&(name, detail, body)| Completion {
                    detail: Some(detail.into()),
                    insert: Some(body.into()),
                    snippet: true,
                    ..Completion::new(name, SymbolKind::Keyword)
                })
                .collect(),
        }
    }

    /// Directories holding the files the program includes, used to prefer
    /// headers for the same target when importing.
    pub fn include_dirs_used(&self) -> Vec<std::path::PathBuf> {
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        for i in &self.pp.includes {
            let Some(t) = i.target else { continue };
            if let Some(d) = self.db.path(t).parent() {
                if !dirs.iter().any(|x| x == d) {
                    dirs.push(d.to_path_buf());
                }
            }
        }
        dirs
    }

    /// Names from headers the program does not include, each completing
    /// with an `#include` for its header.
    fn import_completions(
        &self,
        file: FileId,
        offset: u32,
        seen: &std::collections::HashSet<String>,
    ) -> Vec<Completion> {
        let text = self.db.text(file);
        let end = text.floor_char_boundary(offset as usize);
        let start = text[..end]
            .trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_')
            .len();
        let prefix = &text[start..end];
        if prefix.is_empty() || prefix.starts_with(|c: char| c.is_ascii_digit())
        {
            return Vec::new();
        }
        let index =
            crate::headers::HeaderIndex::for_dirs(&self.opts.include_dirs);
        let mut out = Vec::new();
        for (name, cand) in index.complete(prefix, &self.include_dirs_used()) {
            if seen.contains(&name) {
                continue;
            }
            let Some(edit) =
                crate::headers::include_edit(self, file, &cand.include)
            else {
                continue;
            };
            if edit.0.file != file {
                continue;
            }
            out.push(Completion {
                detail: Some(format!("#include <{}>", cand.include)),
                documentation: Some(format!("```p4\n{}\n```", cand.detail)),
                additional_edits: vec![edit],
                ..Completion::new(name, cand.kind)
            });
        }
        out
    }

    /// Quick fixes and refactorings available for a range.
    pub fn code_actions(
        &self,
        file: FileId,
        start: u32,
        end: u32,
    ) -> Vec<CodeAction> {
        crate::assists::code_actions(self, file, start, end)
    }

    fn signature(&self, d: DefId) -> Signature {
        let def = self.sema.def(d);
        let prefix = def.detail.split('(').next().unwrap_or(&def.detail);
        let mut label = format!("{prefix}(");
        let mut params = Vec::new();
        for (i, &p) in def.params.iter().enumerate() {
            if i > 0 {
                label.push_str(", ");
            }
            let start = label.len() as u32;
            label.push_str(&self.sema.def(p).detail);
            params.push((start, label.len() as u32));
        }
        label.push(')');
        Signature {
            label,
            params,
            documentation: self.def_doc(d),
        }
    }

    pub fn signature_help(
        &self,
        file: FileId,
        offset: u32,
    ) -> Option<SignatureHelp> {
        let site = self
            .sema
            .calls
            .iter()
            .filter(|c| {
                c.span.file == file
                    && c.span.start < offset
                    && offset <= c.span.end
                    && !c.candidates.is_empty()
            })
            .min_by_key(|c| c.span.len())?;
        // Count top-level commas between the opening paren and the cursor.
        let text = self.db.text(file);
        let mut end = offset as usize;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let text = &text[site.span.start as usize..end];
        let mut depth = 0i32;
        let mut active = 0u32;
        let mut opened = false;
        for c in text.chars() {
            match c {
                '(' | '[' | '{' => {
                    depth += 1;
                    opened = true;
                }
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 1 && opened => active += 1,
                _ => {}
            }
        }
        if !opened || depth < 1 {
            return None;
        }
        let signatures: Vec<Signature> =
            site.candidates.iter().map(|&d| self.signature(d)).collect();
        let active_signature = signatures
            .iter()
            .position(|s| s.params.len() as u32 > active)
            .unwrap_or(0) as u32;
        Some(SignatureHelp {
            signatures,
            active_signature,
            active_param: active,
        })
    }

    /// The span of the renameable name under the cursor.
    pub fn prepare_rename(&self, file: FileId, offset: u32) -> Option<Span> {
        let (span, Target::Def(d)) = self.target_at(file, offset)? else {
            return None;
        };
        let def = self.sema.def(d);
        (def.name_span.is_some() && self.db.slice(span) == def.name)
            .then_some(span)
    }

    /// Edits renaming the symbol under the cursor, or an error message.
    pub fn rename(
        &self,
        file: FileId,
        offset: u32,
        new_name: &str,
    ) -> Result<Vec<(Span, String)>, String> {
        let valid = new_name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && new_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid || KEYWORDS.contains(&new_name) {
            return Err(format!("`{new_name}` is not a valid P4 identifier"));
        }
        let Some((_, Target::Def(d))) = self.target_at(file, offset) else {
            return Err("no renameable symbol at the cursor".into());
        };
        let def = self.sema.def(d);
        if def.name_span.is_none() {
            return Err(format!("`{}` is built in", def.name));
        }
        if self
            .sema
            .refs
            .iter()
            .any(|r| r.def == d && self.db.slice(r.span) != def.name)
        {
            return Err(format!(
                "`{}` is used through a macro expansion and cannot be renamed safely",
                def.name
            ));
        }
        Ok(self
            .occurrences(Target::Def(d))
            .into_iter()
            .map(|(s, _)| (s, new_name.to_string()))
            .collect())
    }

    /// Parameter names before call arguments, and labels after the closing
    /// `}` of long declarations and after long conditionals' `#else`,
    /// `#elif`, and `#endif`.
    pub fn inlay_hints(
        &self,
        file: FileId,
        start: u32,
        end: u32,
    ) -> Vec<InlayHint> {
        let lines = &self.db.file(file).lines;
        let text = self.db.text(file);
        let long = |a: Span, b: Span| {
            lines.line_of(b.end) >= lines.line_of(a.start) + CLOSING_HINT_LINES
        };
        let in_range = |o: u32| start <= o && o <= end;
        let mut out = Vec::new();
        for site in &self.sema.calls {
            if site.span.file == file && in_range(site.span.start) {
                self.param_hints(site, &mut out);
            }
        }
        for d in &self.sema.defs {
            let Some(span) = d.span else { continue };
            let block = matches!(
                d.kind,
                SymbolKind::Control
                    | SymbolKind::Parser
                    | SymbolKind::State
                    | SymbolKind::Action
                    | SymbolKind::Table
                    | SymbolKind::Function
                    | SymbolKind::Extern
                    | SymbolKind::Struct
                    | SymbolKind::Header
                    | SymbolKind::HeaderUnion
                    | SymbolKind::Enum
            );
            if block
                && span.file == file
                && in_range(span.end)
                && d.name_span.is_some_and(|n| n.file == file)
                && self.db.slice(span).ends_with('}')
                && long(span, span)
                && !comment_follows(text, span.end)
            {
                let kw = d.detail.split_whitespace().next().unwrap_or("");
                let kw = match d.kind {
                    SymbolKind::State => "state",
                    SymbolKind::Table => "table",
                    SymbolKind::Action => "action",
                    _ => kw,
                };
                out.push(InlayHint {
                    file,
                    offset: span.end,
                    label: format!("{kw} {}", d.name),
                    kind: InlayHintKind::Closing,
                });
            }
        }
        for &(close, open) in &self.pp.conditionals {
            if close.file == file
                && in_range(close.end)
                && long(open, close)
                && !comment_follows(text, close.end)
            {
                out.push(InlayHint {
                    file,
                    offset: close.end,
                    label: self.db.slice(open).split_whitespace().fold(
                        String::new(),
                        |mut l, w| {
                            if !l.is_empty() {
                                l.push(' ');
                            }
                            l.push_str(w);
                            l
                        },
                    ),
                    kind: InlayHintKind::Closing,
                });
            }
        }
        // Fragments included several times record the same sites again.
        out.sort_by(|a, b| (a.offset, &a.label).cmp(&(b.offset, &b.label)));
        out.dedup();
        out
    }

    /// `name:` hints for a call's positional arguments, when the parameter
    /// is known and the argument does not already say it.
    fn param_hints(&self, site: &sema::CallSite, out: &mut Vec<InlayHint>) {
        let n = site.arg_spans.len();
        // Arguments from a macro expansion share the invocation's span.
        if site
            .arg_spans
            .iter()
            .any(|a| a.file != site.span.file || a.start < site.span.start)
            || site.arg_spans.windows(2).any(|w| w[1].start < w[0].end)
        {
            return;
        }
        let fits: Vec<&sema::Def> = site
            .candidates
            .iter()
            .map(|&c| self.sema.def(c))
            .filter(|d| d.required_params <= n && n <= d.params.len())
            .collect();
        let Some(first) = fits.first() else { return };
        let callee = first.name.to_lowercase();
        for (i, &arg) in site.arg_spans.iter().enumerate() {
            let name = &self.sema.def(first.params[i]).name;
            if fits
                .iter()
                .any(|d| &self.sema.def(d.params[i]).name != name)
            {
                continue;
            }
            let text = self.db.slice(arg).trim();
            if is_named_arg(text) || redundant_hint(name, text, &callee, n) {
                continue;
            }
            out.push(InlayHint {
                file: arg.file,
                offset: arg.start,
                label: format!("{name}:"),
                kind: InlayHintKind::Parameter,
            });
        }
    }
}

/// The best candidate containing the cursor: one the cursor is inside
/// beats one it is just past, then the shortest wins.
fn best_at(
    candidates: impl Iterator<Item = (Span, Target)>,
    file: FileId,
    offset: u32,
) -> Option<(Span, Target)> {
    candidates
        .filter(|(s, _)| s.contains(file, offset))
        .max_by_key(|(s, _)| (offset < s.end, std::cmp::Reverse(s.len())))
}

/// `0x…` and, for values up to 64 bits, `0b…` renderings of an integer,
/// zero-padded to `width` bits when known.
fn radixes(v: u128, width: Option<u32>) -> String {
    let hex_digits = width.map_or(0, |w| w.div_ceil(4) as usize);
    let mut out = format!("0x{v:0hex_digits$x}");
    if v <= u64::MAX as u128 {
        let bin_digits = width.unwrap_or(0) as usize;
        let bin = format!("{v:0bin_digits$b}");
        // Group by nibbles from the right.
        let mut grouped = String::new();
        for (i, c) in bin.chars().enumerate() {
            if i > 0 && (bin.len() - i) % 4 == 0 {
                grouped.push('_');
            }
            grouped.push(c);
        }
        out.push_str(&format!(" · 0b{grouped}"));
    }
    out
}

/// Whether an argument is written `name = value`.
fn is_named_arg(text: &str) -> bool {
    let rest =
        text.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_');
    rest.len() < text.len()
        && rest
            .trim_start()
            .strip_prefix('=')
            .is_some_and(|r| !r.starts_with('='))
}

/// Whether a parameter name hint would repeat what the argument or callee
/// already says: `port` for `port`, `meta.egress_port`, or `Port()`; `hdr` for
/// `hdr.ipv4`; `type` for `CounterType_t.PACKETS`; or the sole argument of
/// `set_port(...)`.
fn redundant_hint(param: &str, arg: &str, callee: &str, nargs: usize) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let p = norm(param);
    let words: Vec<&str> =
        arg.trim_end_matches("()").split(['.', '[']).collect();
    let first = words[0].trim();
    let last = words[words.len() - 1];
    p.len() < 2
        || param.starts_with('_')
        || norm(last) == p
        || (p.len() >= 3 && {
            let l = norm(last);
            l.len() >= 3
                && (l.ends_with(&p) || l.starts_with(&p) || p.starts_with(&l))
        })
        || (words.len() > 1
            && norm(first.strip_suffix("_t").unwrap_or(first)).contains(&p))
        || (nargs == 1 && norm(callee).ends_with(&p))
}

/// Whether a comment follows `offset` on its line.
fn comment_follows(text: &str, offset: u32) -> bool {
    let rest = &text[offset as usize..];
    let line = &rest[..rest.find('\n').unwrap_or(rest.len())];
    line.contains("//") || line.contains("/*")
}

/// Strip comment delimiters and leading `*` decorations.
fn clean_comment(c: &str) -> String {
    let body = if let Some(b) = c.strip_prefix("/*") {
        b.strip_suffix("*/").unwrap_or(b)
    } else {
        c.trim_start_matches('/')
    };
    let lines: Vec<&str> = body
        .lines()
        .map(|l| {
            let l = l.trim();
            let l = l.strip_prefix('*').unwrap_or(l);
            l.strip_prefix(' ').unwrap_or(l).trim_end()
        })
        .collect();
    let start = lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(start, |e| e + 1);
    lines[start..end].join("\n")
}

/// Case-insensitive subsequence match.
fn fuzzy_match(query: &str, name: &str) -> bool {
    let mut chars = name.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|q| chars.any(|c| c == q))
}
