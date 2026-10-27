// Copyright 2026 Oxide Computer Company

//! Code actions: quick fixes for diagnostics and code generators.

use crate::analysis::{Analysis, CodeAction, CodeActionKind, SymbolKind};
use crate::headers::{include_edit, HeaderIndex};
use crate::sema::{is_type_kind, DefId, Ty, UnresolvedKind};
use crate::source::{FileId, Span};
use crate::syntax::ast::*;

pub fn code_actions(
    a: &Analysis,
    file: FileId,
    start: u32,
    end: u32,
) -> Vec<CodeAction> {
    let cx = Cx {
        a,
        file,
        start,
        end,
    };
    let mut out = Vec::new();
    cx.unresolved_fixes(&mut out);
    cx.cast_fix(&mut out);
    cx.missing_semicolon(&mut out);
    cx.not_in_actions(&mut out);
    cx.remove_unused(&mut out);
    cx.fill_switch(&mut out);
    cx.generate_emits(&mut out);
    cx.generate_parse_states(&mut out);
    cx.literal_to_const(&mut out);
    cx.extract_constant(&mut out);
    cx.sort_table_actions(&mut out);
    cx.inline_macro(&mut out);
    out
}

/// The packet parameter's name, the headers parameter's name, and the
/// fields to emit (name, whether it is a plain header rather than a stack,
/// union or struct).
type PacketAndHeaders = (String, String, Vec<(String, bool)>);

struct Cx<'a> {
    a: &'a Analysis,
    file: FileId,
    start: u32,
    end: u32,
}

impl<'a> Cx<'a> {
    /// Cast a value of the wrong width or signedness to the expected type.
    fn cast_fix(&self, out: &mut Vec<CodeAction>) {
        for m in &self.a.sema.mismatches {
            if m.span.file != self.file
                || !(m.span.start <= self.end && self.start <= m.span.end)
                || out.iter().any(|a| a.fixes == Some(m.span))
            {
                continue;
            }
            let at = |o: u32| Span::new(m.span.file, o, o);
            let mut edits =
                vec![(at(m.span.start), format!("({})", m.expected))];
            if m.compound {
                edits[0].1.push('(');
                edits.push((at(m.span.end), ")".into()));
            }
            out.push(CodeAction {
                title: format!("Cast to `{}`", m.expected),
                kind: CodeActionKind::QuickFix,
                edits,
                fixes: Some(m.span),
                preferred: true,
            });
        }
    }

    fn text(&self, file: FileId) -> &str {
        self.a.db.text(file)
    }

    /// Whether a diagnostic span is under the requested range. A zero-width
    /// span (such as a missing `;` after a token) also matches a range on
    /// the same line, since the cursor is rarely exactly on it.
    fn touches(&self, span: Span) -> bool {
        if span.file != self.file {
            return false;
        }
        if span.start <= self.end && self.start <= span.end {
            return true;
        }
        let text = self.text(self.file);
        span.is_empty()
            && line_start(text, span.start) == line_start(text, self.start)
    }

    /// Whether the range starts inside `span`.
    fn inside(&self, span: Span) -> bool {
        span.file == self.file
            && span.start <= self.start
            && self.start <= span.end
    }

    fn insert(&self, file: FileId, at: u32, text: String) -> (Span, String) {
        (Span::new(file, at, at), text)
    }

    // -------------------------------------------------------------------
    // Quick fixes

    fn unresolved_fixes(&self, out: &mut Vec<CodeAction>) {
        let sema = &self.a.sema;
        for u in &sema.unresolved {
            if !self.touches(u.span) {
                continue;
            }
            // Did you mean ...?
            let candidates: Vec<String> = match &u.kind {
                UnresolvedKind::Member(ty) => sema
                    .members_of(ty)
                    .into_iter()
                    .map(|(d, _)| sema.def(d).name.clone())
                    .collect(),
                kind => sema
                    .visible_from(u.scope)
                    .into_iter()
                    .chain(sema.implicit_names())
                    .filter(|&d| {
                        let k = sema.def(d).kind;
                        match kind {
                            UnresolvedKind::Type => is_type_kind(k),
                            UnresolvedKind::State => k == SymbolKind::State,
                            UnresolvedKind::MatchKind => {
                                k == SymbolKind::MatchKind
                            }
                            UnresolvedKind::Action { .. } => {
                                k == SymbolKind::Action
                            }
                            _ => !is_type_kind(k),
                        }
                    })
                    .map(|d| sema.def(d).name.clone())
                    .chain(
                        matches!(kind, UnresolvedKind::State)
                            .then(|| ["accept".into(), "reject".into()])
                            .into_iter()
                            .flatten(),
                    )
                    .collect(),
            };
            for (i, name) in
                similar(&u.name, candidates).into_iter().enumerate()
            {
                out.push(CodeAction {
                    title: format!("Change to `{name}`"),
                    kind: CodeActionKind::QuickFix,
                    edits: vec![(u.span, name)],
                    fixes: Some(u.span),
                    preferred: i == 0,
                });
            }
            match &u.kind {
                UnresolvedKind::Action { table, params } => {
                    if let Some(a) = self.create_action(&u.name, *table, params)
                    {
                        out.push(CodeAction {
                            fixes: Some(u.span),
                            ..a
                        });
                    }
                }
                UnresolvedKind::State => {
                    if let Some(a) =
                        u.parser.and_then(|p| self.create_state(&u.name, p))
                    {
                        out.push(CodeAction {
                            fixes: Some(u.span),
                            ..a
                        });
                    }
                }
                UnresolvedKind::Member(ty) => {
                    if let Some(a) = self.add_field(&u.name, u.span, ty) {
                        out.push(CodeAction {
                            fixes: Some(u.span),
                            ..a
                        });
                    }
                }
                UnresolvedKind::Name | UnresolvedKind::Type => {
                    let index =
                        HeaderIndex::for_dirs(&self.a.opts.include_dirs);
                    let near = self.a.include_dirs_used();
                    for c in
                        index.lookup_near(&u.name, &near).into_iter().take(2)
                    {
                        if let Some(edit) =
                            include_edit(self.a, self.file, &c.include)
                        {
                            out.push(CodeAction {
                                title: format!(
                                    "Add `#include <{}>`",
                                    c.include
                                ),
                                kind: CodeActionKind::QuickFix,
                                edits: vec![edit],
                                fixes: Some(u.span),
                                preferred: false,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether declarations in `file` may be edited: not when it is in an
    /// include directory, such as an architecture header, unless the
    /// program's root is in the same directory.
    fn editable(&self, file: FileId) -> bool {
        let path = self.a.db.path(file);
        let root = self.a.db.path(self.a.root);
        !self
            .a
            .opts
            .include_dirs
            .iter()
            .any(|d| path.starts_with(d) && !root.starts_with(d))
    }

    /// The type of an expression, as P4 source, when it is evident: a
    /// sized literal, a bool, a cast, or a name or member with a declared
    /// type.
    fn type_of(&self, e: &Expr) -> Option<String> {
        let sema = &self.a.sema;
        let name = match &e.kind {
            ExprKind::Int(IntLit {
                width: Some(w),
                signed,
                ..
            }) => {
                return Some(format!(
                    "{}<{w}>",
                    if *signed { "int" } else { "bit" }
                ))
            }
            ExprKind::Bool(_) => return Some("bool".into()),
            ExprKind::Cast { ty, .. } => {
                return Some(self.a.db.slice(ty.span).to_string())
            }
            ExprKind::Member { member, .. } => member.span,
            ExprKind::Path(p) => p.name.span,
            _ => return None,
        };
        let r = sema.refs.iter().find(|r| r.span == name)?;
        sema.type_text.get(&r.def).cloned()
    }

    /// The other side of the assignment or binary operation whose one side
    /// is the expression at `span`.
    fn counterpart(&self, span: Span) -> Option<&'a Expr> {
        let mut found = None;
        let mut pair = |a: &'a Expr, b: &'a Expr| {
            if a.span == span {
                found = Some(b);
            } else if b.span == span {
                found = Some(a);
            }
        };
        each_stmt(&self.a.parse.program.decls, &mut |s| {
            if let StmtKind::Assign { lhs, rhs, .. } = &s.kind {
                pair(lhs, rhs);
            }
        });
        each_expr(&self.a.parse.program.decls, &mut |e| {
            if let ExprKind::Binary { lhs, rhs, .. } = &e.kind {
                pair(lhs, rhs);
            }
        });
        found
    }

    /// Add a missing field to the struct or header it was looked up in,
    /// typed after the other side of its assignment or comparison.
    fn add_field(&self, name: &str, at: Span, ty: &Ty) -> Option<CodeAction> {
        let sema = &self.a.sema;
        let d = sema.type_def(&sema.resolve_alias(ty))?;
        let def = sema.def(d);
        if !matches!(
            def.kind,
            SymbolKind::Struct | SymbolKind::Header | SymbolKind::HeaderUnion
        ) || name.is_empty()
        {
            return None;
        }
        let span = def.span?;
        if !self.editable(span.file) {
            return None;
        }
        // The member expression whose name is `at`.
        let mut access = None;
        each_expr(&self.a.parse.program.decls, &mut |e| {
            if let ExprKind::Member { member, .. } = &e.kind {
                if member.span == at {
                    access = Some(e.span);
                }
            }
        });
        let field_ty = access
            .and_then(|s| self.counterpart(s))
            .and_then(|e| self.type_of(e))
            .unwrap_or_else(|| "bit<8>".into());
        let text = self.a.db.text(span.file);
        let u = unit(text);
        let (pos, indent, lead) = before_close(text, span, &u)?;
        let insert = format!(
            "{lead}{indent}{field_ty} {name};\n{}",
            close_indent(&indent, &u, &lead)
        );
        Some(CodeAction {
            title: format!("Add field `{field_ty} {name}` to `{}`", def.name),
            kind: CodeActionKind::QuickFix,
            edits: vec![self.insert(span.file, pos, insert)],
            fixes: None,
            preferred: false,
        })
    }

    /// An empty action inserted before the table that names it.
    fn create_action(
        &self,
        name: &str,
        table: Span,
        params: &[(String, String)],
    ) -> Option<CodeAction> {
        let def = self.a.sema.defs.iter().find(|d| {
            d.kind == SymbolKind::Table && d.name_span == Some(table)
        })?;
        let span = def.span.filter(|s| s.file == table.file)?;
        let text = self.text(span.file);
        let at = line_start(text, span.start);
        let indent = indent_at(text, at);
        let mut names = Vec::new();
        let params: Vec<String> = params
            .iter()
            .map(|(ty, n)| {
                let mut n = n.clone();
                while names.contains(&n) {
                    n.push('_');
                }
                names.push(n.clone());
                format!("{ty} {n}")
            })
            .collect();
        Some(CodeAction {
            title: format!("Create action `{name}`"),
            kind: CodeActionKind::QuickFix,
            edits: vec![self.insert(
                span.file,
                at,
                format!(
                    "{indent}action {name}({}) {{\n{indent}}}\n\n",
                    params.join(", ")
                ),
            )],
            fixes: None,
            preferred: false,
        })
    }

    /// A state that accepts, appended to the parser.
    fn create_state(&self, name: &str, parser: DefId) -> Option<CodeAction> {
        let span = self.a.sema.def(parser).span?;
        let text = self.text(span.file);
        let (at, indent, lead) = before_close(text, span, &unit(text))?;
        let u = unit(text);
        Some(CodeAction {
            title: format!("Create state `{name}`"),
            kind: CodeActionKind::QuickFix,
            edits: vec![self.insert(
                span.file,
                at,
                format!(
                    "{lead}\n{indent}state {name} {{\n{indent}{u}transition accept;\n{indent}}}\n{}",
                    close_indent(&indent, &u, &lead),
                ),
            )],
            fixes: None,
            preferred: false,
        })
    }

    fn missing_semicolon(&self, out: &mut Vec<CodeAction>) {
        for d in &self.a.parse.diagnostics {
            if d.span.is_empty()
                && d.message.starts_with("expected `;`")
                && self.touches(d.span)
            {
                out.push(CodeAction {
                    title: "Insert `;`".into(),
                    kind: CodeActionKind::QuickFix,
                    edits: vec![(d.span, ";".into())],
                    fixes: Some(d.span),
                    preferred: true,
                });
            }
        }
    }

    fn not_in_actions(&self, out: &mut Vec<CodeAction>) {
        for n in &self.a.sema.not_in_actions {
            if !self.touches(n.span) {
                continue;
            }
            let text = self.text(n.list_end.file);
            let close = n.list_end.start;
            let ls = line_start(text, close);
            let edit = if text[ls as usize..close as usize].trim().is_empty() {
                let indent = format!("{}{}", indent_at(text, ls), unit(text));
                self.insert(
                    n.list_end.file,
                    ls,
                    format!("{indent}{};\n", n.name),
                )
            } else {
                let before = &text[..close as usize];
                let sep = if before.ends_with(' ') { "" } else { " " };
                self.insert(
                    n.list_end.file,
                    close,
                    format!("{sep}{}; ", n.name),
                )
            };
            out.push(CodeAction {
                title: format!("Add `{}` to the table's actions", n.name),
                kind: CodeActionKind::QuickFix,
                edits: vec![edit],
                fixes: Some(n.span),
                preferred: true,
            });
        }
    }

    fn remove_unused(&self, out: &mut Vec<CodeAction>) {
        for &d in &self.a.sema.unused {
            let def = self.a.sema.def(d);
            let (Some(name), Some(span)) = (def.name_span, def.span) else {
                continue;
            };
            if !self.touches(name) || span.file != name.file {
                continue;
            }
            let text = self.text(span.file);
            let decl = &text[span.start as usize..span.end as usize];
            // Removing an initializer could remove a side effect.
            if def.kind == SymbolKind::Variable
                && decl
                    .split_once('=')
                    .is_some_and(|(_, init)| init.contains('('))
            {
                continue;
            }
            out.push(CodeAction {
                title: format!("Remove unused `{}`", def.name),
                kind: CodeActionKind::QuickFix,
                edits: vec![(whole_lines(text, span), String::new())],
                fixes: Some(name),
                preferred: false,
            });
        }
    }

    // -------------------------------------------------------------------
    // Generators and rewrites

    /// Add a case for each of the table's actions missing from a
    /// `switch (t.apply().action_run)`.
    fn fill_switch(&self, out: &mut Vec<CodeAction>) {
        let mut best: Option<(&'a Stmt, &'a Expr, &'a [SwitchCase])> = None;
        each_stmt(&self.a.parse.program.decls, &mut |s| {
            if let StmtKind::Switch { expr, cases } = &s.kind {
                if self.inside(s.span)
                    && best.is_none_or(|(b, _, _)| s.span.len() < b.span.len())
                {
                    best = Some((s, expr, cases));
                }
            }
        });
        let Some((stmt, expr, cases)) = best else {
            return;
        };
        // `<table>.apply().action_run`
        let ExprKind::Member { base, member } = &expr.kind else {
            return;
        };
        if member.text != "action_run" {
            return;
        }
        let ExprKind::Call { callee, .. } = &base.kind else {
            return;
        };
        let ExprKind::Member { base: table, .. } = &callee.kind else {
            return;
        };
        let ExprKind::Path(path) = &table.kind else {
            return;
        };
        let sema = &self.a.sema;
        let Some(table) = sema
            .refs
            .iter()
            .find(|r| r.span == path.name.span)
            .map(|r| r.def)
        else {
            return;
        };
        let Some(actions) = sema.table_actions.get(&table) else {
            return;
        };
        let labeled: Vec<&str> = cases
            .iter()
            .filter_map(|c| match &c.label {
                SwitchLabel::Expr(Expr {
                    kind: ExprKind::Path(p),
                    ..
                }) => Some(p.name.text.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<&str> = actions
            .iter()
            .map(|&d| sema.def(d).name.as_str())
            .filter(|n| !labeled.contains(n))
            .collect();
        if missing.is_empty() {
            return;
        }
        let text = self.text(stmt.span.file);
        let u = unit(text);
        let Some((at, indent, lead)) = before_close(text, stmt.span, &u) else {
            return;
        };
        let indent = match cases.first() {
            Some(c) if c.span.file == stmt.span.file => {
                indent_at(text, line_start(text, c.span.start))
            }
            _ => indent,
        };
        let mut body = String::from(&lead);
        for name in &missing {
            body.push_str(&format!("{indent}{name}: {{ }}\n"));
        }
        body.push_str(&close_indent(&indent, &u, &lead));
        out.push(CodeAction {
            title: format!(
                "Add {} missing `action_run` case(s)",
                missing.len()
            ),
            kind: CodeActionKind::Generate,
            edits: vec![self.insert(stmt.span.file, at, body)],
            fixes: None,
            preferred: false,
        });
    }

    /// The parameter of a block whose type is the extern `extern_name`, and
    /// the parameter whose type is a struct containing headers.
    fn packet_and_headers(
        &self,
        block: DefId,
        extern_name: &str,
    ) -> Option<PacketAndHeaders> {
        let sema = &self.a.sema;
        let params = &sema.def(block).params;
        let packet = params.iter().find(|&&p| {
            sema.type_def(&sema.def(p).ty)
                .is_some_and(|t| sema.def(t).name == extern_name)
        })?;
        for &p in params {
            let Some(t) = sema.type_def(&sema.def(p).ty) else {
                continue;
            };
            if sema.def(t).kind != SymbolKind::Struct {
                continue;
            }
            // Fields that hold headers, and whether each is a plain header.
            let fields: Vec<(String, bool)> = sema
                .def(t)
                .members
                .iter()
                .filter_map(|&f| {
                    let ty = sema.resolve_alias(&sema.def(f).ty);
                    let kind = sema.type_def(&ty).map(|d| sema.def(d).kind);
                    let header = kind == Some(SymbolKind::Header)
                        && !matches!(ty, Ty::Stack(..));
                    let emits = matches!(
                        kind,
                        Some(
                            SymbolKind::Header
                                | SymbolKind::HeaderUnion
                                | SymbolKind::Struct
                        )
                    );
                    emits.then(|| (sema.def(f).name.clone(), header))
                })
                .collect();
            if !fields.is_empty() {
                return Some((
                    sema.def(*packet).name.clone(),
                    sema.def(p).name.clone(),
                    fields,
                ));
            }
        }
        None
    }

    /// The parser or control declaration (and its def) containing the
    /// cursor.
    fn enclosing(&self, want_parser: bool) -> Option<(&'a Decl, DefId)> {
        let decl = self.a.parse.program.decls.iter().find(|d| {
            matches!(
                (d, want_parser),
                (Decl::Parser(_), true) | (Decl::Control(_), false)
            ) && self.inside(d.span())
        })?;
        let name = decl.name()?;
        let id = self
            .a
            .sema
            .defs
            .iter()
            .position(|d| d.name_span == Some(name.span))?;
        Some((decl, DefId(id as u32)))
    }

    /// `emit` every header of the headers parameter of a deparser control.
    fn generate_emits(&self, out: &mut Vec<CodeAction>) {
        let Some((Decl::Control(c), id)) = self.enclosing(false) else {
            return;
        };
        let Some(apply) = &c.apply else { return };
        let Some((pkt, hdr, fields)) =
            self.packet_and_headers(id, "packet_out")
        else {
            return;
        };
        let text = self.text(apply.span.file);
        let existing: String = text
            [apply.span.start as usize..apply.span.end as usize]
            .split_whitespace()
            .collect();
        let missing: Vec<&str> = fields
            .iter()
            .map(|(f, _)| f.as_str())
            .filter(|f| !existing.contains(&format!("emit({hdr}.{f})")))
            .collect();
        if missing.is_empty() {
            return;
        }
        let u = unit(text);
        let Some((at, indent, lead)) = before_close(text, apply.span, &u)
        else {
            return;
        };
        let mut body = lead.clone();
        for f in &missing {
            body.push_str(&format!("{indent}{pkt}.emit({hdr}.{f});\n"));
        }
        body.push_str(&close_indent(&indent, &u, &lead));
        out.push(CodeAction {
            title: format!("Generate `emit` calls for `{hdr}`"),
            kind: CodeActionKind::Generate,
            edits: vec![self.insert(apply.span.file, at, body)],
            fixes: None,
            preferred: false,
        });
    }

    /// A state extracting each header no state extracts yet.
    fn generate_parse_states(&self, out: &mut Vec<CodeAction>) {
        let Some((Decl::Parser(p), id)) = self.enclosing(true) else {
            return;
        };
        let Some((pkt, hdr, fields)) = self.packet_and_headers(id, "packet_in")
        else {
            return;
        };
        let text = self.text(p.span.file);
        let existing: String = text[p.span.start as usize..p.span.end as usize]
            .split_whitespace()
            .collect();
        let missing: Vec<&str> = fields
            .iter()
            .filter(|(_, header)| *header)
            .map(|(f, _)| f.as_str())
            .filter(|f| !existing.contains(&format!("extract({hdr}.{f})")))
            .collect();
        if missing.is_empty() {
            return;
        }
        let u = unit(text);
        let Some((at, indent, lead)) = before_close(text, p.span, &u) else {
            return;
        };
        let states: Vec<&str> =
            p.states.iter().map(|s| s.name.text.as_str()).collect();
        let mut body = lead.clone();
        for f in &missing {
            let mut name = format!("parse_{f}");
            while states.contains(&name.as_str()) {
                name.push('_');
            }
            body.push_str(&format!(
                "\n{indent}state {name} {{\n{indent}{u}{pkt}.extract({hdr}.{f});\n{indent}{u}transition accept;\n{indent}}}\n"
            ));
        }
        body.push_str(&close_indent(&indent, &u, &lead));
        out.push(CodeAction {
            title: format!(
                "Generate parser states for {} unextracted header(s)",
                missing.len()
            ),
            kind: CodeActionKind::Generate,
            edits: vec![self.insert(p.span.file, at, body)],
            fixes: None,
            preferred: false,
        });
    }

    /// Replace an integer literal with a constant of the same value.
    fn literal_to_const(&self, out: &mut Vec<CodeAction>) {
        let mut lit: Option<(Span, &'a IntLit)> = None;
        each_expr(&self.a.parse.program.decls, &mut |e| {
            if let ExprKind::Int(i) = &e.kind {
                if self.inside(e.span) && self.a.db.slice(e.span) == i.text {
                    lit = Some((e.span, i));
                }
            }
        });
        let Some((span, lit)) = lit else { return };
        let Some(value) = lit.value else { return };
        let sema = &self.a.sema;
        let mut n = 0;
        for d in sema.visible_at(span) {
            let def = sema.def(d);
            if def.kind != SymbolKind::Constant
                || sema.const_values.get(&d) != Some(&value)
                || def.span.is_some_and(|s| {
                    s.file == span.file
                        && s.start <= span.start
                        && span.end <= s.end
                })
            {
                continue;
            }
            if let Some(w) = lit.width {
                let ty = sema.type_text.get(&d).map(String::as_str);
                let want =
                    format!("{}<{w}>", if lit.signed { "int" } else { "bit" });
                if ty != Some(want.as_str()) {
                    continue;
                }
            }
            out.push(CodeAction {
                title: format!("Replace with `{}`", def.name),
                kind: CodeActionKind::Rewrite,
                edits: vec![(span, def.name.clone())],
                fixes: None,
                preferred: false,
            });
            n += 1;
            if n == 3 {
                break;
            }
        }
    }

    /// Move an integer literal into a new constant declared before the
    /// declaration containing it (at the top level, or among a control's
    /// or parser's locals). The name comes from what the literal is
    /// assigned to or compared with, e.g. `ETHER_TYPE_0800` for
    /// `hdr.eth.ether_type == 0x0800`; rename it afterwards.
    fn extract_constant(&self, out: &mut Vec<CodeAction>) {
        let mut lit: Option<(Span, &'a IntLit)> = None;
        each_expr(&self.a.parse.program.decls, &mut |e| {
            if let ExprKind::Int(i) = &e.kind {
                if self.inside(e.span) && self.a.db.slice(e.span) == i.text {
                    lit = Some((e.span, i));
                }
            }
        });
        let Some((span, lit)) = lit else { return };
        let contains = |s: Span| {
            s.file == span.file && s.start <= span.start && span.end <= s.end
        };
        // The innermost declaration in a declaration list containing it.
        let mut target = None;
        for d in &self.a.parse.program.decls {
            if contains(d.span()) {
                target = Some(d.span());
            }
            let locals = match d {
                Decl::Control(c) => &c.locals,
                Decl::Parser(p) => &p.locals,
                _ => continue,
            };
            for l in locals {
                if contains(l.span()) {
                    target = Some(l.span());
                }
            }
        }
        let Some(target) = target else { return };
        // A constant declaration's own initializer is already a constant.
        if self
            .a
            .sema
            .defs
            .iter()
            .any(|d| d.kind == SymbolKind::Constant && d.span == Some(target))
        {
            return;
        }
        let other = self.counterpart(span);
        let ty = self
            .type_of(&Expr {
                kind: ExprKind::Int(lit.clone()),
                span,
            })
            .or_else(|| other.and_then(|e| self.type_of(e)))
            .unwrap_or_else(|| "int".into());
        let base = other
            .and_then(|e| match &e.kind {
                ExprKind::Member { member, .. } => Some(member.text.clone()),
                ExprKind::Path(p) => Some(p.name.text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "VALUE".into());
        let digits = lit.text.rsplit(['w', 's']).next().unwrap_or(&lit.text);
        let digits = digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
            .unwrap_or(digits)
            .replace('_', "");
        let mut name = format!("{base}_{digits}").to_uppercase();
        let taken: Vec<String> = self
            .a
            .sema
            .visible_at(span)
            .into_iter()
            .map(|d| self.a.sema.def(d).name.clone())
            .collect();
        if taken.contains(&name) {
            let mut i = 2;
            while taken.contains(&format!("{name}_{i}")) {
                i += 1;
            }
            name = format!("{name}_{i}");
        }
        let text = self.a.db.text(span.file);
        let ls = line_start(text, target.start);
        let indent = indent_at(text, ls);
        let blank = if indent.is_empty() { "\n" } else { "" };
        out.push(CodeAction {
            title: format!("Extract constant `{name}`"),
            kind: CodeActionKind::Extract,
            edits: vec![
                self.insert(
                    span.file,
                    ls,
                    format!(
                        "{indent}const {ty} {name} = {};\n{blank}",
                        lit.text
                    ),
                ),
                (span, name),
            ],
            fixes: None,
            preferred: false,
        });
    }

    /// Sort a table's `actions` list by name, or remove duplicate entries.
    /// Offered only when nothing but `;` and whitespace separates the
    /// entries, so no comment is moved away from its entry.
    fn sort_table_actions(&self, out: &mut Vec<CodeAction>) {
        let mut found = None;
        walk_tables(&self.a.parse.program.decls, &mut |t| {
            for p in &t.properties {
                if let PropertyValue::Actions(list) = &p.value {
                    if self.inside(p.span) && list.len() > 1 {
                        found = Some(list);
                    }
                }
            }
        });
        let Some(list) = found else { return };
        let file = list[0].span.file;
        let text = self.a.db.text(file);
        // Entries from a macro expansion share the invocation's span.
        if list.iter().any(|a| a.span.file != file)
            || list.windows(2).any(|w| w[0].span.end > w[1].span.start)
        {
            return;
        }
        let gaps: Vec<&str> = list
            .windows(2)
            .map(|w| &text[w[0].span.end as usize..w[1].span.start as usize])
            .collect();
        if gaps.iter().any(|g| g.trim() != ";" && !g.trim().is_empty()) {
            return;
        }
        let name = |a: &ActionRef| self.a.db.slice(a.name.span).to_string();
        let region =
            Span::new(file, list[0].span.start, list[list.len() - 1].span.end);
        let rebuild = |entries: Vec<&ActionRef>| {
            let mut s = String::new();
            for (i, a) in entries.iter().enumerate() {
                if i > 0 {
                    s.push_str(gaps[i - 1]);
                }
                s.push_str(self.a.db.slice(a.span));
            }
            s
        };
        let mut unique: Vec<&ActionRef> = Vec::new();
        for a in list {
            if !unique.iter().any(|u| name(u) == name(a)) {
                unique.push(a);
            }
        }
        if unique.len() < list.len() {
            out.push(CodeAction {
                title: "Remove duplicate actions".into(),
                kind: CodeActionKind::Rewrite,
                edits: vec![(region, rebuild(unique))],
                fixes: None,
                preferred: false,
            });
        }
        let key = |a: &&ActionRef| name(a).to_lowercase();
        let mut sorted: Vec<&ActionRef> = list.iter().collect();
        sorted.sort_by_key(key);
        if sorted.iter().zip(list).any(|(a, b)| a.span != b.span) {
            out.push(CodeAction {
                title: "Sort actions".into(),
                kind: CodeActionKind::Rewrite,
                edits: vec![(region, rebuild(sorted))],
                fixes: None,
                preferred: false,
            });
        }
    }

    /// Replace a macro invocation with its expansion.
    fn inline_macro(&self, out: &mut Vec<CodeAction>) {
        for &(use_span, m) in &self.a.pp.macro_uses {
            if !self.inside(use_span) {
                continue;
            }
            let toks: Vec<_> = self
                .a
                .pp
                .tokens
                .iter()
                .filter(|t| {
                    t.span.file == use_span.file
                        && t.span.start == use_span.start
                })
                .collect();
            let Some(first) = toks.first() else { continue };
            let invocation = first.span;
            if toks.iter().any(|t| t.span != invocation) {
                continue;
            }
            let mut text = String::new();
            for (i, t) in toks.iter().enumerate() {
                if i > 0 && t.ws_before {
                    text.push(' ');
                }
                text.push_str(&t.text);
            }
            out.push(CodeAction {
                title: format!("Inline macro `{}`", self.a.pp.macros[m].name),
                kind: CodeActionKind::Inline,
                edits: vec![(invocation, text)],
                fixes: None,
                preferred: false,
            });
        }
    }
}

fn line_start(text: &str, offset: u32) -> u32 {
    text[..offset as usize]
        .rfind('\n')
        .map_or(0, |i| i as u32 + 1)
}

/// The whitespace at the start of the line beginning at `line`.
fn indent_at(text: &str, line: u32) -> String {
    text[line as usize..]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// The file's indentation unit: a tab if lines are mostly indented with
/// tabs, otherwise four spaces.
fn unit(text: &str) -> String {
    let tabs = text.lines().filter(|l| l.starts_with('\t')).count();
    let spaces = text.lines().filter(|l| l.starts_with("  ")).count();
    if tabs > spaces {
        "\t".into()
    } else {
        "    ".into()
    }
}

/// Where to insert lines before the closing `}` of a braced span: the
/// offset, the indentation for the new lines, and a prefix (a newline when
/// the `}` shares its line with other code).
fn before_close(
    text: &str,
    span: Span,
    unit: &str,
) -> Option<(u32, String, String)> {
    let close = span.end.checked_sub(1)?;
    if text.as_bytes().get(close as usize) != Some(&b'}') {
        return None;
    }
    let ls = line_start(text, close);
    let close_indent = indent_at(text, ls);
    if text[ls as usize..close as usize].trim().is_empty() {
        Some((ls, format!("{close_indent}{unit}"), String::new()))
    } else {
        let open_line = line_start(text, span.start);
        let indent = indent_at(text, open_line);
        Some((close, format!("{indent}{unit}"), "\n".into()))
    }
}

/// Text to restore the closing brace's indentation after inserted lines
/// when the brace shared a line with other code.
fn close_indent(indent: &str, unit: &str, lead: &str) -> String {
    if lead.is_empty() {
        String::new()
    } else {
        indent.strip_suffix(unit).unwrap_or("").to_string()
    }
}

/// Extend a span to whole lines when nothing else shares them.
fn whole_lines(text: &str, span: Span) -> Span {
    let ls = line_start(text, span.start);
    let le = text[span.end as usize..]
        .find('\n')
        .map_or(text.len(), |i| span.end as usize + i + 1);
    let before = &text[ls as usize..span.start as usize];
    let after = &text[span.end as usize..le];
    if before.trim().is_empty() && after.trim().is_empty() {
        Span::new(span.file, ls, le as u32)
    } else {
        span
    }
}

/// Candidates within a small edit distance of `name`, closest first.
fn similar(name: &str, candidates: Vec<String>) -> Vec<String> {
    let limit = (name.chars().count() / 3).max(1);
    let lower = name.to_lowercase();
    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .filter(|c| c != name && !c.is_empty())
        .filter_map(|c| {
            let d = distance(&lower, &c.to_lowercase());
            (d <= limit).then_some((d, c))
        })
        .collect();
    scored.sort();
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.into_iter().take(3).map(|(_, c)| c).collect()
}

/// Levenshtein distance with adjacent transpositions.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, v) in d[0].iter_mut().enumerate() {
        *v = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

// -----------------------------------------------------------------------
// AST traversal

/// Every table declaration, including those in controls.
fn walk_tables<'a>(decls: &'a [Decl], f: &mut dyn FnMut(&'a TableDecl)) {
    for d in decls {
        match d {
            Decl::Table(t) => f(t),
            Decl::Control(c) => walk_tables(&c.locals, f),
            _ => {}
        }
    }
}

fn each_stmt<'a>(decls: &'a [Decl], f: &mut dyn FnMut(&'a Stmt)) {
    walk_decls(decls, f, &mut |_| {});
}

fn each_expr<'a>(decls: &'a [Decl], f: &mut dyn FnMut(&'a Expr)) {
    walk_decls(decls, &mut |_| {}, f);
}

fn walk_decls<'a>(
    decls: &'a [Decl],
    s: &mut dyn FnMut(&'a Stmt),
    e: &mut dyn FnMut(&'a Expr),
) {
    for d in decls {
        walk_decl(d, s, e);
    }
}

fn walk_decl<'a>(
    d: &'a Decl,
    s: &mut dyn FnMut(&'a Stmt),
    e: &mut dyn FnMut(&'a Expr),
) {
    match d {
        Decl::Const(c) => walk_expr(&c.value, e),
        Decl::Var(v) => {
            if let Some(i) = &v.init {
                walk_expr(i, e);
            }
        }
        Decl::Enum(en) => {
            for m in &en.members {
                if let Some(v) = &m.value {
                    walk_expr(v, e);
                }
            }
        }
        Decl::Function(f) => walk_block(&f.body, s, e),
        Decl::Action(a) => walk_block(&a.body, s, e),
        Decl::Parser(p) => {
            walk_decls(&p.locals, s, e);
            for st in &p.states {
                for x in &st.body {
                    walk_stmt(x, s, e);
                }
                if let Some(t) = &st.transition {
                    walk_transition(t, e);
                }
            }
        }
        Decl::Control(c) => {
            walk_decls(&c.locals, s, e);
            if let Some(a) = &c.apply {
                walk_block(a, s, e);
            }
        }
        Decl::Instantiation(i) => {
            for a in &i.args {
                walk_expr(&a.value, e);
            }
            if let Some(init) = &i.initializer {
                walk_decls(init, s, e);
            }
        }
        Decl::Table(t) => {
            for p in &t.properties {
                match &p.value {
                    PropertyValue::Key(keys) => {
                        for k in keys {
                            walk_expr(&k.expr, e);
                        }
                    }
                    PropertyValue::Actions(refs) => {
                        for r in refs {
                            for a in r.args.iter().flatten() {
                                walk_expr(&a.value, e);
                            }
                        }
                    }
                    PropertyValue::Entries(entries) => {
                        for en in entries {
                            walk_keyset(&en.keyset, e);
                            for a in en.action.args.iter().flatten() {
                                walk_expr(&a.value, e);
                            }
                        }
                    }
                    PropertyValue::Expr(x) => walk_expr(x, e),
                }
            }
        }
        _ => {}
    }
}

fn walk_block<'a>(
    b: &'a Block,
    s: &mut dyn FnMut(&'a Stmt),
    e: &mut dyn FnMut(&'a Expr),
) {
    for x in &b.stmts {
        walk_stmt(x, s, e);
    }
}

fn walk_stmt<'a>(
    st: &'a Stmt,
    s: &mut dyn FnMut(&'a Stmt),
    e: &mut dyn FnMut(&'a Expr),
) {
    s(st);
    match &st.kind {
        StmtKind::Decl(d) => walk_decl(d, s, e),
        StmtKind::Assign { lhs, rhs, .. } => {
            walk_expr(lhs, e);
            walk_expr(rhs, e);
        }
        StmtKind::Call(x) | StmtKind::Expr(x) => walk_expr(x, e),
        StmtKind::If { cond, then, els } => {
            walk_expr(cond, e);
            walk_stmt(then, s, e);
            if let Some(x) = els {
                walk_stmt(x, s, e);
            }
        }
        StmtKind::Block(b) => walk_block(b, s, e),
        StmtKind::Switch { expr, cases } => {
            walk_expr(expr, e);
            for c in cases {
                if let SwitchLabel::Expr(x) = &c.label {
                    walk_expr(x, e);
                }
                if let Some(b) = &c.body {
                    walk_block(b, s, e);
                }
            }
        }
        StmtKind::Return(Some(x)) => walk_expr(x, e),
        StmtKind::For {
            init,
            cond,
            update,
            body,
            ..
        } => {
            for x in init.iter().chain(update) {
                walk_stmt(x, s, e);
            }
            if let Some(c) = cond {
                walk_expr(c, e);
            }
            walk_stmt(body, s, e);
        }
        StmtKind::ForIn {
            collection, body, ..
        } => {
            walk_expr(collection, e);
            walk_stmt(body, s, e);
        }
        StmtKind::Transition(t) => walk_transition(t, e),
        _ => {}
    }
}

fn walk_transition<'a>(t: &'a Transition, e: &mut dyn FnMut(&'a Expr)) {
    if let TransitionKind::Select(sel) = &t.kind {
        for x in &sel.exprs {
            walk_expr(x, e);
        }
        for c in &sel.cases {
            walk_keyset(&c.keyset, e);
        }
    }
}

fn walk_keyset<'a>(k: &'a Keyset, e: &mut dyn FnMut(&'a Expr)) {
    match &k.kind {
        KeysetKind::Expr(x) => walk_expr(x, e),
        KeysetKind::Mask(a, b) | KeysetKind::Range(a, b) => {
            walk_expr(a, e);
            walk_expr(b, e);
        }
        KeysetKind::Tuple(ks) => {
            for k in ks {
                walk_keyset(k, e);
            }
        }
        KeysetKind::Default | KeysetKind::DontCare => {}
    }
}

fn walk_expr<'a>(x: &'a Expr, e: &mut dyn FnMut(&'a Expr)) {
    e(x);
    match &x.kind {
        ExprKind::Member { base, .. } => walk_expr(base, e),
        ExprKind::Index { base, index } => {
            walk_expr(base, e);
            walk_expr(index, e);
        }
        ExprKind::Slice { base, hi, lo } => {
            walk_expr(base, e);
            walk_expr(hi, e);
            walk_expr(lo, e);
        }
        ExprKind::PlusSlice { base, start, width } => {
            walk_expr(base, e);
            walk_expr(start, e);
            walk_expr(width, e);
        }
        ExprKind::Call { callee, args, .. } => {
            walk_expr(callee, e);
            for a in args {
                walk_expr(&a.value, e);
            }
        }
        ExprKind::Specialized { base, .. } => walk_expr(base, e),
        ExprKind::Cast { expr, .. } | ExprKind::Unary { expr, .. } => {
            walk_expr(expr, e)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, e);
            walk_expr(rhs, e);
        }
        ExprKind::Ternary { cond, then, els } => {
            walk_expr(cond, e);
            walk_expr(then, e);
            walk_expr(els, e);
        }
        ExprKind::List(xs) => {
            for y in xs {
                walk_expr(y, e);
            }
        }
        ExprKind::Struct { fields, .. } => {
            for f in fields {
                walk_expr(&f.value, e);
            }
        }
        _ => {}
    }
}
