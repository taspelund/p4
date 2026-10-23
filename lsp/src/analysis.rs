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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlayHint {
    /// Offset the hint is displayed at.
    pub file: FileId,
    pub offset: u32,
    pub label: String,
}

/// What an identifier occurrence refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Def(DefId),
    Macro(usize),
    Include(usize),
}

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
    diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    pub fn new(mut db: SourceDb, root: FileId, opts: &Options) -> Self {
        let pp = preprocess::preprocess(&mut db, root, opts);
        let parse = syntax::parse(&pp.tokens);
        // A missing include makes every name it declares unresolved;
        // report the include alone rather than a cascade.
        let complete = pp.includes.iter().all(|i| i.target.is_some());
        let sema = sema::analyze(&parse.program, &db, complete);
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
        diagnostics.extend(sema.diagnostics.iter().cloned());
        for &span in &pp.inactive {
            let mut d = Diagnostic::new(
                span,
                Severity::Hint,
                "inactive preprocessor block",
            );
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
        if let Some(doc) = self.def_doc(d) {
            md.push_str("\n\n---\n\n");
            md.push_str(&doc);
        }
        md
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
        let (span, target) = self.target_at(file, offset)?;
        let markdown = match target {
            Target::Def(d) => self.def_markdown(d),
            Target::Macro(m) => {
                let mut md = format!("```c\n{}\n```", self.macro_signature(m));
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
            label: def.name.clone(),
            kind: def.kind,
            detail: Some(def.detail.clone()),
            documentation: self.def_doc(d),
        }
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
        if let Some(site) = self.member_site_at(file, offset) {
            let mut seen = std::collections::HashSet::new();
            return self
                .sema
                .members_of(&site.base)
                .into_iter()
                .filter(|(d, _)| seen.insert(self.sema.def(*d).name.clone()))
                .map(|(d, _)| self.completion_for(d))
                .collect();
        }
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<Completion> = self
            .sema
            .visible_at(Span::new(file, offset, offset))
            .into_iter()
            .filter(|&d| seen.insert(self.sema.def(d).name.clone()))
            .map(|d| self.completion_for(d))
            .collect();
        for m in &self.pp.macros {
            if seen.insert(m.name.clone()) {
                out.push(Completion {
                    label: m.name.clone(),
                    kind: SymbolKind::Macro,
                    detail: Some(m.body.clone()),
                    documentation: None,
                });
            }
        }
        for &k in KEYWORDS {
            if seen.insert(k.to_string()) {
                out.push(Completion {
                    label: k.to_string(),
                    kind: SymbolKind::Keyword,
                    detail: None,
                    documentation: None,
                });
            }
        }
        out
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

    pub fn inlay_hints(
        &self,
        _file: FileId,
        _start: u32,
        _end: u32,
    ) -> Vec<InlayHint> {
        Vec::new()
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
