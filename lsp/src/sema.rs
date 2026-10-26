// Copyright 2026 Oxide Computer Company

//! Semantic model: declarations, scopes, name resolution, and the types
//! needed to resolve member accesses.
//!
//! Resolution is deliberately lenient. Declarations at the top level and in
//! parser and control bodies are visible throughout their scope (so a
//! forward reference still navigates), while declarations in statement
//! blocks become visible where they appear. Missing names are reported only
//! where a report is certain to be correct: unknown members are reported
//! only when the base type is fully known, and nothing is reported when an
//! `#include` could not be resolved, because missing declarations would
//! cascade.

use crate::analysis::SymbolKind;
use crate::diag::Diagnostic;
use crate::source::{SourceDb, Span};
use crate::syntax::ast::*;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u32);

/// The type of a value or the meaning of a type expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ty {
    Unknown,
    /// Another type without members: `void`, `match_kind`, lists, ...
    Base,
    /// `bit<W>`, or `int<W>` when signed. The width is `None` when it is not
    /// a constant the analysis can evaluate.
    Bits {
        signed: bool,
        width: Option<u32>,
    },
    /// `varbit<W>`.
    Varbit(Option<u32>),
    /// Arbitrary-precision `int`, the type of unsized integer literals.
    Int,
    Bool,
    String,
    /// A declared type with type arguments: struct, header, header union,
    /// enum, extern, parser, control, package, typedef, type parameter, or
    /// table.
    Def(DefId, Vec<Ty>),
    /// A header stack and its size, if known.
    Stack(Box<Ty>, Option<u32>),
    Tuple(Vec<Ty>),
    /// The `error` type.
    Error,
    /// A type name used as a value, as in `E.A` or `error.NoError`.
    TypeName(Box<Ty>),
    /// The result of `table.apply()`.
    TableApply(DefId),
}

#[derive(Clone, Debug)]
pub struct Def {
    pub name: String,
    pub kind: SymbolKind,
    /// The declared name, or `None` for built-in members such as
    /// `isValid`.
    pub name_span: Option<Span>,
    /// The whole declaration.
    pub span: Option<Span>,
    /// The enclosing declaration, for document symbols and hover context.
    pub parent: Option<DefId>,
    /// The type of the value (variables, parameters, fields, instances), or
    /// the aliased type of a typedef, or the return type of a function.
    pub ty: Ty,
    /// One-line P4 rendering of the declaration for hover and completion.
    pub detail: String,
    /// Type parameters of generic declarations.
    pub type_params: Vec<DefId>,
    /// Fields, enum members, extern methods, or callable parameters.
    pub members: Vec<DefId>,
    /// Parameters of callables (actions, functions, methods, parsers,
    /// controls, packages, externs' constructors).
    pub params: Vec<DefId>,
    /// Number of parameters that must be supplied (no default value and
    /// not `@optional`).
    pub required_params: usize,
    /// Declared inside a statement body or parameter list.
    pub local: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ref {
    pub span: Span,
    pub def: DefId,
    pub write: bool,
}

#[derive(Debug)]
pub struct Scope {
    pub parent: Option<ScopeId>,
    pub names: HashMap<String, Vec<DefId>>,
    /// The source region of the scope, for completion.
    pub span: Option<Span>,
}

/// A member access `base.member`, recorded for member completion.
#[derive(Clone, Debug)]
pub struct MemberSite {
    /// Span of the member name; zero-width right after the dot when the
    /// member is missing.
    pub span: Span,
    pub base: Ty,
}

/// A call's argument list, recorded for signature help.
#[derive(Clone, Debug)]
pub struct CallSite {
    /// From the end of the callee to the end of the call.
    pub span: Span,
    pub candidates: Vec<DefId>,
    pub arg_spans: Vec<Span>,
}

/// What an unresolved name was expected to be, for quick fixes.
#[derive(Clone, Debug)]
pub enum UnresolvedKind {
    /// A value in an expression.
    Name,
    Type,
    State,
    MatchKind,
    /// A member of a value of this type.
    Member(Ty),
    /// An action named by a table (`actions`, `default_action`, entries),
    /// with guessed parameter types and names from its arguments.
    Action {
        table: Span,
        params: Vec<(String, String)>,
    },
}

#[derive(Clone, Debug)]
pub struct Unresolved {
    pub name: String,
    pub span: Span,
    pub kind: UnresolvedKind,
    /// The scope the name was looked up in.
    pub scope: ScopeId,
    /// The enclosing parser, for creating a missing state.
    pub parser: Option<DefId>,
}

/// An expression whose type does not match what its context expects.
#[derive(Clone, Debug)]
pub struct Mismatch {
    pub span: Span,
    /// The expected type, as P4 source (`bit<8>`).
    pub expected: String,
    /// Whether the expression needs parentheses under a cast.
    pub compound: bool,
}

/// An action used by a table's `default_action` or entries but missing from
/// its `actions` list.
#[derive(Clone, Debug)]
pub struct NotInActions {
    pub name: String,
    /// The use of the action.
    pub span: Span,
    /// The closing `}` of the `actions` list.
    pub list_end: Span,
}

#[derive(Debug, Default)]
pub struct Sema {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
    pub scopes: Vec<Scope>,
    pub members: Vec<MemberSite>,
    pub calls: Vec<CallSite>,
    pub diagnostics: Vec<Diagnostic>,
    /// Members of every `error { ... }` declaration.
    pub errors: Vec<DefId>,
    /// Names that did not resolve, including those not reported because
    /// an include is missing.
    pub unresolved: Vec<Unresolved>,
    pub not_in_actions: Vec<NotInActions>,
    /// Each table's `actions` list.
    pub table_actions: HashMap<DefId, Vec<DefId>>,
    /// Source text of the declared type of variables, constants,
    /// parameters, and fields.
    pub type_text: HashMap<DefId, String>,
    /// Values of constants initialized with an integer literal.
    pub const_values: HashMap<DefId, u128>,
    /// Local declarations that are never referenced.
    pub unused: Vec<DefId>,
    /// The underlying type of serializable enums (`enum bit<8> E { ... }`).
    pub enum_reprs: HashMap<DefId, Ty>,
    /// Parameter directions.
    pub directions: HashMap<DefId, Direction>,
    /// Values whose fixed-width type differs from the expected one, for
    /// the cast quick fix.
    pub mismatches: Vec<Mismatch>,
    /// `(abstract, concrete)`: a parser, control, or package type (such as
    /// an architecture's `control Ingress<H, M>(...)`) and the block or
    /// instance passed for it when a package is instantiated.
    pub implementations: Vec<(DefId, DefId)>,
    builtins: HashMap<&'static str, DefId>,
}

impl Sema {
    pub fn def(&self, id: DefId) -> &Def {
        &self.defs[id.0 as usize]
    }

    /// Follow typedefs to the underlying type.
    pub fn resolve_alias(&self, ty: &Ty) -> Ty {
        let mut ty = ty.clone();
        for _ in 0..32 {
            match &ty {
                Ty::Def(id, _) if self.def(*id).kind == SymbolKind::Typedef => {
                    ty = self.def(*id).ty.clone();
                }
                _ => break,
            }
        }
        ty
    }

    /// The declaration a type refers to, after following typedefs.
    pub fn type_def(&self, ty: &Ty) -> Option<DefId> {
        match self.resolve_alias(ty) {
            Ty::Def(id, _) | Ty::TableApply(id) => Some(id),
            Ty::Stack(elem, _) => self.type_def(&elem),
            Ty::TypeName(t) => self.type_def(&t),
            _ => None,
        }
    }

    /// Members available on a value of type `ty`, with their types.
    pub fn members_of(&self, ty: &Ty) -> Vec<(DefId, Ty)> {
        let ty = self.resolve_alias(ty);
        let b = |name: &str| self.builtins[name];
        let mut out = Vec::new();
        match &ty {
            Ty::Def(id, args) => {
                let def = self.def(*id);
                let env = self.env(def, args);
                match def.kind {
                    SymbolKind::Struct
                    | SymbolKind::Header
                    | SymbolKind::HeaderUnion => {
                        for &m in &def.members {
                            out.push((m, subst(&self.def(m).ty, &env)));
                        }
                        let sizes = [
                            "minSizeInBits",
                            "minSizeInBytes",
                            "maxSizeInBits",
                            "maxSizeInBytes",
                        ];
                        let validity: &[&str] = match def.kind {
                            SymbolKind::Header => {
                                &["isValid", "setValid", "setInvalid"]
                            }
                            SymbolKind::HeaderUnion => &["isValid"],
                            _ => &[],
                        };
                        let builtins = validity.iter().chain(&sizes);
                        out.extend(builtins.map(|n| (b(n), Ty::Base)));
                    }
                    SymbolKind::Extern => {
                        for &m in &def.members {
                            if self.def(m).kind == SymbolKind::Method {
                                out.push((m, subst(&self.def(m).ty, &env)));
                            }
                        }
                    }
                    SymbolKind::Parser | SymbolKind::Control => {
                        out.push((b("apply"), Ty::Base));
                    }
                    SymbolKind::Table => {
                        out.push((b("table.apply"), Ty::TableApply(*id)));
                    }
                    _ => {}
                }
            }
            Ty::Stack(elem, _) => {
                for n in ["next", "last"] {
                    out.push((b(n), (**elem).clone()));
                }
                for n in [
                    "size",
                    "lastIndex",
                    "nextIndex",
                    "push_front",
                    "pop_front",
                ] {
                    out.push((b(n), Ty::Base));
                }
            }
            Ty::TableApply(_) => {
                for n in ["hit", "miss", "action_run"] {
                    out.push((b(n), Ty::Base));
                }
            }
            Ty::TypeName(t) => match self.resolve_alias(t) {
                Ty::Error => {
                    out.extend(self.errors.iter().map(|&e| (e, Ty::Error)));
                }
                Ty::Def(id, args) if self.def(id).kind == SymbolKind::Enum => {
                    for &m in &self.def(id).members {
                        out.push((m, Ty::Def(id, args.clone())));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        out
    }

    /// Whether every member of `ty` is known, so a missing member is
    /// certainly an error.
    fn members_complete(&self, ty: &Ty) -> bool {
        match self.resolve_alias(ty) {
            Ty::Def(id, _) => matches!(
                self.def(id).kind,
                SymbolKind::Struct
                    | SymbolKind::Header
                    | SymbolKind::HeaderUnion
                    | SymbolKind::Table
            ),
            Ty::Stack(..) | Ty::TableApply(_) => true,
            Ty::TypeName(t) => match self.resolve_alias(&t) {
                Ty::Error => true,
                Ty::Def(id, _) => self.def(id).kind == SymbolKind::Enum,
                _ => false,
            },
            _ => false,
        }
    }

    fn env(&self, def: &Def, args: &[Ty]) -> Vec<(DefId, Ty)> {
        def.type_params
            .iter()
            .copied()
            .zip(args.iter().cloned())
            .collect()
    }

    /// Names visible at a position, innermost first, for completion.
    pub fn visible_at(&self, span: Span) -> Vec<DefId> {
        // The innermost scope containing the position.
        let mut best: Option<(ScopeId, u32)> = None;
        for (i, s) in self.scopes.iter().enumerate() {
            if let Some(ss) = s.span {
                if ss.contains(span.file, span.start)
                    && best.is_none_or(|(_, len)| ss.len() <= len)
                {
                    best = Some((ScopeId(i as u32), ss.len()));
                }
            }
        }
        self.visible_from(best.map_or(ScopeId(0), |b| b.0))
    }

    /// Names x4c declares implicitly that the program does not declare.
    pub fn implicit_names(&self) -> Vec<DefId> {
        self.builtins
            .iter()
            .filter(|(k, _)| k.starts_with("x4c."))
            .map(|(_, &d)| d)
            .filter(|&d| !self.scopes[0].names.contains_key(&self.def(d).name))
            .collect()
    }

    /// Names visible from a scope, innermost first.
    pub fn visible_from(&self, scope: ScopeId) -> Vec<DefId> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cur = Some(scope);
        while let Some(id) = cur {
            let scope = &self.scopes[id.0 as usize];
            // Declaration order within each scope.
            let mut names: Vec<_> = scope.names.iter().collect();
            names.sort_by_key(|(_, defs)| defs.first().copied());
            for (name, defs) in names {
                if seen.insert(name.clone()) {
                    out.extend(defs.iter().copied());
                }
            }
            cur = scope.parent;
        }
        out
    }
}

fn subst(ty: &Ty, env: &[(DefId, Ty)]) -> Ty {
    match ty {
        Ty::Def(id, args) => {
            if let Some((_, t)) = env.iter().find(|(p, _)| p == id) {
                return t.clone();
            }
            Ty::Def(*id, args.iter().map(|a| subst(a, env)).collect())
        }
        Ty::Stack(e, n) => Ty::Stack(Box::new(subst(e, env)), *n),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| subst(t, env)).collect()),
        t => t.clone(),
    }
}

/// Whether a kind names a type (as opposed to a value).
pub fn is_type_kind(kind: SymbolKind) -> bool {
    use SymbolKind::*;
    matches!(
        kind,
        Typedef
            | Struct
            | Header
            | HeaderUnion
            | Enum
            | Extern
            | Parser
            | Control
            | Package
            | TypeParameter
    )
}

/// Kinds that may share a name within one scope: p4c overloads these by
/// parameter count.
fn overloadable(kind: SymbolKind) -> bool {
    use SymbolKind::*;
    matches!(kind, Function | Method | Parser | Control | Package)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Type,
    Value,
}

pub fn analyze(
    program: &Program,
    db: &SourceDb,
    report_unresolved: bool,
) -> Sema {
    let mut r = Resolver {
        s: Sema::default(),
        db,
        scope: ScopeId(0),
        parent: None,
        local: false,
        report_unresolved,
        quiet: false,
        action_table: None,
        parser: None,
    };
    r.s.scopes.push(Scope {
        parent: None,
        names: HashMap::new(),
        span: None,
    });
    r.builtins();
    for d in &program.decls {
        r.declare(d);
    }
    r.pre_types(&program.decls);
    for d in &program.decls {
        r.walk_decl(d);
    }
    r.find_unused();
    r.s
}

struct Resolver<'a> {
    s: Sema,
    db: &'a SourceDb,
    scope: ScopeId,
    /// The declaration being walked, recorded as each new def's parent.
    parent: Option<DefId>,
    local: bool,
    report_unresolved: bool,
    /// Suppresses references and diagnostics during type pre-passes.
    quiet: bool,
    /// While resolving a table's action reference: the table's name span
    /// and the guessed parameters of the action.
    action_table: Option<(Span, Vec<(String, String)>)>,
    /// The parser being walked.
    parser: Option<DefId>,
}

impl Resolver<'_> {
    fn builtins(&mut self) {
        let list: &[(&'static str, &str, &str)] = &[
            ("isValid", "isValid", "bool isValid()"),
            ("setValid", "setValid", "void setValid()"),
            ("setInvalid", "setInvalid", "void setInvalid()"),
            ("minSizeInBits", "minSizeInBits", "int minSizeInBits()"),
            ("minSizeInBytes", "minSizeInBytes", "int minSizeInBytes()"),
            ("maxSizeInBits", "maxSizeInBits", "int maxSizeInBits()"),
            ("maxSizeInBytes", "maxSizeInBytes", "int maxSizeInBytes()"),
            ("apply", "apply", "void apply(...)"),
            ("table.apply", "apply", "apply_result apply()"),
            ("next", "next", "next element of the header stack"),
            ("last", "last", "last valid element of the header stack"),
            ("size", "size", "const bit<32> size"),
            ("lastIndex", "lastIndex", "bit<32> lastIndex"),
            ("nextIndex", "nextIndex", "bit<32> nextIndex"),
            ("push_front", "push_front", "void push_front(int count)"),
            ("pop_front", "pop_front", "void pop_front(int count)"),
            ("hit", "hit", "bool hit"),
            ("miss", "miss", "bool miss"),
            ("action_run", "action_run", "action_list action_run"),
            ("accept", "accept", "state accept"),
            ("reject", "reject", "state reject"),
            // x4c declares these implicitly (its core.p4 does not); they
            // apply only when the program declares no such name.
            ("x4c.NoAction", "NoAction", "action NoAction() // built into x4c"),
            ("x4c.exact", "exact", "match_kind exact // built into x4c"),
            ("x4c.ternary", "ternary", "match_kind ternary // built into x4c"),
            ("x4c.lpm", "lpm", "match_kind lpm // built into x4c"),
            ("x4c.range", "range", "match_kind range // built into x4c"),
            (
                "x4c.SoftNPU",
                "SoftNPU",
                "package SoftNPU(parser, control ingress, control egress) // built into x4c",
            ),
        ];
        for &(key, name, detail) in list {
            let kind = match key {
                "accept" | "reject" => SymbolKind::State,
                "x4c.NoAction" => SymbolKind::Action,
                "x4c.SoftNPU" => SymbolKind::Package,
                k if k.starts_with("x4c.") => SymbolKind::MatchKind,
                "size" | "lastIndex" | "nextIndex" | "hit" | "miss"
                | "action_run" | "next" | "last" => SymbolKind::Field,
                _ => SymbolKind::Method,
            };
            let id = self.push_def(Def {
                name: name.into(),
                kind,
                name_span: None,
                span: None,
                parent: None,
                ty: if key == "isValid" { Ty::Bool } else { Ty::Base },
                detail: detail.into(),
                type_params: Vec::new(),
                members: Vec::new(),
                params: Vec::new(),
                required_params: 0,
                local: false,
            });
            self.s.builtins.insert(key, id);
        }
    }

    fn push_def(&mut self, def: Def) -> DefId {
        let id = DefId(self.s.defs.len() as u32);
        self.s.defs.push(def);
        id
    }

    fn text(&self, span: Span) -> String {
        // Collapse whitespace so multi-line types render on one line.
        self.db
            .slice(span)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Create a def for `name` and add it to the current scope.
    fn define(
        &mut self,
        name: &Name,
        kind: SymbolKind,
        span: Span,
        detail: String,
    ) -> DefId {
        let id = self.push_def(Def {
            name: name.text.clone(),
            kind,
            name_span: Some(name.span),
            span: Some(span),
            parent: self.parent,
            ty: Ty::Unknown,
            detail,
            type_params: Vec::new(),
            members: Vec::new(),
            params: Vec::new(),
            required_params: 0,
            local: self.local,
        });
        if !name.is_missing() {
            self.add_to_scope(self.scope, id);
        }
        id
    }

    fn add_to_scope(&mut self, scope: ScopeId, id: DefId) {
        let def = &self.s.defs[id.0 as usize];
        let (name, kind, span) = (def.name.clone(), def.kind, def.name_span);
        let entry = self.s.scopes[scope.0 as usize]
            .names
            .entry(name.clone())
            .or_default();
        if let Some(&prev) = entry.iter().find(|&&p| {
            let pk = self.s.defs[p.0 as usize].kind;
            !(overloadable(pk) && overloadable(kind))
                && is_type_kind(pk) == is_type_kind(kind)
        }) {
            let prev_span = self.s.defs[prev.0 as usize].name_span;
            // A forward `extern E;` or a parser/control type followed by
            // its definition is not a duplicate in practice; neither are
            // declarations we cannot locate.
            if let (Some(span), Some(prev_span)) = (span, prev_span) {
                if prev_span != span {
                    self.s.diagnostics.push(
                        Diagnostic::error(
                            span,
                            format!(
                                "`{name}` is already declared in this scope"
                            ),
                        )
                        .with_related(prev_span, "previous declaration"),
                    );
                }
            }
        }
        entry.push(id);
    }

    fn push_scope(&mut self, span: Option<Span>) -> ScopeId {
        let id = ScopeId(self.s.scopes.len() as u32);
        self.s.scopes.push(Scope {
            parent: Some(self.scope),
            names: HashMap::new(),
            span,
        });
        id
    }

    fn with_scope<T>(
        &mut self,
        span: Option<Span>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let saved = self.scope;
        self.scope = self.push_scope(span);
        let out = f(self);
        self.scope = saved;
        out
    }

    fn with_parent<T>(
        &mut self,
        parent: DefId,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let saved = self.parent.replace(parent);
        let out = f(self);
        self.parent = saved;
        out
    }

    fn lookup(&self, name: &str, want: Want, global: bool) -> Vec<DefId> {
        let mut cur = Some(if global { ScopeId(0) } else { self.scope });
        let mut fallback = Vec::new();
        while let Some(id) = cur {
            let scope = &self.s.scopes[id.0 as usize];
            if let Some(defs) = scope.names.get(name) {
                let preferred: Vec<DefId> = defs
                    .iter()
                    .copied()
                    .filter(|d| {
                        is_type_kind(self.s.def(*d).kind)
                            == (want == Want::Type)
                    })
                    .collect();
                if !preferred.is_empty() {
                    return preferred;
                }
                if fallback.is_empty() {
                    fallback = defs.clone();
                }
            }
            cur = if global { None } else { scope.parent };
        }
        if fallback.is_empty() {
            if let Some(&b) =
                self.s.builtins.get(format!("x4c.{name}").as_str())
            {
                fallback.push(b);
            }
        }
        fallback
    }

    fn add_ref(&mut self, span: Span, def: DefId, write: bool) {
        if !self.quiet {
            self.s.refs.push(Ref { span, def, write });
        }
    }

    fn unresolved(&mut self, name: &Name, what: &str, kind: UnresolvedKind) {
        if self.quiet || name.is_missing() {
            return;
        }
        let kind = match (kind, &self.action_table) {
            (UnresolvedKind::Name, Some((table, params))) => {
                UnresolvedKind::Action {
                    table: *table,
                    params: params.clone(),
                }
            }
            (k, _) => k,
        };
        self.s.unresolved.push(Unresolved {
            name: name.text.clone(),
            span: name.span,
            kind,
            scope: self.scope,
            parser: self.parser,
        });
        if self.report_unresolved {
            self.s.diagnostics.push(
                Diagnostic::error(
                    name.span,
                    format!("cannot find {what} `{}` in this scope", name.text),
                )
                .with_code("unresolved"),
            );
        }
    }

    // -----------------------------------------------------------------
    // Declaration pass: create defs and members, without resolving bodies.

    fn params_detail(&self, params: &[Param]) -> String {
        params
            .iter()
            .map(|p| self.text(p.span))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn type_params_detail(names: &[Name]) -> String {
        if names.is_empty() {
            String::new()
        } else {
            let names: Vec<_> = names.iter().map(|n| n.text.as_str()).collect();
            format!("<{}>", names.join(", "))
        }
    }

    fn declare_type_params(&mut self, owner: DefId, names: &[Name]) {
        let ids: Vec<DefId> = self.with_parent(owner, |r| {
            names
                .iter()
                .map(|n| {
                    let id = r.push_def(Def {
                        name: n.text.clone(),
                        kind: SymbolKind::TypeParameter,
                        name_span: Some(n.span),
                        span: Some(n.span),
                        parent: Some(owner),
                        ty: Ty::Unknown,
                        detail: format!("type parameter {}", n.text),
                        type_params: Vec::new(),
                        members: Vec::new(),
                        params: Vec::new(),
                        required_params: 0,
                        local: true,
                    });
                    r.s.defs[id.0 as usize].ty = Ty::Def(id, Vec::new());
                    id
                })
                .collect()
        });
        self.s.defs[owner.0 as usize].type_params = ids;
    }

    /// Declare parameters as defs owned by `owner` without adding them to a
    /// scope; the walk adds them to the body scope.
    /// With `signature` set, the parameters become the owner's callable
    /// signature; constructor parameters of parsers and controls do not.
    fn declare_params(
        &mut self,
        owner: DefId,
        params: &[Param],
        signature: bool,
    ) -> Vec<DefId> {
        let ids: Vec<DefId> = params
            .iter()
            .map(|p| {
                self.push_def(Def {
                    name: p.name.text.clone(),
                    kind: SymbolKind::Parameter,
                    name_span: Some(p.name.span),
                    span: Some(p.span),
                    parent: Some(owner),
                    ty: Ty::Unknown,
                    detail: self.text(p.span),
                    type_params: Vec::new(),
                    members: Vec::new(),
                    params: Vec::new(),
                    required_params: 0,
                    local: true,
                })
            })
            .collect();
        for (p, &id) in params.iter().zip(&ids) {
            let t = self.text(p.ty.span);
            self.s.type_text.insert(id, t);
            self.s.directions.insert(id, p.direction);
        }
        let required = params
            .iter()
            .filter(|p| {
                p.default.is_none()
                    && !p.annotations.iter().any(|a| a.name.text == "optional")
            })
            .count();
        if signature {
            let def = &mut self.s.defs[owner.0 as usize];
            def.params.extend(ids.iter().copied());
            def.required_params += required;
        }
        ids
    }

    fn declare(&mut self, d: &Decl) {
        match d {
            Decl::Const(c) => {
                let detail = format!(
                    "const {} {} = {}",
                    self.text(c.ty.span),
                    c.name.text,
                    self.text(c.value.span)
                );
                let id =
                    self.define(&c.name, SymbolKind::Constant, c.span, detail);
                self.s.type_text.insert(id, self.text(c.ty.span));
                if let ExprKind::Int(IntLit { value: Some(v), .. }) =
                    &c.value.kind
                {
                    self.s.const_values.insert(id, *v);
                }
            }
            Decl::Var(v) => {
                let detail =
                    format!("{} {}", self.text(v.ty.span), v.name.text);
                let id =
                    self.define(&v.name, SymbolKind::Variable, v.span, detail);
                self.s.type_text.insert(id, self.text(v.ty.span));
            }
            Decl::Typedef(t) => {
                if let Some(inline) = &t.inline_decl {
                    self.declare(inline);
                }
                let kw = match t.kind {
                    TypedefKind::Typedef => "typedef",
                    TypedefKind::Type => "type",
                };
                let detail =
                    format!("{kw} {} {}", self.text(t.ty.span), t.name.text);
                self.define(&t.name, SymbolKind::Typedef, t.span, detail);
            }
            Decl::Struct(s) => {
                let (kind, kw) = match s.kind {
                    StructKind::Struct => (SymbolKind::Struct, "struct"),
                    StructKind::Header => (SymbolKind::Header, "header"),
                    StructKind::HeaderUnion => {
                        (SymbolKind::HeaderUnion, "header_union")
                    }
                };
                let detail = format!(
                    "{kw} {}{}",
                    s.name.text,
                    Self::type_params_detail(&s.type_params)
                );
                let id = self.define(&s.name, kind, s.span, detail);
                self.declare_type_params(id, &s.type_params);
                let fields: Vec<DefId> = self.with_parent(id, |r| {
                    s.fields
                        .iter()
                        .map(|f| {
                            let detail = format!(
                                "{} {}",
                                r.text(f.ty.span),
                                f.name.text
                            );
                            r.push_def(Def {
                                name: f.name.text.clone(),
                                kind: SymbolKind::Field,
                                name_span: Some(f.name.span),
                                span: Some(f.span),
                                parent: Some(id),
                                ty: Ty::Unknown,
                                detail,
                                type_params: Vec::new(),
                                members: Vec::new(),
                                params: Vec::new(),
                                required_params: 0,
                                local: false,
                            })
                        })
                        .collect()
                });
                for (f, &fid) in s.fields.iter().zip(&fields) {
                    let t = self.text(f.ty.span);
                    self.s.type_text.insert(fid, t);
                }
                self.check_member_duplicates(&fields);
                self.s.defs[id.0 as usize].members = fields;
            }
            Decl::Enum(e) => {
                let detail = match &e.repr {
                    Some(t) => {
                        format!("enum {} {}", self.text(t.span), e.name.text)
                    }
                    None => format!("enum {}", e.name.text),
                };
                let id = self.define(&e.name, SymbolKind::Enum, e.span, detail);
                let members: Vec<DefId> = e
                    .members
                    .iter()
                    .map(|m| {
                        let detail = match &m.value {
                            Some(v) => format!(
                                "{}.{} = {}",
                                e.name.text,
                                m.name.text,
                                self.text(v.span)
                            ),
                            None => format!("{}.{}", e.name.text, m.name.text),
                        };
                        self.push_def(Def {
                            name: m.name.text.clone(),
                            kind: SymbolKind::EnumMember,
                            name_span: Some(m.name.span),
                            span: Some(m.span),
                            parent: Some(id),
                            ty: Ty::Def(id, Vec::new()),
                            detail,
                            type_params: Vec::new(),
                            members: Vec::new(),
                            params: Vec::new(),
                            required_params: 0,
                            local: false,
                        })
                    })
                    .collect();
                self.check_member_duplicates(&members);
                self.s.defs[id.0 as usize].members = members;
            }
            Decl::Errors(list) => {
                for m in &list.members {
                    let id = self.push_def(Def {
                        name: m.text.clone(),
                        kind: SymbolKind::Error,
                        name_span: Some(m.span),
                        span: Some(m.span),
                        parent: None,
                        ty: Ty::Error,
                        detail: format!("error.{}", m.text),
                        type_params: Vec::new(),
                        members: Vec::new(),
                        params: Vec::new(),
                        required_params: 0,
                        local: false,
                    });
                    self.s.errors.push(id);
                }
            }
            Decl::MatchKinds(list) => {
                for m in &list.members {
                    self.define(
                        m,
                        SymbolKind::MatchKind,
                        m.span,
                        format!("match_kind {}", m.text),
                    );
                }
            }
            Decl::Extern(e) => {
                let detail = format!(
                    "extern {}{}",
                    e.name.text,
                    Self::type_params_detail(&e.type_params)
                );
                // A forward declaration followed by a full one shares a def.
                let existing = self
                    .lookup(&e.name.text, Want::Type, false)
                    .into_iter()
                    .find(|&d| {
                        self.s.def(d).kind == SymbolKind::Extern
                            && self.s.def(d).members.is_empty()
                    });
                let id = match existing {
                    Some(id) if !e.forward => {
                        let def = &mut self.s.defs[id.0 as usize];
                        def.name_span = Some(e.name.span);
                        def.span = Some(e.span);
                        id
                    }
                    Some(id) => id,
                    None => {
                        self.define(&e.name, SymbolKind::Extern, e.span, detail)
                    }
                };
                self.declare_type_params(id, &e.type_params);
                let methods: Vec<DefId> = self.with_parent(id, |r| {
                    e.methods
                        .iter()
                        .map(|m| r.declare_method(id, e, m))
                        .collect()
                });
                self.s.defs[id.0 as usize].members = methods;
            }
            Decl::ExternFunction(f) => {
                self.declare_function(f, true);
            }
            Decl::Function(f) => {
                self.declare_function(&f.proto, false);
            }
            Decl::Action(a) => {
                let detail = format!(
                    "action {}({})",
                    a.name.text,
                    self.params_detail(&a.params)
                );
                let id =
                    self.define(&a.name, SymbolKind::Action, a.span, detail);
                self.declare_params(id, &a.params, true);
            }
            Decl::ParserType(b) | Decl::ControlType(b) | Decl::Package(b) => {
                let (kind, kw) = match d {
                    Decl::ParserType(_) => (SymbolKind::Parser, "parser"),
                    Decl::ControlType(_) => (SymbolKind::Control, "control"),
                    _ => (SymbolKind::Package, "package"),
                };
                let detail = format!(
                    "{kw} {}{}({})",
                    b.name.text,
                    Self::type_params_detail(&b.type_params),
                    self.params_detail(&b.params)
                );
                let id = self.define(&b.name, kind, b.span, detail);
                self.declare_type_params(id, &b.type_params);
                self.declare_params(id, &b.params, true);
            }
            Decl::Parser(p) => {
                let detail = format!(
                    "parser {}{}({})",
                    p.name.text,
                    Self::type_params_detail(&p.type_params),
                    self.params_detail(&p.params)
                );
                let id =
                    self.define(&p.name, SymbolKind::Parser, p.span, detail);
                self.declare_type_params(id, &p.type_params);
                self.declare_params(id, &p.params, true);
            }
            Decl::Control(c) => {
                let detail = format!(
                    "control {}{}({})",
                    c.name.text,
                    Self::type_params_detail(&c.type_params),
                    self.params_detail(&c.params)
                );
                let id =
                    self.define(&c.name, SymbolKind::Control, c.span, detail);
                self.declare_type_params(id, &c.type_params);
                self.declare_params(id, &c.params, true);
            }
            Decl::Instantiation(i) => {
                let detail =
                    format!("{} {}", self.text(i.ty.span), i.name.text);
                self.define(&i.name, SymbolKind::Instance, i.span, detail);
            }
            Decl::ValueSet(v) => {
                let detail = format!(
                    "value_set<{}>({}) {}",
                    self.text(v.elem_ty.span),
                    self.text(v.size.span),
                    v.name.text
                );
                self.define(&v.name, SymbolKind::ValueSet, v.span, detail);
            }
            Decl::Table(t) => {
                let detail = format!("table {}", t.name.text);
                let id =
                    self.define(&t.name, SymbolKind::Table, t.span, detail);
                self.s.defs[id.0 as usize].ty = Ty::Def(id, Vec::new());
            }
            Decl::Error(_) => {}
        }
    }

    fn check_member_duplicates(&mut self, members: &[DefId]) {
        let mut seen: HashMap<String, DefId> = HashMap::new();
        for &m in members {
            let def = self.s.def(m);
            let (name, span) = (def.name.clone(), def.name_span);
            if name.is_empty() {
                continue;
            }
            if let Some(&prev) = seen.get(&name) {
                if let (Some(span), Some(prev_span)) =
                    (span, self.s.def(prev).name_span)
                {
                    self.s.diagnostics.push(
                        Diagnostic::error(
                            span,
                            format!("duplicate member `{name}`"),
                        )
                        .with_related(prev_span, "previous declaration"),
                    );
                }
            } else {
                seen.insert(name, m);
            }
        }
    }

    fn declare_method(
        &mut self,
        owner: DefId,
        e: &ExternDecl,
        m: &MethodDecl,
    ) -> DefId {
        let tp = Self::type_params_detail(&m.type_params);
        let params = self.params_detail(&m.params);
        let detail = match &m.ret {
            None => format!("{}({params})", e.name.text),
            Some(ret) => format!(
                "{}{} {}{tp}({params})",
                if m.is_abstract { "abstract " } else { "" },
                self.text(ret.span),
                m.name.text
            ),
        };
        let id = self.push_def(Def {
            name: m.name.text.clone(),
            kind: if m.is_constructor() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            },
            name_span: Some(m.name.span),
            span: Some(m.span),
            parent: Some(owner),
            ty: Ty::Unknown,
            detail,
            type_params: Vec::new(),
            members: Vec::new(),
            params: Vec::new(),
            required_params: 0,
            local: false,
        });
        self.declare_type_params(id, &m.type_params);
        self.declare_params(id, &m.params, true);
        id
    }

    fn declare_function(
        &mut self,
        f: &FunctionProto,
        is_extern: bool,
    ) -> DefId {
        let detail = format!(
            "{}{} {}{}({})",
            if is_extern { "extern " } else { "" },
            self.text(f.ret.span),
            f.name.text,
            Self::type_params_detail(&f.type_params),
            self.params_detail(&f.params)
        );
        let id = self.define(&f.name, SymbolKind::Function, f.span, detail);
        self.declare_type_params(id, &f.type_params);
        self.declare_params(id, &f.params, true);
        id
    }

    // -----------------------------------------------------------------
    // Walk pass: resolve types and bodies.

    /// The def created for a declaration name by the declaration pass.
    fn def_for(&self, name: &Name) -> Option<DefId> {
        self.s.scopes[self.scope.0 as usize]
            .names
            .get(&name.text)?
            .iter()
            .copied()
            .find(|&d| self.s.def(d).name_span == Some(name.span))
    }

    /// Add a def's type parameters to the current scope.
    fn enter_type_params(&mut self, id: DefId) {
        for tp in self.s.def(id).type_params.clone() {
            self.add_to_scope(self.scope, tp);
        }
    }

    /// Resolve parameter types and add the parameters to the current scope.
    fn enter_params(&mut self, params: &[Param], ids: &[DefId]) {
        for (p, &id) in params.iter().zip(ids) {
            let ty = self.resolve_type(&p.ty);
            self.s.defs[id.0 as usize].ty = ty;
            if let Some(e) = &p.default {
                self.expr(e);
            }
            if !p.name.is_missing() {
                self.add_to_scope(self.scope, id);
            }
        }
    }

    fn walk_decl(&mut self, d: &Decl) {
        match d {
            Decl::Const(c) => {
                let ty = self.resolve_type(&c.ty);
                let value = self.expr(&c.value);
                self.check_assign(&ty, &value, &c.value);
                if let Some(id) = self.def_for(&c.name) {
                    self.s.defs[id.0 as usize].ty = ty;
                    if let Some(v) = self.const_eval(&c.value) {
                        if let Ok(v) = u128::try_from(v) {
                            self.s.const_values.insert(id, v);
                        }
                    }
                }
            }
            Decl::Var(v) => {
                let ty = self.resolve_type(&v.ty);
                if let Some(e) = &v.init {
                    let value = self.expr(e);
                    self.check_assign(&ty, &value, e);
                }
                if let Some(id) = self.def_for(&v.name) {
                    self.s.defs[id.0 as usize].ty = ty;
                }
            }
            Decl::Typedef(t) => {
                if let Some(inline) = &t.inline_decl {
                    self.walk_decl(inline);
                }
                let ty = self.resolve_type(&t.ty);
                if let Some(id) = self.def_for(&t.name) {
                    self.s.defs[id.0 as usize].ty = ty;
                }
            }
            Decl::Struct(s) => {
                let Some(id) = self.def_for(&s.name) else {
                    return;
                };
                self.s.defs[id.0 as usize].ty = Ty::Def(id, Vec::new());
                self.with_scope(Some(s.span), |r| {
                    r.enter_type_params(id);
                    let fields = r.s.def(id).members.clone();
                    for (f, fid) in s.fields.iter().zip(fields) {
                        let ty = r.resolve_type(&f.ty);
                        r.s.defs[fid.0 as usize].ty = ty;
                    }
                });
            }
            Decl::Enum(e) => {
                let Some(id) = self.def_for(&e.name) else {
                    return;
                };
                if let Some(t) = &e.repr {
                    let repr = self.resolve_type(t);
                    self.s.enum_reprs.insert(id, repr);
                }
                // Member values may refer to earlier members by bare name.
                self.with_scope(None, |r| {
                    let members = r.s.def(id).members.clone();
                    for (m, mid) in e.members.iter().zip(members) {
                        if let Some(v) = &m.value {
                            r.expr(v);
                        }
                        r.add_to_scope(r.scope, mid);
                    }
                });
                self.s.defs[id.0 as usize].ty = Ty::Def(id, Vec::new());
            }
            Decl::Errors(_) | Decl::MatchKinds(_) | Decl::Error(_) => {}
            Decl::Extern(e) => {
                let Some(id) = self
                    .lookup(&e.name.text, Want::Type, false)
                    .into_iter()
                    .find(|&d| self.s.def(d).kind == SymbolKind::Extern)
                else {
                    return;
                };
                self.s.defs[id.0 as usize].ty = Ty::Def(id, Vec::new());
                let methods = self.s.def(id).members.clone();
                self.with_scope(Some(e.span), |r| {
                    r.enter_type_params(id);
                    for (m, mid) in e.methods.iter().zip(methods) {
                        r.with_scope(Some(m.span), |r| {
                            r.enter_type_params(mid);
                            let ret = match &m.ret {
                                Some(t) => r.resolve_type(t),
                                None => Ty::Def(id, Vec::new()),
                            };
                            r.s.defs[mid.0 as usize].ty = ret;
                            let params = r.s.def(mid).params.clone();
                            r.enter_params(&m.params, &params);
                        });
                    }
                });
            }
            Decl::ExternFunction(f) => {
                let Some(id) = self.def_for(&f.name) else {
                    return;
                };
                self.with_scope(Some(f.span), |r| r.proto(id, f));
            }
            Decl::Function(f) => {
                let Some(id) = self.def_for(&f.proto.name) else {
                    return;
                };
                self.with_scope(Some(f.span), |r| {
                    r.proto(id, &f.proto);
                    r.with_parent(id, |r| r.body(&f.body));
                });
            }
            Decl::Action(a) => {
                let Some(id) = self.def_for(&a.name) else {
                    return;
                };
                self.with_scope(Some(a.span), |r| {
                    let params = r.s.def(id).params.clone();
                    r.enter_params(&a.params, &params);
                    r.with_parent(id, |r| r.body(&a.body));
                });
            }
            Decl::ParserType(b) | Decl::ControlType(b) | Decl::Package(b) => {
                let Some(id) = self.def_for(&b.name) else {
                    return;
                };
                self.s.defs[id.0 as usize].ty = Ty::Def(id, Vec::new());
                self.with_scope(Some(b.span), |r| {
                    r.enter_type_params(id);
                    let params = r.s.def(id).params.clone();
                    r.enter_params(&b.params, &params);
                });
            }
            Decl::Parser(p) => {
                let Some(id) = self.def_for(&p.name) else {
                    return;
                };
                self.s.defs[id.0 as usize].ty = Ty::Def(id, Vec::new());
                let saved_parser = self.parser.replace(id);
                self.with_scope(Some(p.span), |r| {
                    r.enter_type_params(id);
                    let params = r.s.def(id).params.clone();
                    r.enter_params(&p.params, &params);
                    let ctor = r.declare_params(id, &p.ctor_params, false);
                    r.enter_params(&p.ctor_params, &ctor);
                    r.with_scope(Some(p.span), |r| {
                        r.with_parent(id, |r| {
                            for l in &p.locals {
                                r.declare(l);
                            }
                            for s in &p.states {
                                r.define(
                                    &s.name,
                                    SymbolKind::State,
                                    s.span,
                                    format!("state {}", s.name.text),
                                );
                            }
                            r.pre_types(&p.locals);
                            for l in &p.locals {
                                r.walk_decl(l);
                            }
                            for s in &p.states {
                                r.state(s);
                            }
                        })
                    });
                });
                self.parser = saved_parser;
            }
            Decl::Control(c) => {
                let Some(id) = self.def_for(&c.name) else {
                    return;
                };
                self.s.defs[id.0 as usize].ty = Ty::Def(id, Vec::new());
                self.with_scope(Some(c.span), |r| {
                    r.enter_type_params(id);
                    let params = r.s.def(id).params.clone();
                    r.enter_params(&c.params, &params);
                    let ctor = r.declare_params(id, &c.ctor_params, false);
                    r.enter_params(&c.ctor_params, &ctor);
                    r.with_scope(Some(c.span), |r| {
                        r.with_parent(id, |r| {
                            for l in &c.locals {
                                r.declare(l);
                            }
                            r.pre_types(&c.locals);
                            for l in &c.locals {
                                r.walk_decl(l);
                            }
                            if let Some(apply) = &c.apply {
                                r.body(apply);
                            }
                        })
                    });
                });
            }
            Decl::Instantiation(i) => {
                let ty = self.resolve_type(&i.ty);
                let all = self.constructors(&ty);
                let span =
                    Span::new(i.ty.span.file, i.ty.span.end, i.name.span.start);
                if i.name.span.file == i.ty.span.file
                    && i.name.span.start >= i.ty.span.end
                {
                    self.call_site(span, all.clone(), &i.args);
                }
                let candidates = pick_overloads(&self.s, all, i.args.len());
                self.args(&i.args, &candidates);
                self.record_implementations(&candidates, &i.args);
                let id = self.def_for(&i.name);
                if let Some(id) = id {
                    self.s.defs[id.0 as usize].ty = ty.clone();
                }
                if let Some(init) = &i.initializer {
                    self.with_scope(Some(i.span), |r| {
                        let saved = r.parent;
                        r.parent = id.or(saved);
                        for d in init {
                            r.declare(d);
                        }
                        for d in init {
                            r.walk_decl(d);
                        }
                        r.parent = saved;
                    });
                }
            }
            Decl::ValueSet(v) => {
                self.resolve_type(&v.elem_ty);
                self.expr(&v.size);
                if let Some(id) = self.def_for(&v.name) {
                    self.s.defs[id.0 as usize].ty = Ty::Base;
                }
            }
            Decl::Table(t) => self.table(t),
        }
    }

    /// Constructors that could build a value of type `ty` with `argc`
    /// arguments, for signature help and named arguments.
    fn constructors(&self, ty: &Ty) -> Vec<DefId> {
        let Some(id) = self.s.type_def(ty) else {
            return Vec::new();
        };
        let def = self.s.def(id);
        match def.kind {
            SymbolKind::Extern => {
                let ctors: Vec<DefId> = def
                    .members
                    .iter()
                    .copied()
                    .filter(|&m| self.s.def(m).kind == SymbolKind::Function)
                    .collect();
                ctors
            }
            SymbolKind::Parser | SymbolKind::Control | SymbolKind::Package => {
                vec![id]
            }
            _ => Vec::new(),
        }
    }

    fn proto(&mut self, id: DefId, f: &FunctionProto) {
        self.enter_type_params(id);
        let ret = self.resolve_type(&f.ret);
        self.s.defs[id.0 as usize].ty = ret;
        let params = self.s.def(id).params.clone();
        self.enter_params(&f.params, &params);
    }

    fn state(&mut self, s: &ParserState) {
        self.with_scope(Some(s.span), |r| {
            for st in &s.body {
                r.stmt(st);
            }
            if let Some(t) = &s.transition {
                r.transition(t);
            }
        });
    }

    fn transition(&mut self, t: &Transition) {
        match &t.kind {
            TransitionKind::Direct(name) => self.state_ref(name),
            TransitionKind::Select(sel) => {
                let keys: Vec<Ty> =
                    sel.exprs.iter().map(|e| self.expr(e)).collect();
                for c in &sel.cases {
                    self.check_keyset(&c.keyset, &keys);
                    self.state_ref(&c.state);
                }
            }
        }
    }

    fn state_ref(&mut self, name: &Name) {
        if name.is_missing() {
            return;
        }
        let found = self
            .lookup(&name.text, Want::Value, false)
            .into_iter()
            .find(|&d| self.s.def(d).kind == SymbolKind::State);
        match found {
            Some(d) => self.add_ref(name.span, d, false),
            None => match name.text.as_str() {
                "accept" | "reject" => {
                    let b = self.s.builtins[name.text.as_str()];
                    self.add_ref(name.span, b, false);
                }
                _ => self.unresolved(name, "state", UnresolvedKind::State),
            },
        }
    }

    fn keyset(&mut self, k: &Keyset) {
        match &k.kind {
            KeysetKind::Expr(e) => {
                self.expr(e);
            }
            KeysetKind::Mask(a, b) | KeysetKind::Range(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            KeysetKind::Tuple(ks) => {
                for k in ks {
                    self.keyset(k);
                }
            }
            KeysetKind::Default | KeysetKind::DontCare => {}
        }
    }

    /// Resolve a `select` case or table entry and check it against the key
    /// types.
    fn check_keyset(&mut self, k: &Keyset, keys: &[Ty]) {
        let elems: Vec<&Keyset> = match &k.kind {
            KeysetKind::Tuple(ks) => ks.iter().collect(),
            _ => vec![k],
        };
        let mut values: Vec<Vec<(&Expr, Ty)>> = Vec::new();
        for e in &elems {
            let exprs: Vec<&Expr> = match &e.kind {
                KeysetKind::Expr(v) => vec![v],
                KeysetKind::Mask(a, b) | KeysetKind::Range(a, b) => vec![a, b],
                KeysetKind::Tuple(_) => {
                    self.keyset(e);
                    vec![]
                }
                KeysetKind::Default | KeysetKind::DontCare => vec![],
            };
            values.push(exprs.into_iter().map(|v| (v, self.expr(v))).collect());
        }
        if matches!(k.kind, KeysetKind::Default | KeysetKind::DontCare) {
            return;
        }
        if elems.len() != keys.len() {
            if matches!(k.kind, KeysetKind::Tuple(_)) && !keys.is_empty() {
                let msg = format!(
                    "expected {} key value{}, found {}",
                    keys.len(),
                    if keys.len() == 1 { "" } else { "s" },
                    elems.len()
                );
                self.report(Diagnostic::error(k.span, msg).with_code("type"));
            }
            return;
        }
        for (vs, key) in values.into_iter().zip(keys) {
            for (v, ty) in vs {
                self.check_assign(key, &ty, v);
            }
        }
    }

    fn table(&mut self, t: &TableDecl) {
        let id = self.def_for(&t.name);
        // The `actions` list first: other properties are checked against it.
        let mut listed: Option<(Vec<DefId>, Span)> = None;
        for p in &t.properties {
            if let PropertyValue::Actions(refs) = &p.value {
                let mut defs = Vec::new();
                for a in refs {
                    if let Some(&d) = self.action_ref(a, t).first() {
                        defs.push(d);
                    }
                }
                let text = self.db.slice(p.span);
                if let Some(close) = text.rfind('}') {
                    let at = p.span.start + close as u32;
                    listed = Some((defs, Span::new(p.span.file, at, at + 1)));
                }
            }
        }
        if let (Some(id), Some((defs, _))) = (id, &listed) {
            self.s.table_actions.insert(id, defs.clone());
        }
        // Key types, for checking entries (`key` precedes `entries`).
        let mut keys: Vec<Ty> = Vec::new();
        for p in &t.properties {
            match &p.value {
                PropertyValue::Key(elems) => {
                    for k in elems {
                        keys.push(self.expr(&k.expr));
                        self.match_kind_ref(&k.match_kind);
                    }
                }
                PropertyValue::Actions(_) => {}
                PropertyValue::Entries(entries) => {
                    for e in entries {
                        if let Some(p) = &e.priority {
                            self.expr(p);
                        }
                        self.check_keyset(&e.keyset, &keys);
                        let defs = self.action_ref(&e.action, t);
                        self.check_listed(&e.action.name.name, &defs, &listed);
                    }
                }
                PropertyValue::Expr(e) if p.name.text == "default_action" => {
                    let (path, args) = match &e.kind {
                        ExprKind::Path(path) => (path, None),
                        ExprKind::Call { callee, args, .. } => {
                            match &callee.kind {
                                ExprKind::Path(path) => (path, Some(args)),
                                _ => {
                                    self.expr(e);
                                    continue;
                                }
                            }
                        }
                        _ => {
                            self.expr(e);
                            continue;
                        }
                    };
                    let a = ActionRef {
                        annotations: Vec::new(),
                        name: path.clone(),
                        args: args.cloned(),
                        span: e.span,
                    };
                    let defs = self.action_ref(&a, t);
                    self.check_listed(&path.name, &defs, &listed);
                }
                PropertyValue::Expr(e) => {
                    self.expr(e);
                }
            }
        }
    }

    /// Report an action used by a table but missing from its `actions`.
    fn check_listed(
        &mut self,
        name: &Name,
        defs: &[DefId],
        listed: &Option<(Vec<DefId>, Span)>,
    ) {
        let (Some(&d), Some((list, list_end))) = (defs.first(), listed) else {
            return;
        };
        if self.s.def(d).kind != SymbolKind::Action || list.contains(&d) {
            return;
        }
        self.s.diagnostics.push(
            Diagnostic::error(
                name.span,
                format!("`{}` is not in the table's `actions` list", name.text),
            )
            .with_code("not-in-actions"),
        );
        self.s.not_in_actions.push(NotInActions {
            name: name.text.clone(),
            span: name.span,
            list_end: *list_end,
        });
    }

    /// The type and a parameter name for an argument, guessed for creating
    /// a missing action.
    fn guess_param(&mut self, i: usize, e: &Expr) -> (String, String) {
        let saved = std::mem::replace(&mut self.quiet, true);
        let (_, defs) = self.expr_inner(e);
        self.quiet = saved;
        let name = match &e.kind {
            ExprKind::Path(p) => p.name.text.clone(),
            ExprKind::Member { member, .. } => member.text.clone(),
            _ => format!("arg{i}"),
        };
        let ty = match (&e.kind, defs.first()) {
            (_, Some(d)) if self.s.type_text.contains_key(d) => {
                self.s.type_text[d].clone()
            }
            (
                ExprKind::Int(IntLit {
                    width: Some(w),
                    signed,
                    ..
                }),
                _,
            ) => {
                format!("{}<{w}>", if *signed { "int" } else { "bit" })
            }
            (ExprKind::Bool(_), _) => "bool".into(),
            _ => "bit<32>".into(),
        };
        (ty, name)
    }

    fn match_kind_ref(&mut self, name: &Name) {
        if name.is_missing() {
            return;
        }
        let found = self
            .lookup(&name.text, Want::Value, false)
            .into_iter()
            .find(|&d| self.s.def(d).kind == SymbolKind::MatchKind);
        match found {
            Some(d) => self.add_ref(name.span, d, false),
            None => {
                self.unresolved(name, "match kind", UnresolvedKind::MatchKind)
            }
        }
    }

    fn action_ref(&mut self, a: &ActionRef, table: &TableDecl) -> Vec<DefId> {
        let params = a
            .args
            .iter()
            .flatten()
            .enumerate()
            .map(|(i, arg)| self.guess_param(i, &arg.value))
            .collect();
        let saved = self.action_table.replace((table.name.span, params));
        let defs = self.path(&a.name, Want::Value);
        self.action_table = saved;
        if let Some(args) = &a.args {
            let span = Span::new(
                a.name.span.file,
                a.name.span.end,
                a.span.end.max(a.name.span.end),
            );
            self.call_site(span, defs.clone(), args);
            self.args(args, &defs);
        }
        defs
    }

    /// Record local declarations that nothing refers to. A declaration in a
    /// fragment included several times is unused only if every copy is.
    fn find_unused(&mut self) {
        let mut used: std::collections::HashSet<Span> = Default::default();
        for r in &self.s.refs {
            if let Some(s) = self.s.def(r.def).name_span {
                used.insert(s);
            }
        }
        let mut seen = std::collections::HashSet::new();
        for (i, d) in self.s.defs.iter().enumerate() {
            let Some(span) = d.name_span else { continue };
            let candidate = matches!(
                d.kind,
                SymbolKind::Action
                    | SymbolKind::Table
                    | SymbolKind::Instance
                    | SymbolKind::Variable
                    | SymbolKind::Constant
                    | SymbolKind::ValueSet
            ) && d.parent.is_some_and(|p| {
                matches!(
                    self.s.def(p).kind,
                    SymbolKind::Control
                        | SymbolKind::Parser
                        | SymbolKind::Action
                        | SymbolKind::Function
                )
            });
            if candidate
                && !d.name.is_empty()
                && !used.contains(&span)
                && seen.insert(span)
            {
                self.s.unused.push(DefId(i as u32));
                self.s.diagnostics.push(
                    Diagnostic::new(
                        span,
                        crate::diag::Severity::Hint,
                        format!("`{}` is never used", d.name),
                    )
                    .with_code("unused"),
                );
                self.s.diagnostics.last_mut().unwrap().unnecessary = true;
            }
        }
    }

    fn body(&mut self, b: &Block) {
        self.with_scope(Some(b.span), |r| {
            for s in &b.stmts {
                r.stmt(s);
            }
        });
    }

    fn local_decl(&mut self, d: &Decl) {
        let saved = self.local;
        self.local = true;
        self.declare(d);
        self.walk_decl(d);
        self.local = saved;
    }

    /// Resolve the types of declarations ahead of the walk, so that uses
    /// before the declaration (an action using a counter declared after
    /// it) still resolve members.
    fn pre_types(&mut self, decls: &[Decl]) {
        let saved = std::mem::replace(&mut self.quiet, true);
        for d in decls {
            let (name, ty) = match d {
                Decl::Const(c) => (&c.name, &c.ty),
                Decl::Var(v) => (&v.name, &v.ty),
                Decl::Instantiation(i) => (&i.name, &i.ty),
                _ => continue,
            };
            let t = self.resolve_type(ty);
            if let Some(id) = self.def_for(name) {
                self.s.defs[id.0 as usize].ty = t;
            }
        }
        self.quiet = saved;
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Decl(d) => self.local_decl(d),
            StmtKind::Assign { lhs, op, rhs } => {
                let target = self.lvalue(lhs);
                let value = self.expr(rhs);
                self.check_writable(lhs);
                match op {
                    AssignOp::Shl | AssignOp::Shr => {}
                    _ => self.check_assign(&target, &value, rhs),
                }
            }
            StmtKind::Call(e) | StmtKind::Expr(e) => {
                self.expr(e);
            }
            StmtKind::If { cond, then, els } => {
                let t = self.expr(cond);
                self.check_condition(&t, cond);
                self.stmt(then);
                if let Some(e) = els {
                    self.stmt(e);
                }
            }
            StmtKind::Block(b) => self.body(b),
            StmtKind::Switch { expr, cases } => {
                self.expr(expr);
                for c in cases {
                    if let SwitchLabel::Expr(e) = &c.label {
                        self.expr(e);
                    }
                    if let Some(b) = &c.body {
                        self.body(b);
                    }
                }
            }
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    self.expr(e);
                }
            }
            StmtKind::For {
                init,
                cond,
                update,
                body,
                ..
            } => {
                self.with_scope(Some(s.span), |r| {
                    for i in init {
                        r.stmt(i);
                    }
                    if let Some(c) = cond {
                        r.expr(c);
                    }
                    for u in update {
                        r.stmt(u);
                    }
                    r.stmt(body);
                });
            }
            StmtKind::ForIn {
                var,
                collection,
                body,
                ..
            } => {
                self.expr(collection);
                self.with_scope(Some(s.span), |r| {
                    r.local_decl(&Decl::Var(var.clone()));
                    r.stmt(body);
                });
            }
            StmtKind::Transition(t) => self.transition(t),
            StmtKind::Exit
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Empty
            | StmtKind::Error => {}
        }
    }

    fn lvalue(&mut self, e: &Expr) -> Ty {
        let start = self.s.refs.len();
        let ty = self.expr(e);
        // The written name is the one ending the l-value's base, below any
        // index or slice: `ttl` in `hdr.ipv4.ttl[7:0] = ...`.
        let mut base = e;
        while let ExprKind::Index { base: b, .. }
        | ExprKind::Slice { base: b, .. }
        | ExprKind::PlusSlice { base: b, .. } = &base.kind
        {
            base = b;
        }
        if let Some(r) = self.s.refs[start..].iter_mut().rev().find(|r| {
            r.span.file == base.span.file && r.span.end == base.span.end
        }) {
            r.write = true;
        }
        ty
    }

    // ----- Type checks -----
    //
    // Only certain errors are reported: both types must be fully known
    // fixed-width types (after following typedefs), so generic, enum,
    // and unresolved types never produce a diagnostic.

    fn report(&mut self, d: Diagnostic) {
        if !self.quiet {
            self.s.diagnostics.push(d);
        }
    }

    /// `(signed, width)` of a type that is `bit<W>` or `int<W>` with a
    /// known width.
    fn bits(&self, t: &Ty) -> Option<(bool, u32)> {
        match self.s.resolve_alias(t) {
            Ty::Bits {
                signed,
                width: Some(w),
            } => Some((signed, w)),
            _ => None,
        }
    }

    /// Whether a type is certainly not `bool`.
    fn not_bool(&self, t: &Ty) -> bool {
        matches!(
            self.s.resolve_alias(t),
            Ty::Bits { .. } | Ty::Int | Ty::String | Ty::Varbit(_)
        )
    }

    fn show_ty(&self, t: &Ty) -> String {
        match self.s.resolve_alias(t) {
            Ty::Bits {
                signed,
                width: Some(w),
            } => format!("{}<{w}>", if signed { "int" } else { "bit" }),
            Ty::Bool => "bool".into(),
            Ty::Int => "int".into(),
            Ty::String => "string".into(),
            Ty::Def(id, _) => self.s.def(id).name.clone(),
            _ => "?".into(),
        }
    }

    /// The struct, header, or header union a type names, after typedefs.
    fn data_type(&self, t: &Ty) -> Option<DefId> {
        match self.s.resolve_alias(t) {
            Ty::Def(id, _)
                if matches!(
                    self.s.def(id).kind,
                    SymbolKind::Struct
                        | SymbolKind::Header
                        | SymbolKind::HeaderUnion
                ) =>
            {
                Some(id)
            }
            _ => None,
        }
    }

    /// Whether a type is a scalar (`bit<W>`, `int<W>`, `int`, `bool`).
    fn scalar(&self, t: &Ty) -> bool {
        matches!(
            self.s.resolve_alias(t),
            Ty::Bits { width: Some(_), .. } | Ty::Int | Ty::Bool
        )
    }

    fn check_condition(&mut self, t: &Ty, cond: &Expr) {
        if self.not_bool(t) {
            let msg = format!(
                "condition must be `bool`, found `{}`",
                self.show_ty(t)
            );
            self.report(Diagnostic::error(cond.span, msg).with_code("type"));
        }
    }

    /// Check that `value` (the expression `e`) may be stored in a place of
    /// type `target` without a cast.
    fn check_assign(&mut self, target: &Ty, value: &Ty, e: &Expr) {
        let (t, v) =
            (self.s.resolve_alias(target), self.s.resolve_alias(value));
        let mismatch = match (&t, &v) {
            _ if matches!(e.kind, ExprKind::DontCare) => false,
            (Ty::Bits { .. }, Ty::Bits { .. }) => {
                matches!((self.bits(&t), self.bits(&v)), (Some(a), Some(b)) if a != b)
            }
            (Ty::Bits { width: Some(_), .. }, Ty::Bool) => true,
            (Ty::Def(..), Ty::Def(..))
                if self.data_type(&t).is_some()
                    && self.data_type(&v).is_some()
                    && self.data_type(&t) != self.data_type(&v) =>
            {
                true
            }
            _ if (self.data_type(&t).is_some() && self.scalar(&v))
                || (self.scalar(&t) && self.data_type(&v).is_some()) =>
            {
                true
            }
            (Ty::Bool, Ty::Bits { width: Some(_), .. } | Ty::Int) => true,
            (
                Ty::Bits {
                    signed: false,
                    width: Some(w),
                },
                Ty::Int,
            ) => {
                if let ExprKind::Int(IntLit {
                    value: Some(n),
                    width: None,
                    ..
                }) = &e.kind
                {
                    if *w < 128 && *n >> *w != 0 {
                        let msg = format!(
                            "`{}` does not fit in `bit<{w}>` and is truncated",
                            self.text(e.span)
                        );
                        self.report(
                            Diagnostic::warning(e.span, msg)
                                .with_code("overflow"),
                        );
                    }
                }
                false
            }
            _ => false,
        };
        if !mismatch {
            return;
        }
        let expected = self.show_ty(&t);
        let msg = format!(
            "mismatched types: expected `{expected}`, found `{}`",
            self.show_ty(&v)
        );
        let code = if self.bits(&t).is_some() && self.bits(&v).is_some() {
            "width"
        } else {
            "type"
        };
        self.report(Diagnostic::error(e.span, msg).with_code(code));
        if !self.quiet && self.bits(&t).is_some() {
            let compound = !matches!(
                e.kind,
                ExprKind::Path(_)
                    | ExprKind::Member { .. }
                    | ExprKind::Index { .. }
                    | ExprKind::Slice { .. }
                    | ExprKind::Int(_)
                    | ExprKind::Call { .. }
                    | ExprKind::Cast { .. }
            );
            self.s.mismatches.push(Mismatch {
                span: e.span,
                expected,
                compound,
            });
        }
    }

    /// The declaration an l-value is rooted at (`hdr` in `hdr.a[1].b`), or
    /// `Err(())` if the expression is certainly not assignable.
    fn lvalue_root(&self, e: &Expr) -> Result<Option<DefId>, ()> {
        let mut base = e;
        loop {
            match &base.kind {
                ExprKind::Member { base: b, .. }
                | ExprKind::Index { base: b, .. }
                | ExprKind::Slice { base: b, .. }
                | ExprKind::PlusSlice { base: b, .. } => base = b,
                ExprKind::Path(p) => {
                    let def = self
                        .s
                        .refs
                        .iter()
                        .rev()
                        .find(|r| r.span == p.name.span)
                        .map(|r| r.def);
                    return Ok(def);
                }
                ExprKind::Int(_)
                | ExprKind::Bool(_)
                | ExprKind::Str(_)
                | ExprKind::Binary { .. }
                | ExprKind::Unary { .. }
                | ExprKind::Cast { .. }
                | ExprKind::Ternary { .. }
                    if std::ptr::eq(base, e) =>
                {
                    return Err(())
                }
                _ => return Ok(None),
            }
        }
    }

    /// `packet_in.extract` takes a header and `packet_out.emit` a header,
    /// header union, struct, or stack of them.
    fn check_packet_call(
        &mut self,
        callee: DefId,
        args: &[Argument],
        tys: &[Ty],
    ) {
        let def = self.s.def(callee);
        let ext = def.parent.map(|p| self.s.def(p).name.as_str());
        let method = def.name.as_str();
        let extract = method == "extract" && ext == Some("packet_in");
        if !(extract || method == "emit" && ext == Some("packet_out")) {
            return;
        }
        let (Some(a), Some(ty)) = (args.first(), tys.first()) else {
            return;
        };
        let ty = self.s.resolve_alias(ty);
        let kind = self.s.type_def(&ty).map(|d| self.s.def(d).kind);
        let bad = if extract {
            matches!(ty, Ty::Stack(..))
                || matches!(
                    kind,
                    Some(SymbolKind::Struct | SymbolKind::HeaderUnion)
                )
        } else {
            false
        } || self.scalar(&ty)
            || matches!(ty, Ty::String | Ty::Varbit(_));
        if !bad {
            return;
        }
        let found = match (&ty, kind) {
            (Ty::Stack(..), _) => "a header stack".to_string(),
            (_, Some(SymbolKind::Struct)) => {
                format!("struct `{}`", self.show_ty(&ty))
            }
            (_, Some(SymbolKind::HeaderUnion)) => {
                format!("header union `{}`", self.show_ty(&ty))
            }
            _ => format!("`{}`", self.show_ty(&ty)),
        };
        let msg = if extract {
            let hint = if matches!(ty, Ty::Stack(..)) {
                " (use `.next` to extract into a stack)"
            } else {
                ""
            };
            format!("`extract` expects a header, found {found}{hint}")
        } else {
            format!(
                "`emit` expects a header, header union, struct, or header stack, found {found}"
            )
        };
        self.report(Diagnostic::error(a.value.span, msg).with_code("type"));
    }

    /// Record which blocks or instances are passed for the parser,
    /// control, and package parameters of a constructor call.
    fn record_implementations(
        &mut self,
        candidates: &[DefId],
        args: &[Argument],
    ) {
        let [ctor] = candidates[..] else { return };
        if !matches!(
            self.s.def(ctor).kind,
            SymbolKind::Package | SymbolKind::Parser | SymbolKind::Control
        ) {
            return;
        }
        let params = self.s.def(ctor).params.clone();
        for (i, a) in args.iter().enumerate() {
            let param = match &a.name {
                Some(n) => params
                    .iter()
                    .copied()
                    .find(|&p| self.s.def(p).name == n.text),
                None => params.get(i).copied(),
            };
            let Some(abs) =
                param.and_then(|p| self.s.type_def(&self.s.def(p).ty))
            else {
                continue;
            };
            if !matches!(
                self.s.def(abs).kind,
                SymbolKind::Package | SymbolKind::Parser | SymbolKind::Control
            ) {
                continue;
            }
            let name = match &a.value.kind {
                ExprKind::Call { callee, .. } => callee_name_span(callee),
                ExprKind::Path(p) => Some(p.name.span),
                _ => None,
            };
            let concrete = name.and_then(|n| {
                self.s
                    .refs
                    .iter()
                    .rev()
                    .find(|r| r.span == n)
                    .map(|r| r.def)
            });
            if let Some(c) = concrete {
                if c != abs {
                    self.s.implementations.push((abs, c));
                }
            }
        }
    }

    /// Every action and function named `name` in the enclosing scopes.
    fn visible_callables(&self, name: &str) -> Vec<DefId> {
        let mut out = Vec::new();
        let mut cur = Some(self.scope);
        while let Some(id) = cur {
            let scope = &self.s.scopes[id.0 as usize];
            if let Some(defs) = scope.names.get(name) {
                out.extend(defs.iter().copied().filter(|&d| {
                    matches!(
                        self.s.def(d).kind,
                        SymbolKind::Action | SymbolKind::Function
                    )
                }));
            }
            cur = scope.parent;
        }
        out
    }

    /// Why a declaration cannot be written, if it cannot.
    fn read_only(&self, d: DefId) -> Option<String> {
        let def = self.s.def(d);
        match def.kind {
            SymbolKind::Constant => {
                Some(format!("`{}` is a constant", def.name))
            }
            SymbolKind::Parameter => match self.s.directions.get(&d)? {
                Direction::In => {
                    Some(format!("`{}` is an `in` parameter", def.name))
                }
                Direction::None => {
                    Some(format!("`{}` is a directionless parameter", def.name))
                }
                Direction::Out | Direction::InOut => None,
            },
            _ => None,
        }
    }

    /// Report an assignment to a constant or read-only parameter.
    fn check_writable(&mut self, lhs: &Expr) {
        let Ok(Some(d)) = self.lvalue_root(lhs) else {
            return;
        };
        if let Some(why) = self.read_only(d) {
            let mut diag = Diagnostic::error(
                lhs.span,
                format!("cannot assign to `{}`: {why}", self.text(lhs.span)),
            )
            .with_code("read-only");
            if let Some(n) = self.s.def(d).name_span {
                diag = diag.with_related(n, "declared here");
            }
            self.report(diag);
        }
    }

    /// Check a call to an action, function, or method: the argument count,
    /// and for the one matching declaration, argument types and that `out`
    /// and `inout` arguments are assignable.
    fn check_call(
        &mut self,
        callee: &Expr,
        all: &[DefId],
        args: &[Argument],
        arg_tys: &[Ty],
    ) {
        let checkable = !all.is_empty()
            && all.iter().all(|&d| {
                let def = self.s.def(d);
                def.name_span.is_some()
                    && matches!(
                        def.kind,
                        SymbolKind::Action
                            | SymbolKind::Function
                            | SymbolKind::Method
                    )
            });
        if !checkable {
            return;
        }
        let n = args.len();
        let fitting: Vec<DefId> = all
            .iter()
            .copied()
            .filter(|&d| {
                let def = self.s.def(d);
                def.required_params <= n && n <= def.params.len()
            })
            .collect();
        let name = self.s.def(all[0]).name.clone();
        // A name may shadow another callable of the same name (an action
        // `hash` calling the extern function `hash`).
        let shadowed = matches!(callee.kind, ExprKind::Path(_))
            && self.visible_callables(&name).iter().any(|&d| {
                let def = self.s.def(d);
                !all.contains(&d)
                    && def.required_params <= n
                    && n <= def.params.len()
            });
        if fitting.is_empty() && !shadowed {
            let mut counts: Vec<String> = all
                .iter()
                .map(|&d| {
                    let def = self.s.def(d);
                    if def.required_params == def.params.len() {
                        def.params.len().to_string()
                    } else {
                        format!(
                            "{} to {}",
                            def.required_params,
                            def.params.len()
                        )
                    }
                })
                .collect();
            counts.sort();
            counts.dedup();
            let plural = |c: &str| if c == "1" { "" } else { "s" };
            let expected = match counts.as_slice() {
                [one] => format!("{one} argument{}", plural(one)),
                many => format!("{} arguments", many.join(" or ")),
            };
            let msg = format!(
                "`{name}` takes {expected} but {n} {} given",
                if n == 1 { "was" } else { "were" }
            );
            let mut diag =
                Diagnostic::error(callee.span, msg).with_code("arguments");
            if let Some(span) = self.s.def(all[0]).name_span {
                diag = diag.with_related(span, "declared here");
            }
            self.report(diag);
            return;
        }
        let [callee_def] = fitting[..] else { return };
        if shadowed {
            return;
        }
        self.check_packet_call(callee_def, args, arg_tys);
        let params = self.s.def(callee_def).params.clone();
        for (i, (a, ty)) in args.iter().zip(arg_tys).enumerate() {
            let param = match &a.name {
                Some(nm) => params
                    .iter()
                    .copied()
                    .find(|&p| self.s.def(p).name == nm.text),
                None if args.iter().all(|a| a.name.is_none()) => {
                    params.get(i).copied()
                }
                None => None,
            };
            let Some(p) = param else { continue };
            let pty = self.s.def(p).ty.clone();
            let dir = self.s.directions.get(&p).copied();
            if matches!(dir, Some(Direction::Out | Direction::InOut))
                && !matches!(a.value.kind, ExprKind::DontCare)
            {
                let pname = self.s.def(p).name.clone();
                let what = if dir == Some(Direction::Out) {
                    "out"
                } else {
                    "inout"
                };
                match self.lvalue_root(&a.value) {
                    Err(()) => {
                        let msg = format!(
                            "argument for `{what}` parameter `{pname}` must be assignable"
                        );
                        self.report(
                            Diagnostic::error(a.value.span, msg)
                                .with_code("read-only"),
                        );
                        continue;
                    }
                    Ok(Some(d)) => {
                        if let Some(why) = self.read_only(d) {
                            let msg = format!(
                                "argument for `{what}` parameter `{pname}` must be assignable: {why}"
                            );
                            self.report(
                                Diagnostic::error(a.value.span, msg)
                                    .with_code("read-only"),
                            );
                            continue;
                        }
                    }
                    Ok(None) => {}
                }
            }
            self.check_assign(&pty, ty, &a.value);
        }
    }

    fn path(&mut self, p: &Path, want: Want) -> Vec<DefId> {
        if p.name.is_missing() {
            return Vec::new();
        }
        let defs = self.lookup(&p.name.text, want, p.global);
        if let Some(&first) = defs.first() {
            self.add_ref(p.name.span, first, false);
        } else {
            let (what, kind) = if want == Want::Type {
                ("type", UnresolvedKind::Type)
            } else {
                ("name", UnresolvedKind::Name)
            };
            self.unresolved(&p.name, what, kind);
        }
        defs
    }

    /// A compile-time constant expression's value (literals, constants,
    /// and integer arithmetic on them), as P4 integers without widths.
    fn const_eval(&self, e: &Expr) -> Option<i128> {
        self.const_eval_depth(e, 0)
    }

    fn const_eval_depth(&self, e: &Expr, depth: u32) -> Option<i128> {
        if depth > 64 {
            return None;
        }
        let eval = |e: &Expr| self.const_eval_depth(e, depth + 1);
        match &e.kind {
            ExprKind::Int(lit) => {
                lit.value.and_then(|v| i128::try_from(v).ok())
            }
            ExprKind::Path(p) => {
                let d = *self
                    .lookup(&p.name.text, Want::Value, p.global)
                    .first()?;
                let v = *self.s.const_values.get(&d)?;
                i128::try_from(v).ok()
            }
            ExprKind::Cast { expr, .. } => eval(expr),
            ExprKind::Unary { op, expr } => {
                let v = eval(expr)?;
                match op {
                    UnOp::Neg => v.checked_neg(),
                    UnOp::Plus => Some(v),
                    _ => None,
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let (a, b) = (eval(lhs)?, eval(rhs)?);
                match op {
                    BinOp::Add => a.checked_add(b),
                    BinOp::Sub => a.checked_sub(b),
                    BinOp::Mul => a.checked_mul(b),
                    BinOp::Div => a.checked_div(b),
                    BinOp::Mod => a.checked_rem(b),
                    BinOp::Shl => a.checked_shl(u32::try_from(b).ok()?),
                    BinOp::Shr => a.checked_shr(u32::try_from(b).ok()?),
                    BinOp::BitAnd => Some(a & b),
                    BinOp::BitOr => Some(a | b),
                    BinOp::BitXor => Some(a ^ b),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// A constant width or size.
    fn const_u32(&self, e: &Expr) -> Option<u32> {
        self.const_eval(e).and_then(|v| u32::try_from(v).ok())
    }

    fn resolve_type(&mut self, t: &Type) -> Ty {
        match &t.kind {
            TypeKind::Bit(w) | TypeKind::Int(w) => {
                let signed = matches!(t.kind, TypeKind::Int(_));
                match w {
                    Some(w) => {
                        self.expr(w);
                        Ty::Bits {
                            signed,
                            width: self.const_u32(w),
                        }
                    }
                    None if signed => Ty::Int,
                    None => Ty::Bits {
                        signed,
                        width: Some(1),
                    },
                }
            }
            TypeKind::Varbit(w) => {
                self.expr(w);
                Ty::Varbit(self.const_u32(w))
            }
            TypeKind::Bool => Ty::Bool,
            TypeKind::String => Ty::String,
            TypeKind::Void | TypeKind::MatchKind => Ty::Base,
            TypeKind::ErrorType => Ty::Error,
            TypeKind::DontCare | TypeKind::Error => Ty::Unknown,
            TypeKind::Named { path, args } => {
                let defs = self.path(path, Want::Type);
                let args: Vec<Ty> =
                    args.iter().map(|a| self.resolve_type(a)).collect();
                match defs.first() {
                    Some(&d) if is_type_kind(self.s.def(d).kind) => {
                        if self.s.def(d).kind == SymbolKind::TypeParameter {
                            Ty::Def(d, Vec::new())
                        } else {
                            Ty::Def(d, args)
                        }
                    }
                    _ => Ty::Unknown,
                }
            }
            TypeKind::Stack { elem, size } => {
                let elem = self.resolve_type(elem);
                self.expr(size);
                Ty::Stack(Box::new(elem), self.const_u32(size))
            }
            TypeKind::Tuple(ts) => {
                Ty::Tuple(ts.iter().map(|t| self.resolve_type(t)).collect())
            }
            TypeKind::List(t) => {
                self.resolve_type(t);
                Ty::Base
            }
        }
    }

    fn args(&mut self, args: &[Argument], callees: &[DefId]) -> Vec<Ty> {
        let mut tys = Vec::with_capacity(args.len());
        for a in args {
            if let Some(name) = &a.name {
                let param = callees.iter().find_map(|&c| {
                    self.s
                        .def(c)
                        .params
                        .iter()
                        .copied()
                        .find(|&p| self.s.def(p).name == name.text)
                });
                if let Some(p) = param {
                    self.add_ref(name.span, p, false);
                }
            }
            tys.push(self.expr(&a.value));
        }
        tys
    }

    fn call_site(
        &mut self,
        span: Span,
        candidates: Vec<DefId>,
        args: &[Argument],
    ) {
        self.s.calls.push(CallSite {
            span,
            candidates,
            arg_spans: args.iter().map(|a| a.span).collect(),
        });
    }

    /// Resolve an expression and return its type.
    fn expr(&mut self, e: &Expr) -> Ty {
        self.expr_inner(e).0
    }

    /// Resolve an expression, returning its type and, for names and
    /// members, the declarations it refers to.
    fn expr_inner(&mut self, e: &Expr) -> (Ty, Vec<DefId>) {
        match &e.kind {
            ExprKind::Int(lit) => {
                let ty = match lit.width {
                    Some(w) => Ty::Bits {
                        signed: lit.signed,
                        width: Some(w),
                    },
                    None => Ty::Int,
                };
                (ty, Vec::new())
            }
            ExprKind::Bool(_) => (Ty::Bool, Vec::new()),
            ExprKind::Str(_) => (Ty::String, Vec::new()),
            ExprKind::Path(p) => {
                if !p.global && p.name.text == "error" {
                    return (Ty::TypeName(Box::new(Ty::Error)), Vec::new());
                }
                let defs = self.path(p, Want::Value);
                let ty =
                    defs.first().map_or(Ty::Unknown, |&d| self.value_ty(d));
                (ty, defs)
            }
            ExprKind::Member { base, member } => {
                let base_ty = self.expr(base);
                self.s.members.push(MemberSite {
                    span: member.span,
                    base: base_ty.clone(),
                });
                if member.is_missing() {
                    return (Ty::Unknown, Vec::new());
                }
                let candidates: Vec<(DefId, Ty)> = self
                    .s
                    .members_of(&base_ty)
                    .into_iter()
                    .filter(|(d, _)| self.s.def(*d).name == member.text)
                    .collect();
                if let Some((d, ty)) = candidates.first().cloned() {
                    self.add_ref(member.span, d, false);
                    // `p.apply(...)` on a parser or control takes the
                    // block's parameters.
                    if d == self.s.builtins["apply"] {
                        let block = self.s.type_def(&base_ty);
                        return (ty, block.into_iter().collect());
                    }
                    (ty, candidates.into_iter().map(|c| c.0).collect())
                } else {
                    if !self.quiet && self.s.members_complete(&base_ty) {
                        let tname = self
                            .s
                            .type_def(&base_ty)
                            .map(|d| self.s.def(d).name.clone());
                        let msg = match tname {
                            Some(t) => {
                                format!("no member `{}` in `{t}`", member.text)
                            }
                            None => format!("no member `{}`", member.text),
                        };
                        self.s.diagnostics.push(
                            Diagnostic::error(member.span, msg)
                                .with_code("unknown-member"),
                        );
                        self.s.unresolved.push(Unresolved {
                            name: member.text.clone(),
                            span: member.span,
                            kind: UnresolvedKind::Member(base_ty.clone()),
                            scope: self.scope,
                            parser: self.parser,
                        });
                    }
                    (Ty::Unknown, Vec::new())
                }
            }
            ExprKind::Index { base, index } => {
                let base = self.expr(base);
                self.expr(index);
                match self.s.resolve_alias(&base) {
                    Ty::Stack(elem, _) => (*elem, Vec::new()),
                    _ => (Ty::Unknown, Vec::new()),
                }
            }
            ExprKind::Slice { base, hi, lo } => {
                self.expr(base);
                self.expr(hi);
                self.expr(lo);
                let width = match (self.const_eval(hi), self.const_eval(lo)) {
                    (Some(h), Some(l)) if h >= l => {
                        u32::try_from(h - l + 1).ok()
                    }
                    _ => None,
                };
                (
                    Ty::Bits {
                        signed: false,
                        width,
                    },
                    Vec::new(),
                )
            }
            ExprKind::PlusSlice { base, start, width } => {
                self.expr(base);
                self.expr(start);
                self.expr(width);
                let width = self.const_u32(width);
                (
                    Ty::Bits {
                        signed: false,
                        width,
                    },
                    Vec::new(),
                )
            }
            ExprKind::Call {
                callee,
                type_args,
                args,
            } => {
                let (callee_ty, defs) = self.expr_inner(callee);
                let targs: Vec<Ty> =
                    type_args.iter().map(|t| self.resolve_type(t)).collect();
                let all = self.callables(&defs);
                let span = Span::new(
                    callee.span.file,
                    callee.span.end,
                    e.span.end.max(callee.span.end),
                );
                // Signature help offers every overload; navigation uses the
                // one matching the argument count.
                self.call_site(span, all.clone(), args);
                let candidates =
                    pick_overloads(&self.s, all.clone(), args.len());
                if let (Some(&best), Some(name)) =
                    (candidates.first(), callee_name_span(callee))
                {
                    if let Some(r) =
                        self.s.refs.iter_mut().rev().find(|r| r.span == name)
                    {
                        if self.s.defs[r.def.0 as usize].kind
                            == self.s.defs[best.0 as usize].kind
                        {
                            r.def = best;
                        }
                    }
                }
                let arg_tys = self.args(args, &candidates);
                self.check_call(callee, &all, args, &arg_tys);
                self.record_implementations(&candidates, args);
                let ty = match candidates.first() {
                    Some(&c) => {
                        let def = self.s.def(c);
                        match def.kind {
                            SymbolKind::Function | SymbolKind::Method => {
                                let env: Vec<(DefId, Ty)> = def
                                    .type_params
                                    .iter()
                                    .copied()
                                    .zip(targs)
                                    .collect();
                                let ty = def.ty.clone();
                                // Extern method return types are already
                                // substituted by `members_of`.
                                let ty = if def.kind == SymbolKind::Method
                                    && def.name_span.is_some()
                                {
                                    callee_ret_ty(&callee_ty, &ty)
                                } else {
                                    ty
                                };
                                subst(&ty, &env)
                            }
                            k if is_type_kind(k) => Ty::Def(c, Vec::new()),
                            _ => Ty::Base,
                        }
                    }
                    None => match &callee_ty {
                        Ty::TableApply(_) => callee_ty.clone(),
                        _ => Ty::Unknown,
                    },
                };
                // `t.apply()` has the table-apply type, carried by the
                // member type of `apply`.
                if let Ty::TableApply(_) = callee_ty {
                    return (callee_ty, Vec::new());
                }
                (ty, Vec::new())
            }
            ExprKind::Specialized { base, type_args } => {
                let r = self.expr_inner(base);
                for t in type_args {
                    self.resolve_type(t);
                }
                r
            }
            ExprKind::Cast { ty, expr } => {
                let t = self.resolve_type(ty);
                self.expr(expr);
                (t, Vec::new())
            }
            ExprKind::Unary { op, expr } => {
                let t = self.expr(expr);
                let ty = match op {
                    UnOp::Not => Ty::Bool,
                    _ => match self.s.resolve_alias(&t) {
                        t @ (Ty::Bits { .. } | Ty::Int) => t,
                        _ => Ty::Unknown,
                    },
                };
                (ty, Vec::new())
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let l = self.expr(lhs);
                let r = self.expr(rhs);
                (self.binary_ty(*op, &l, &r, e), Vec::new())
            }
            ExprKind::Ternary { cond, then, els } => {
                let c = self.expr(cond);
                self.check_condition(&c, cond);
                let t = self.expr(then);
                let f = self.expr(els);
                let t = self.s.resolve_alias(&t);
                let f = self.s.resolve_alias(&f);
                let ty = if t == f
                    || (f == Ty::Int && matches!(t, Ty::Bits { .. }))
                {
                    t
                } else if t == Ty::Int && matches!(f, Ty::Bits { .. }) {
                    f
                } else {
                    Ty::Unknown
                };
                (ty, Vec::new())
            }
            ExprKind::List(es) => {
                let ts = es.iter().map(|e| self.expr(e)).collect();
                (Ty::Tuple(ts), Vec::new())
            }
            ExprKind::Struct { fields, .. } => {
                for f in fields {
                    self.expr(&f.value);
                }
                (Ty::Unknown, Vec::new())
            }
            ExprKind::Invalid
            | ExprKind::Dots
            | ExprKind::This
            | ExprKind::DontCare
            | ExprKind::Error => (Ty::Unknown, Vec::new()),
        }
    }

    /// The type of a binary expression, reporting operands of different
    /// fixed widths.
    fn binary_ty(&mut self, op: BinOp, l: &Ty, r: &Ty, e: &Expr) -> Ty {
        let (l, r) = (self.s.resolve_alias(l), self.s.resolve_alias(r));
        let bits = |t: &Ty| matches!(t, Ty::Bits { .. });
        // Operands of arithmetic, bitwise, and comparison operators must
        // have the same type, with unsized integers adapting.
        let same = || {
            if l == r {
                Some(l.clone())
            } else if l == Ty::Int && bits(&r) {
                Some(r.clone())
            } else if r == Ty::Int && bits(&l) {
                Some(l.clone())
            } else {
                None
            }
        };
        let check = |s: &mut Self| {
            if let (Some(a), Some(b)) = (s.bits(&l), s.bits(&r)) {
                if a != b {
                    let msg = format!(
                        "mismatched operand types: `{}` and `{}`",
                        s.show_ty(&l),
                        s.show_ty(&r)
                    );
                    s.report(Diagnostic::error(e.span, msg).with_code("width"));
                }
            }
        };
        match op {
            BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Mod
            | BinOp::AddSat
            | BinOp::SubSat
            | BinOp::BitAnd
            | BinOp::BitOr
            | BinOp::BitXor => {
                check(self);
                same()
                    .filter(|t| bits(t) || *t == Ty::Int)
                    .unwrap_or(Ty::Unknown)
            }
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge => {
                check(self);
                Ty::Bool
            }
            BinOp::And | BinOp::Or => Ty::Bool,
            BinOp::Shl | BinOp::Shr => match l {
                Ty::Bits { .. } | Ty::Int => l,
                _ => Ty::Unknown,
            },
            BinOp::Concat => match (self.bits(&l), self.bits(&r)) {
                (Some((signed, a)), Some((_, b))) => Ty::Bits {
                    signed,
                    width: a.checked_add(b),
                },
                _ => Ty::Unknown,
            },
            BinOp::Range => Ty::Unknown,
        }
    }

    /// The type of a declaration used as a value.
    fn value_ty(&self, d: DefId) -> Ty {
        let def = self.s.def(d);
        match def.kind {
            k if is_type_kind(k) => {
                Ty::TypeName(Box::new(Ty::Def(d, Vec::new())))
            }
            SymbolKind::Table => Ty::Def(d, Vec::new()),
            SymbolKind::Function | SymbolKind::Action | SymbolKind::Method => {
                Ty::Unknown
            }
            _ => def.ty.clone(),
        }
    }

    /// Declarations a call may invoke: functions/actions/methods by name
    /// (filtered by arity), or constructors of a type.
    fn callables(&self, defs: &[DefId]) -> Vec<DefId> {
        if let Some(&first) = defs.first() {
            if is_type_kind(self.s.def(first).kind) {
                return self.constructors(&Ty::Def(first, Vec::new()));
            }
            let callable: Vec<DefId> = defs
                .iter()
                .copied()
                .filter(|&d| {
                    matches!(
                        self.s.def(d).kind,
                        SymbolKind::Function
                            | SymbolKind::Method
                            | SymbolKind::Action
                    )
                })
                .collect();
            return callable;
        }
        Vec::new()
    }
}

/// The span of the name a callee expression ends with.
fn callee_name_span(callee: &Expr) -> Option<Span> {
    match &callee.kind {
        ExprKind::Path(p) => Some(p.name.span),
        ExprKind::Member { member, .. } => Some(member.span),
        ExprKind::Specialized { base, .. } => callee_name_span(base),
        _ => None,
    }
}

/// For a method call, the return type as computed by `members_of` (which
/// substitutes the extern's type arguments) is carried in the callee type.
fn callee_ret_ty(callee_ty: &Ty, declared: &Ty) -> Ty {
    match callee_ty {
        Ty::Unknown => declared.clone(),
        t => t.clone(),
    }
}

/// Overloads accepting `argc` arguments, or all of them if none do.
fn pick_overloads(s: &Sema, defs: Vec<DefId>, argc: usize) -> Vec<DefId> {
    let fitting: Vec<DefId> = defs
        .iter()
        .copied()
        .filter(|&d| {
            let def = s.def(d);
            def.required_params <= argc && argc <= def.params.len()
        })
        .collect();
    if fitting.is_empty() {
        defs
    } else {
        fitting
    }
}
