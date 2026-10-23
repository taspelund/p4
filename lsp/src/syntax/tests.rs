// Copyright 2026 Oxide Computer Company

use super::ast::*;
use super::{parse, Parse};
use crate::lexer::lex;
use crate::source::{FileId, Span};

fn parse_src(src: &str) -> Parse {
    parse(&lex(FileId(0), src).tokens)
}

fn clean(src: &str) -> Program {
    let p = parse_src(src);
    let msgs: Vec<_> = p.diagnostics.iter().map(|d| &d.message).collect();
    assert!(msgs.is_empty(), "unexpected diagnostics: {msgs:?}");
    p.program
}

fn text(src: &str, span: Span) -> &str {
    &src[span.start as usize..span.end as usize]
}

/// Statements of the body of the first action in `src`.
fn action_body(prog: &Program) -> &[Stmt] {
    for d in &prog.decls {
        if let Decl::Action(a) = d {
            return &a.body.stmts;
        }
    }
    panic!("no action");
}

/// Parses `e` as the right-hand side of an assignment.
fn rhs(prefix: &str, e: &str) -> String {
    let src = format!("{prefix}\naction a() {{ x = {e}; }}");
    let prog = clean(&src);
    match &action_body(&prog)[0].kind {
        StmtKind::Assign { rhs, .. } => show(rhs),
        k => panic!("not an assignment: {k:?}"),
    }
}

fn show_ty(t: &Type) -> String {
    match &t.kind {
        TypeKind::Bit(Some(w)) => format!("bit<{}>", show(w)),
        TypeKind::Int(Some(w)) => format!("int<{}>", show(w)),
        TypeKind::Bool => "bool".into(),
        TypeKind::Named { path, args } if args.is_empty() => {
            path.name.text.clone()
        }
        TypeKind::Named { path, args } => {
            let a: Vec<_> = args.iter().map(show_ty).collect();
            format!("{}<{}>", path.name.text, a.join(","))
        }
        TypeKind::Stack { elem, size } => {
            format!("{}[{}]", show_ty(elem), show(size))
        }
        k => format!("{k:?}"),
    }
}

/// A compact s-expression rendering of an expression's shape.
fn show(e: &Expr) -> String {
    let bin = |op: BinOp| match op {
        BinOp::Mul => "*",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::Eq => "==",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Concat => "++",
        _ => "?op",
    };
    match &e.kind {
        ExprKind::Int(i) => i.text.clone(),
        ExprKind::Bool(b) => b.to_string(),
        ExprKind::Path(p) => p.name.text.clone(),
        ExprKind::Member { base, member } => {
            format!("(. {} {})", show(base), member.text)
        }
        ExprKind::Index { base, index } => {
            format!("(idx {} {})", show(base), show(index))
        }
        ExprKind::Slice { base, hi, lo } => {
            format!("(slice {} {} {})", show(base), show(hi), show(lo))
        }
        ExprKind::PlusSlice { base, start, width } => {
            format!("(+: {} {} {})", show(base), show(start), show(width))
        }
        ExprKind::Call {
            callee,
            type_args,
            args,
        } => {
            let mut s = format!("(call {}", show(callee));
            if !type_args.is_empty() {
                let t: Vec<_> = type_args.iter().map(show_ty).collect();
                s += &format!(" <{}>", t.join(","));
            }
            for a in args {
                s += " ";
                if let Some(n) = &a.name {
                    s += &format!("{}=", n.text);
                }
                s += &show(&a.value);
            }
            s + ")"
        }
        ExprKind::Cast { ty, expr } => {
            format!("(cast {} {})", show_ty(ty), show(expr))
        }
        ExprKind::Unary { op, expr } => format!("({op:?} {})", show(expr)),
        ExprKind::Binary { op, lhs, rhs } => {
            format!("({} {} {})", bin(*op), show(lhs), show(rhs))
        }
        ExprKind::Ternary { cond, then, els } => {
            format!("(? {} {} {})", show(cond), show(then), show(els))
        }
        ExprKind::List(v) => {
            let v: Vec<_> = v.iter().map(show).collect();
            format!("{{{}}}", v.join(" "))
        }
        ExprKind::Struct { fields, dots } => {
            let mut v: Vec<_> = fields
                .iter()
                .map(|f| format!("{}={}", f.name.text, show(&f.value)))
                .collect();
            if *dots {
                v.push("...".into());
            }
            format!("{{{}}}", v.join(" "))
        }
        ExprKind::Invalid => "{#}".into(),
        ExprKind::DontCare => "_".into(),
        k => format!("{k:?}"),
    }
}

#[test]
fn binary_precedence_differs_from_c() {
    // In P4, `&` binds tighter than `==`, and `==` looser than `<`.
    assert_eq!(rhs("", "a + b * c == d & e"), "(== (+ a (* b c)) (& d e))");
    assert_eq!(
        rhs("", "a < b == c || d && e"),
        "(|| (== (< a b) c) (&& d e))"
    );
    assert_eq!(rhs("", "a ++ b << 2"), "(<< (++ a b) 2)");
    assert_eq!(rhs("", "a | b & c"), "(| a (& b c))");
}

#[test]
fn associativity_and_unary() {
    assert_eq!(rhs("", "a - b - c"), "(- (- a b) c)");
    assert_eq!(rhs("", "a ? b : c ? d : e"), "(? a b (? c d e))");
    assert_eq!(rhs("", "-a.b * c"), "(* (Neg (. a b)) c)");
    assert_eq!(rhs("", "!a && b"), "(&& (Not a) b)");
}

#[test]
fn shift_vs_nested_generic_close() {
    let src = "control c() {\n  register<bit<32>, tuple<bit<8>>>(16) r;\n  \
               apply { x = a >> 2; x >>= 1; y = a >= b; z = a > b; }\n}";
    let prog = clean(src);
    let Decl::Control(c) = &prog.decls[0] else {
        panic!()
    };
    let Decl::Instantiation(i) = &c.locals[0] else {
        panic!()
    };
    let TypeKind::Named { args, .. } = &i.ty.kind else {
        panic!()
    };
    assert_eq!(args.len(), 2);
    assert_eq!(text(src, i.ty.span), "register<bit<32>, tuple<bit<8>>>");
    let stmts = &c.apply.as_ref().unwrap().stmts;
    let ops: Vec<_> = stmts
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Assign { op, rhs, .. } => format!("{op:?} {}", show(rhs)),
            k => panic!("{k:?}"),
        })
        .collect();
    assert_eq!(
        ops,
        [
            "Assign (>> a 2)",
            "Shr 1",
            "Assign (>= a b)",
            "Assign (> a b)"
        ]
    );
}

#[test]
fn generic_call_vs_less_than() {
    assert_eq!(rhs("", "f<bit<8>>(y)"), "(call f <bit<8>> y)");
    assert_eq!(rhs("", "p.lookahead<H>()"), "(call (. p lookahead) <H>)");
    assert_eq!(rhs("", "a < b && c > (d)"), "(&& (< a b) (> c d))");
    assert_eq!(rhs("", "a < b"), "(< a b)");
}

#[test]
fn casts_vs_parentheses() {
    let td = "typedef bit<8> T;";
    assert_eq!(rhs(td, "(bit<8>) y + 1"), "(+ (cast bit<8> y) 1)");
    assert_eq!(rhs(td, "(T) -1"), "(cast T (Neg 1))");
    assert_eq!(rhs(td, "(a) - 1"), "(- a 1)");
    assert_eq!(rhs(td, "(hdr.f) + 1"), "(+ (. hdr f) 1)");
    assert_eq!(rhs(td, "(T)(y)"), "(cast T y)");
    assert_eq!(rhs(td, "(H[2]){ {#}, {#} }"), "(cast H[2] {{#} {#}})");
}

#[test]
fn slices_indices_and_compound_expressions() {
    assert_eq!(rhs("", "y[7:4]"), "(slice y 7 4)");
    assert_eq!(rhs("", "y[i +: 8]"), "(+: y i 8)");
    assert_eq!(rhs("", "s[0].f[3:0]"), "(slice (. (idx s 0) f) 3 0)");
    assert_eq!(rhs("", "{a, b + 1}"), "{a (+ b 1)}");
    assert_eq!(rhs("", "{ f = 1, g = 2, ... }"), "{f=1 g=2 ...}");
    assert_eq!(rhs("", "f(a = 1, b = _)"), "(call f a=1 b=_)");
    assert_eq!(rhs("", "error.NoError"), "(. error NoError)");
}

#[test]
fn integer_literals() {
    let src = "action a() { x = 8w0xff; x = 16s5; x = 0b1010; x = 1_000; }";
    let prog = clean(src);
    let lits: Vec<_> = action_body(&prog)
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Assign {
                rhs:
                    Expr {
                        kind: ExprKind::Int(i),
                        ..
                    },
                ..
            } => (i.value, i.width, i.signed),
            k => panic!("{k:?}"),
        })
        .collect();
    assert_eq!(
        lits,
        [
            (Some(255), Some(8), false),
            (Some(5), Some(16), true),
            (Some(10), None, false),
            (Some(1000), None, false),
        ]
    );
}

#[test]
fn member_expression_name_spans() {
    let src = "action a() { hdr.ipv4.ttl = 1; }";
    let prog = clean(src);
    let StmtKind::Assign { lhs, .. } = &action_body(&prog)[0].kind else {
        panic!()
    };
    let ExprKind::Member { base, member } = &lhs.kind else {
        panic!()
    };
    assert_eq!(text(src, member.span), "ttl");
    assert_eq!(text(src, lhs.span), "hdr.ipv4.ttl");
    let ExprKind::Member { base, member } = &base.kind else {
        panic!()
    };
    assert_eq!(text(src, member.span), "ipv4");
    let ExprKind::Path(p) = &base.kind else {
        panic!()
    };
    assert_eq!(text(src, p.name.span), "hdr");
}

#[test]
fn switch_statement_labels() {
    let src = "control c() { apply { switch (t.apply().action_run) {\n\
               a1: a2: { x = 1; }\n E.B: {}\n default: { exit; }\n} } }";
    let prog = clean(src);
    let Decl::Control(c) = &prog.decls[0] else {
        panic!()
    };
    let StmtKind::Switch { expr, cases } =
        &c.apply.as_ref().unwrap().stmts[0].kind
    else {
        panic!()
    };
    assert_eq!(show(expr), "(. (call (. t apply)) action_run)");
    let labels: Vec<_> = cases
        .iter()
        .map(|c| match &c.label {
            SwitchLabel::Default(_) => format!("default {}", c.body.is_some()),
            SwitchLabel::Expr(e) => format!("{} {}", show(e), c.body.is_some()),
        })
        .collect();
    assert_eq!(
        labels,
        ["a1 false", "a2 true", "(. E B) true", "default true"]
    );
}

#[test]
fn table_properties_and_entries() {
    let src = r#"control c() {
  table t {
    key = { h.a: exact @name("a"); h.b & 0xf : ternary; }
    actions = { a1; @defaultonly a2(3); .NoAction; }
    const default_action = a2(1);
    size = 1024;
    implementation = action_profile(32w128);
    const entries = {
      (1, _) : a1();
      priority = 10 : (0x1 &&& 0xf, 2 .. 5) : a2(4) @name("e");
      default : a1;
    }
  }
  apply { t.apply(); }
}"#;
    let prog = clean(src);
    let Decl::Control(c) = &prog.decls[0] else {
        panic!()
    };
    let Decl::Table(t) = &c.locals[0] else {
        panic!()
    };
    let names: Vec<_> =
        t.properties.iter().map(|p| p.name.text.as_str()).collect();
    assert_eq!(
        names,
        [
            "key",
            "actions",
            "default_action",
            "size",
            "implementation",
            "entries"
        ]
    );
    let PropertyValue::Key(keys) = &t.properties[0].value else {
        panic!()
    };
    assert_eq!(keys[0].match_kind.text, "exact");
    assert_eq!(keys[0].annotations[0].name.text, "name");
    assert_eq!(show(&keys[1].expr), "(& (. h b) 0xf)");
    let PropertyValue::Actions(acts) = &t.properties[1].value else {
        panic!()
    };
    assert_eq!(acts[1].annotations[0].name.text, "defaultonly");
    assert_eq!(acts[1].args.as_ref().map(|a| a.len()), Some(1));
    assert!(acts[2].name.global);
    assert!(t.properties[2].is_const);
    assert!(t.properties[5].is_const);
    let PropertyValue::Entries(entries) = &t.properties[5].value else {
        panic!()
    };
    assert_eq!(entries.len(), 3);
    let KeysetKind::Tuple(k) = &entries[0].keyset.kind else {
        panic!()
    };
    assert_eq!(k[1].kind, KeysetKind::DontCare);
    let e1 = &entries[1];
    assert_eq!(e1.priority.as_ref().map(show).as_deref(), Some("10"));
    let KeysetKind::Tuple(k) = &e1.keyset.kind else {
        panic!()
    };
    assert!(matches!(k[0].kind, KeysetKind::Mask(..)));
    assert!(matches!(k[1].kind, KeysetKind::Range(..)));
    assert_eq!(e1.action.name.name.text, "a2");
    assert_eq!(e1.annotations[0].name.text, "name");
    assert_eq!(entries[2].keyset.kind, KeysetKind::Default);
}

#[test]
fn enums_and_type_declarations() {
    let src = "enum bit<8> E { A = 1, B = 2, }\nenum F { X, Y }\n\
               header H<T> { T f; bit<(4*8)> g; varbit<32> v; }\n\
               typedef H<bit<8>>[4] Stack;\ntype bit<9> Port;\n\
               error { E1, E2 }\nmatch_kind { m1 }";
    let prog = clean(src);
    let Decl::Enum(e) = &prog.decls[0] else {
        panic!()
    };
    assert_eq!(e.repr.as_ref().map(show_ty).as_deref(), Some("bit<8>"));
    let members: Vec<_> = e
        .members
        .iter()
        .map(|m| (m.name.text.as_str(), m.value.as_ref().map(show)))
        .collect();
    assert_eq!(members, [("A", Some("1".into())), ("B", Some("2".into()))]);
    let Decl::Enum(f) = &prog.decls[1] else {
        panic!()
    };
    assert!(f.repr.is_none());
    let Decl::Struct(h) = &prog.decls[2] else {
        panic!()
    };
    assert_eq!(h.kind, StructKind::Header);
    assert_eq!(h.type_params[0].text, "T");
    assert_eq!(show_ty(&h.fields[1].ty), "bit<(* 4 8)>");
    let Decl::Typedef(t) = &prog.decls[3] else {
        panic!()
    };
    assert_eq!(show_ty(&t.ty), "H<bit<8>>[4]");
    assert!(
        matches!(&prog.decls[4], Decl::Typedef(t) if t.kind == TypedefKind::Type)
    );
    let Decl::Errors(errs) = &prog.decls[5] else {
        panic!()
    };
    assert_eq!(errs.members.len(), 2);
    assert!(matches!(prog.decls[6], Decl::MatchKinds(_)));
}

#[test]
fn parser_states_and_select_keysets() {
    let src = "parser P(packet_in pkt, out H h)(bit<8> k) {\n\
               value_set<bit<16>>(4) pvs;\n Checksum() ck;\n\
               state start { pkt.extract(h.e); transition select(h.e.t, h.e.u) {\n\
               (0x800, _): ipv4;\n (pvs, 1 .. 3): s2;\n (0x10 &&& 0xf0, default): s3;\n\
               default: accept; } }\n\
               state ipv4 { transition reject; }\n}";
    let prog = clean(src);
    let Decl::Parser(p) = &prog.decls[0] else {
        panic!()
    };
    assert_eq!(p.params.len(), 2);
    assert_eq!(p.ctor_params[0].name.text, "k");
    assert!(matches!(p.locals[0], Decl::ValueSet(_)));
    assert!(matches!(p.locals[1], Decl::Instantiation(_)));
    assert_eq!(p.states.len(), 2);
    let Some(Transition {
        kind: TransitionKind::Select(sel),
        ..
    }) = &p.states[0].transition
    else {
        panic!()
    };
    assert_eq!(sel.exprs.len(), 2);
    let kinds: Vec<_> = sel
        .cases
        .iter()
        .map(|c| {
            let k = match &c.keyset.kind {
                KeysetKind::Tuple(v) => v
                    .iter()
                    .map(|k| match &k.kind {
                        KeysetKind::Expr(e) => show(e),
                        KeysetKind::Mask(..) => "mask".into(),
                        KeysetKind::Range(..) => "range".into(),
                        KeysetKind::Default => "default".into(),
                        KeysetKind::DontCare => "_".into(),
                        KeysetKind::Tuple(_) => "tuple".into(),
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                KeysetKind::Default => "default".into(),
                k => format!("{k:?}"),
            };
            format!("{k}:{}", c.state.text)
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "0x800,_:ipv4",
            "pvs,range:s2",
            "mask,default:s3",
            "default:accept"
        ]
    );
    let Some(Transition {
        kind: TransitionKind::Direct(n),
        ..
    }) = &p.states[1].transition
    else {
        panic!()
    };
    assert_eq!(n.text, "reject");
}

#[test]
fn externs_instantiations_and_initializers() {
    let src = "extern Reg<T, I> {\n Reg(bit<32> size);\n T read<U>(in I i);\n\
               abstract void cb(inout T x);\n}\n\
               extern void f<T>(in T x);\n\
               package Pkg<H>(P<H> p);\n\
               Reg<bit<8>, bit<4>>(16) r = {\n void cb(inout bit<8> x) { x = 1; }\n};\n\
               Pkg(MyP()) main;";
    let prog = clean(src);
    let Decl::Extern(e) = &prog.decls[0] else {
        panic!()
    };
    assert_eq!(e.type_params.len(), 2);
    assert!(e.methods[0].is_constructor());
    assert_eq!(e.methods[1].type_params[0].text, "U");
    assert!(e.methods[2].is_abstract);
    assert!(
        matches!(&prog.decls[1], Decl::ExternFunction(f) if f.name.text == "f")
    );
    assert!(matches!(&prog.decls[2], Decl::Package(_)));
    let Decl::Instantiation(r) = &prog.decls[3] else {
        panic!()
    };
    let init = r.initializer.as_ref().unwrap();
    assert!(matches!(&init[0], Decl::Function(f) if f.proto.name.text == "cb"));
    let Decl::Instantiation(m) = &prog.decls[4] else {
        panic!()
    };
    assert_eq!(m.name.text, "main");
    assert_eq!(show(&m.args[0].value), "(call MyP)");
}

#[test]
fn local_declarations_and_loops() {
    let src = "action a() {\n bit<8> x = 1;\n const bit<8> y = 2;\n\
               Checksum16() ck;\n H[2] s;\n f<bit<8>>(x);\n\
               for (bit<8> i = 0, j = 1; i < 4; i = i + 1, j = j + 1) { }\n\
               for (bit<8> v in 1 .. 4) { continue; }\n\
               for (H h in s) { break; }\n}";
    let prog = clean(src);
    let kinds: Vec<_> = action_body(&prog)
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Decl(Decl::Var(v)) => format!("var {}", v.name.text),
            StmtKind::Decl(Decl::Const(_)) => "const".into(),
            StmtKind::Decl(Decl::Instantiation(_)) => "inst".into(),
            StmtKind::Call(_) => "call".into(),
            StmtKind::For { init, update, .. } => {
                format!("for {} {}", init.len(), update.len())
            }
            StmtKind::ForIn { collection, .. } => {
                format!("forin {}", show(collection))
            }
            k => format!("{k:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "var x",
            "const",
            "inst",
            "var s",
            "call",
            "for 2 2",
            "forin (?op 1 4)",
            "forin s"
        ]
    );
}

#[test]
fn contextual_keywords_as_names() {
    let src = "struct S { bit<8> key; bit<8> state; bit<8> type; \
               bit<8> entries; bit<8> priority; bit<8> list; bit<8> apply; }\n\
               action a(bit<8> actions) { s.key = actions; s.apply = 1; }";
    let prog = clean(src);
    let Decl::Struct(s) = &prog.decls[0] else {
        panic!()
    };
    let names: Vec<_> = s.fields.iter().map(|f| f.name.text.as_str()).collect();
    assert_eq!(
        names,
        ["key", "state", "type", "entries", "priority", "list", "apply"]
    );
    assert_eq!(action_body(&prog).len(), 2);
}

#[test]
fn annotations_are_retained() {
    let src = "@name(\"x\") @hidden @size[a = 1, b = \"c\"]\n\
               action a(@optional in bit<8> p) { }";
    let prog = clean(src);
    let Decl::Action(a) = &prog.decls[0] else {
        panic!()
    };
    let annos: Vec<_> = a
        .annotations
        .iter()
        .map(|an| (an.name.text.as_str(), an.kind, an.body.len()))
        .collect();
    assert_eq!(
        annos,
        [
            ("name", AnnotationKind::Unstructured, 1),
            ("hidden", AnnotationKind::Plain, 0),
            ("size", AnnotationKind::Structured, 7),
        ]
    );
    assert_eq!(text(src, a.annotations[0].body_span.unwrap()), "\"x\"");
    assert_eq!(a.params[0].annotations[0].name.text, "optional");
    assert_eq!(text(src, a.span), src);
}

#[test]
fn missing_semicolon_reported_after_previous_token() {
    let src = "action a() {\n  x = 1\n  y = 2;\n}";
    let p = parse_src(src);
    assert_eq!(p.diagnostics.len(), 1);
    let d = &p.diagnostics[0];
    assert_eq!(d.message, "expected `;` after statement");
    let after_one = src.find("1\n").unwrap() as u32 + 1;
    assert_eq!((d.span.start, d.span.end), (after_one, after_one));
    assert_eq!(action_body(&p.program).len(), 2);
}

#[test]
fn garbage_in_apply_keeps_sibling_declarations() {
    let src = "control c() {\n action a() { }\n table t { actions = { a; } }\n\
               apply {\n x = = ;\n t.apply();\n }\n action late() { }\n}\n\
               control d() { apply { } }";
    let p = parse_src(src);
    assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
    assert_eq!(p.diagnostics[0].message, "expected expression, found `=`");
    let Decl::Control(c) = &p.program.decls[0] else {
        panic!()
    };
    let locals: Vec<_> = c.locals.iter().filter_map(|d| d.name()).collect();
    let locals: Vec<_> = locals.iter().map(|n| n.text.as_str()).collect();
    assert_eq!(locals, ["a", "t", "late"]);
    let apply = c.apply.as_ref().unwrap();
    assert!(matches!(apply.stmts[1].kind, StmtKind::Call(_)));
    assert!(
        matches!(&p.program.decls[1], Decl::Control(d) if d.name.text == "d")
    );
}

#[test]
fn unclosed_brace_does_not_swallow_next_declaration() {
    let src = "control c() {\n apply {\n x = 1;\n}\n\
               header h_t { bit<8> f; }\n\
               control d() {\n action a() { x = 1;\n\
               table t { key = { x: exact; } }\n apply { }\n}";
    let p = parse_src(src);
    let names: Vec<_> = p
        .program
        .decls
        .iter()
        .filter_map(|d| d.name().map(|n| n.text.as_str()))
        .collect();
    assert_eq!(names, ["c", "h_t", "d"]);
    assert_eq!(p.diagnostics.len(), 2, "{:?}", p.diagnostics);
    // The control's `{` is unclosed; the error points at it.
    let d = &p.diagnostics[0];
    assert!(d.message.starts_with("expected `}`"), "{}", d.message);
    assert_eq!(d.related[0].0.start, src.find('{').unwrap() as u32);
    // The action's body ends at `table`, which cannot appear in it.
    let Decl::Control(c) = &p.program.decls[2] else {
        panic!()
    };
    let locals: Vec<_> = c.locals.iter().filter_map(|d| d.name()).collect();
    assert_eq!(locals.len(), 2);
    assert_eq!(locals[1].text, "t");
    assert!(c.apply.is_some());
}

#[test]
fn x4c_nested_transitions() {
    let src = "parser p() { state start {\n\
               if (x) { transition a; } else { transition b; }\n} }";
    let prog = clean(src);
    let Decl::Parser(p) = &prog.decls[0] else {
        panic!()
    };
    let StmtKind::If { then, .. } = &p.states[0].body[0].kind else {
        panic!()
    };
    let StmtKind::Block(b) = &then.kind else {
        panic!()
    };
    assert!(matches!(b.stmts[0].kind, StmtKind::Transition(_)));
    // Outside parser states, `transition` ends the block.
    assert!(!parse_src("action a() { transition s; }")
        .diagnostics
        .is_empty());
}

#[test]
fn errors_at_end_of_input_and_empty_input() {
    assert!(parse_src("").diagnostics.is_empty());
    let src = "control c() { apply { x = ";
    let p = parse_src(src);
    let d = &p.diagnostics[0];
    assert_eq!(d.message, "expected expression, found end of file");
    assert_eq!(d.span.start as usize, src.trim_end().len());
    let p = parse_src("action a() { x = }");
    assert_eq!(p.diagnostics[0].message, "expected expression, found `}`");
    // Token soup terminates.
    let p = parse_src(") ] } > @ . .. { ( [ < ; = control");
    assert!(!p.diagnostics.is_empty());
}

#[test]
fn spliced_tokens_from_several_files() {
    // An include spliced into a control body: the parse is unaffected and
    // spans do not mix files.
    let mut toks = lex(FileId(0), "control c() {\n").tokens;
    toks.extend(lex(FileId(1), "action a() { }\n").tokens);
    toks.extend(lex(FileId(0), "apply { a(); } }").tokens);
    let p = parse(&toks);
    assert!(p.diagnostics.is_empty());
    let Decl::Control(c) = &p.program.decls[0] else {
        panic!()
    };
    assert_eq!(c.span.file, FileId(0));
    assert_eq!(c.locals[0].span().file, FileId(1));
}

#[test]
fn overlong_chains_are_cut_off_with_one_error() {
    // Chains are parsed iteratively but build trees as deep as they are
    // long; later recursive passes would overflow the stack on these.
    for expr in [
        vec!["1"; 5000].join(" + "),
        format!("a{}", ".b".repeat(5000)),
        format!("f{}", "()".repeat(5000)),
        format!("{}1{}", "1 ? ".repeat(5000), " : 1".repeat(5000)),
    ] {
        let src = format!("const int x = {expr};\nconst int y = 1;");
        let p = parse_src(&src);
        let msgs: Vec<_> = p.diagnostics.iter().map(|d| &d.message).collect();
        assert_eq!(msgs, ["expression is too long"]);
        assert_eq!(p.program.decls.len(), 2);
    }
}
