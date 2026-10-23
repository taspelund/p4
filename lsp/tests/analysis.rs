// Copyright 2026 Oxide Computer Company

//! Editor queries over in-memory programs. Cursor positions are marked with
//! `$0` (or `$1`, `$2`, ...) in fixture text.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use x4c_lsp::analysis::{Analysis, HighlightKind, SymbolKind};
use x4c_lsp::preprocess::Options;
use x4c_lsp::source::{FileId, SourceDb, Span};

struct Fixture {
    a: Analysis,
    /// Marker name -> (file, offset).
    marks: HashMap<String, (FileId, u32)>,
}

const CORE: &str = "
error { NoError, PacketTooShort }
extern packet_in {
    void extract<T>(out T hdr);
    T lookahead<T>();
}
match_kind { exact, ternary, lpm }
action NoAction() {}
";

/// Files as `(path, text)`; the first is the root. Paths are rooted at
/// `/t/`. `core.p4` is always available.
fn fixture(files: &[(&str, &str)]) -> Fixture {
    let mut overlays = HashMap::new();
    let mut raw_marks = Vec::new();
    let mut all = vec![("core.p4", CORE)];
    all.extend_from_slice(files);
    for (name, text) in &all {
        let path = PathBuf::from(format!("/t/{name}"));
        let mut clean = String::new();
        let mut rest = *text;
        while let Some(i) = rest.find('$') {
            clean.push_str(&rest[..i]);
            let digit = rest[i + 1..].chars().next().unwrap();
            raw_marks.push((
                path.clone(),
                digit.to_string(),
                clean.len() as u32,
            ));
            rest = &rest[i + 2..];
        }
        clean.push_str(rest);
        overlays.insert(path, Arc::<str>::from(clean));
    }
    let mut db = SourceDb::new(overlays);
    let root = db
        .load(&PathBuf::from(format!("/t/{}", files[0].0)))
        .unwrap();
    let opts = Options {
        include_dirs: vec![PathBuf::from("/t")],
        defines: vec![],
    };
    let a = Analysis::new(db, root, &opts);
    let marks = raw_marks
        .into_iter()
        .map(|(path, name, off)| {
            let file = a.db.file_id(&path).expect("marked file was loaded");
            (name, (file, off))
        })
        .collect();
    Fixture { a, marks }
}

impl Fixture {
    fn at(&self, mark: &str) -> (FileId, u32) {
        self.marks[mark]
    }

    /// `file:line: text-of-line` for a span, to compare readably.
    fn show(&self, span: Span) -> String {
        let path = self
            .a
            .db
            .path(span.file)
            .file_name()
            .unwrap()
            .to_string_lossy();
        let text = self.a.db.text(span.file);
        let line = text[..span.start as usize].matches('\n').count() + 1;
        format!("{path}:{line}:{}", self.a.db.slice(span))
    }

    fn definition(&self, mark: &str) -> Vec<String> {
        let (f, o) = self.at(mark);
        self.a
            .goto_definition(f, o)
            .into_iter()
            .map(|s| self.show(s))
            .collect()
    }

    fn errors(&self) -> Vec<String> {
        self.a
            .diagnostics()
            .iter()
            .filter(|d| d.severity == x4c_lsp::diag::Severity::Error)
            .map(|d| format!("{}: {}", self.show(d.span), d.message))
            .collect()
    }
}

const HEADERS: &str = "
typedef bit<8> ttl_t;
header ipv4_h {
    ttl_t ttl; // time to live
    bit<32> dst;
}
struct headers_t {
    ipv4_h ipv4;
    ipv4_h[2] stack;
}
";

#[test]
fn definition_through_include_fragment_and_members() {
    // The fragment is spliced into a control body, so its names resolve
    // against the control's parameters, and member types cross three files.
    let f = fixture(&[
        (
            "main.p4",
            "#include <core.p4>
#include \"headers.p4\"
control ingress(inout headers_t hdr) {
    #include \"frag.p4\"
    apply { set_ttl(); }
}
",
        ),
        (
            "frag.p4",
            "action set_ttl() {
    h$0dr.ip$1v4.t$2tl = 1;
    hdr.stack.l$3ast.dst = 2;
    hdr.stack[0].d$4st = 3;
}
",
        ),
        ("headers.p4", HEADERS),
    ]);
    assert_eq!(f.errors(), Vec::<String>::new());
    assert_eq!(f.definition("0"), ["main.p4:3:hdr"]);
    assert_eq!(f.definition("1"), ["headers.p4:8:ipv4"]);
    assert_eq!(f.definition("2"), ["headers.p4:4:ttl"]);
    // Header stack built-ins have no declaration, but their element type
    // carries through.
    assert_eq!(f.definition("3"), Vec::<String>::new());
    assert_eq!(f.definition("4"), ["headers.p4:5:dst"]);
}

#[test]
fn fragment_included_twice_offers_both_declarations() {
    let f = fixture(&[
        (
            "main.p4",
            "#include \"headers.p4\"
control a(inout headers_t hdr) {
    #include \"frag.p4\"
    apply { }
}
control b(inout headers_t hdr) {
    #include \"frag.p4\"
    apply { }
}
",
        ),
        ("frag.p4", "action f() { h$0dr.ipv4.ttl = 1; }\n"),
        ("headers.p4", HEADERS),
    ]);
    assert_eq!(f.errors(), Vec::<String>::new());
    assert_eq!(f.definition("0"), ["main.p4:2:hdr", "main.p4:6:hdr"]);
}

#[test]
fn generic_extern_methods_tables_and_forward_locals() {
    let f = fixture(&[
        (
            "main.p4",
            "#include <core.p4>
#include \"headers.p4\"
extern Register<T, I> {
    Register(bit<32> size);
    T read(in I index);
}
control c(packet_in pkt, inout headers_t hdr) {
    action count() { reg.r$0ead(0); }
    Register<ipv4_h, bit<32>>(4) reg;
    table t {
        key = { hdr.ipv4.dst: e$1xact; }
        actions = { cou$2nt; NoAction; }
        default_action = NoAction();
    }
    apply {
        pkt.ex$3tract(hdr.ipv4);
        reg.read(0).t$4tl = 1;
        pkt.lookahead<ipv4_h>().d$5st = 1;
        if (t.apply().h$6it) { }
        switch (t.apply().action_run) {
            co$7unt: { }
            default: { }
        }
    }
}
",
        ),
        ("headers.p4", HEADERS),
    ]);
    assert_eq!(f.errors(), Vec::<String>::new());
    // `reg` is declared after the action that uses it.
    assert_eq!(f.definition("0"), ["main.p4:5:read"]);
    assert_eq!(f.definition("1"), ["core.p4:7:exact"]);
    assert_eq!(f.definition("2"), ["main.p4:8:count"]);
    assert_eq!(f.definition("3"), ["core.p4:4:extract"]);
    // `T` is substituted from the instance's type arguments...
    assert_eq!(f.definition("4"), ["headers.p4:4:ttl"]);
    // ...and from the call's type arguments.
    assert_eq!(f.definition("5"), ["headers.p4:5:dst"]);
    let (file, off) = f.at("6");
    let hover = f.a.hover(file, off).unwrap();
    assert!(hover.markdown.contains("bool hit"), "{}", hover.markdown);
    assert_eq!(f.definition("7"), ["main.p4:8:count"]);
}

#[test]
fn diagnostics_are_precise() {
    let f = fixture(&[
        (
            "main.p4",
            "#include <core.p4>
#include \"headers.p4\"
control c(packet_in pkt, inout headers_t hdr) {
    action a() {
        hdr.ipv4.nope = 1;
        pkt.anything();
        missing = 1;
    }
    action a() { }
    apply { transition_to_nowhere(); }
}
parser p(packet_in pkt) {
    state start { transition select(8w1) { 1: next; default: accept; } }
}
",
        ),
        ("headers.p4", HEADERS),
    ]);
    let errors = f.errors();
    assert_eq!(
        errors,
        [
            "main.p4:5:nope: no member `nope` in `ipv4_h`",
            "main.p4:7:missing: cannot find name `missing` in this scope",
            "main.p4:9:a: `a` is already declared in this scope",
            "main.p4:10:transition_to_nowhere: cannot find name \
             `transition_to_nowhere` in this scope",
            "main.p4:13:next: cannot find state `next` in this scope",
        ],
        // `pkt.anything()` is not reported: externs may be declared in
        // headers we cannot see the full shape of.
    );
    let dup =
        f.a.diagnostics()
            .iter()
            .find(|d| d.message.contains("already"))
            .unwrap();
    assert_eq!(f.show(dup.related[0].0), "main.p4:4:a");
}

#[test]
fn missing_include_suppresses_cascading_errors() {
    let f = fixture(&[(
        "main.p4",
        "#include <tna.p4>
control c(inout ingress_intrinsic_metadata_t md) { apply { md.port = 1; } }
",
    )]);
    let errors = f.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("tna.p4"), "{errors:?}");
}

#[test]
fn x4c_builtins_yield_to_declarations() {
    // x4c programs use NoAction and match kinds without declaring them;
    // a declared NoAction wins.
    let f = fixture(&[(
        "main.p4",
        "control c() {
    table t { key = { 8w1: exact; } actions = { NoAction; } }
    apply { }
}
SoftNPU(c()) main;
action No$0Action() {}
control d() { table t { actions = { NoAct$1ion; } } apply { } }
",
    )]);
    assert_eq!(f.errors(), Vec::<String>::new());
    assert_eq!(f.definition("1"), ["main.p4:6:NoAction"]);
}

#[test]
fn hover_shows_signature_container_and_docs() {
    let f = fixture(&[
        (
            "main.p4",
            "#include \"headers.p4\"
// Forward to a port.
// Second line.
action fwd(bit<9> port, in bool flag) { }

control c(inout headers_t hdr) {
    apply { f$0wd(1, true); hdr.ipv4.t$1tl = 0; }
}
",
        ),
        ("headers.p4", HEADERS),
    ]);
    let (file, off) = f.at("0");
    let h = f.a.hover(file, off).unwrap();
    assert_eq!(
        h.markdown,
        "```p4\naction fwd(bit<9> port, in bool flag)\n```\n\n---\n\n\
         Forward to a port.\nSecond line."
    );
    assert_eq!(f.show(h.span), "main.p4:7:fwd");
    let (file, off) = f.at("1");
    let h = f.a.hover(file, off).unwrap();
    assert_eq!(
        h.markdown,
        "```p4\nttl_t ttl\n```\n\nin `header ipv4_h`\n\n---\n\ntime to live"
    );
}

#[test]
fn references_highlights_and_rename() {
    let f = fixture(&[
        (
            "main.p4",
            "#include \"headers.p4\"
#define TTL hdr.ipv4.ttl
control c(inout headers_t h$0dr) {
    action a() { hdr.ipv4.ttl = hdr.ipv4.ttl + 1; }
    apply { a(); TTL = 1; }
}
",
        ),
        ("headers.p4", HEADERS),
    ]);
    let (file, off) = f.at("0");
    let refs: Vec<_> =
        f.a.references(file, off, true)
            .into_iter()
            .map(|s| f.show(s))
            .collect();
    // The use inside the macro expansion is not a reference to the text
    // `hdr` at the invocation site.
    assert_eq!(refs, ["main.p4:3:hdr", "main.p4:4:hdr", "main.p4:4:hdr"]);
    let hl = f.a.document_highlight(file, off);
    assert_eq!(
        hl.iter()
            .filter(|(_, k)| *k == HighlightKind::Write)
            .count(),
        1
    );
    // `hdr` is also used through `TTL`, so renaming it would miss that use.
    let err = f.a.rename(file, off, "headers").unwrap_err();
    assert!(err.contains("macro"), "{err}");

    // The field is written once (left of `=`) and read once in `a`.
    let text = f.a.db.text(file);
    let ttl = text.find("ttl =").unwrap() as u32;
    let hl = f.a.document_highlight(file, ttl);
    let kinds: Vec<_> = hl
        .iter()
        .filter(|(s, _)| s.file == file)
        .map(|(_, k)| *k)
        .collect();
    assert_eq!(kinds, [HighlightKind::Write, HighlightKind::Read]);

    let edits = f.a.rename(file, ttl, "hops").unwrap_err();
    assert!(edits.contains("macro"), "{edits}");
    let dst = fixture(&[
        (
            "main.p4",
            "#include \"headers.p4\"
control c(inout headers_t hdr) { apply { hdr.ipv4.d$0st = 1; } }
",
        ),
        ("headers.p4", HEADERS),
    ]);
    let (file, off) = dst.at("0");
    let mut edits: Vec<_> = dst
        .a
        .rename(file, off, "dest")
        .unwrap()
        .into_iter()
        .map(|(s, t)| format!("{} -> {t}", dst.show(s)))
        .collect();
    edits.sort();
    assert_eq!(edits, ["headers.p4:5:dst -> dest", "main.p4:2:dst -> dest"]);
    assert!(dst.a.rename(file, off, "bit").is_err());
}

#[test]
fn completion_contexts() {
    let f = fixture(&[
        (
            "main.p4",
            "#include \"headers.p4\"
action other(bit<8> unrelated) { }
control c(inout headers_t hdr) {
    action a(bit<8> arg) {
        bit<8> local = 0;
        $0
        hdr.$1
        hdr.ipv4 = hdr.ipv4;
    }
    apply { hdr.ipv4.$2 }
}
",
        ),
        ("headers.p4", HEADERS),
    ]);
    let labels = |m: &str| {
        let (file, off) = f.at(m);
        let mut l: Vec<_> =
            f.a.completions(file, off)
                .into_iter()
                .map(|c| c.label)
                .collect();
        l.sort();
        l
    };
    let scope = labels("0");
    for want in ["arg", "local", "hdr", "a", "other", "headers_t", "control"] {
        assert!(
            scope.contains(&want.to_string()),
            "missing {want}: {scope:?}"
        );
    }
    assert!(!scope.contains(&"unrelated".to_string()), "{scope:?}");
    // The parser reads `hdr.` followed by the next line's `hdr` as one
    // member access; completion still offers the struct's fields.
    assert_eq!(
        labels("1"),
        [
            "ipv4",
            "maxSizeInBits",
            "maxSizeInBytes",
            "minSizeInBits",
            "minSizeInBytes",
            "stack"
        ]
    );
    let header = labels("2");
    assert!(
        header.contains(&"ttl".to_string())
            && header.contains(&"isValid".to_string()),
        "{header:?}"
    );
}

#[test]
fn signature_help_tracks_nesting_and_overloads() {
    let f = fixture(&[(
        "main.p4",
        "extern void f(in bit<8> a, in bit<8> b, in bit<8> c);
extern void g(in bit<8> x);
extern void g(in bit<8> x, in bit<8> y);
control c() {
    apply {
        f(1, (2 + g(3$0)), $1);
        g(1, $2);
    }
}
",
    )]);
    let (file, off) = f.at("0");
    let h = f.a.signature_help(file, off).unwrap();
    // Inside the inner call to `g`, both overloads are offered.
    assert_eq!(h.signatures.len(), 2, "{h:?}");
    assert_eq!(h.active_param, 0);
    let (file, off) = f.at("1");
    let h = f.a.signature_help(file, off).unwrap();
    assert_eq!(
        h.signatures[0].label,
        "extern void f(in bit<8> a, in bit<8> b, in bit<8> c)"
    );
    assert_eq!(h.active_param, 2);
    let (s, e) = h.signatures[0].params[2];
    assert_eq!(
        &h.signatures[0].label[s as usize..e as usize],
        "in bit<8> c"
    );
    let (file, off) = f.at("2");
    let h = f.a.signature_help(file, off).unwrap();
    assert_eq!(h.active_param, 1);
    assert_eq!(h.signatures[h.active_signature as usize].params.len(), 2);
}

#[test]
fn preprocessor_navigation_and_symbols() {
    let f = fixture(&[
        (
            "main.p4",
            "#include \"head$0ers.p4\"
// Width of a port.
#define PORT_BITS 9
control c(inout headers_t hdr) {
    action a(bit<PORT_$1BITS> p) { }
    table t { actions = { a; } }
    apply { }
}
",
        ),
        ("headers.p4", HEADERS),
    ]);
    assert_eq!(f.definition("0"), ["headers.p4:1:"]);
    assert_eq!(f.definition("1"), ["main.p4:3:PORT_BITS"]);
    let (file, off) = f.at("1");
    let h = f.a.hover(file, off).unwrap();
    assert_eq!(
        h.markdown,
        "```c\n#define PORT_BITS 9\n```\n\n---\n\nWidth of a port."
    );

    let syms = f.a.document_symbols(file);
    let names: Vec<_> =
        syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    assert_eq!(
        names,
        [("PORT_BITS", SymbolKind::Macro), ("c", SymbolKind::Control)]
    );
    let children: Vec<_> =
        syms[1].children.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(children, ["a", "t"]);
    let ws: Vec<_> =
        f.a.workspace_symbols("ipv")
            .into_iter()
            .map(|s| s.name)
            .collect();
    assert_eq!(ws, ["ipv4_h", "ipv4"]);
}

/// Inputs nested or chained far beyond any real program are cut off with
/// one error each, instead of overflowing the stack (which aborts the
/// server) or flooding the editor with diagnostics.
#[test]
fn pathological_nesting_is_bounded() {
    let n = 20_000;
    let cases = [
        (
            format!("control C() {{ apply {{ a{} = 1; }} }}", "[0]".repeat(n)),
            "expression is too long",
        ),
        (
            format!(
                "control C() {{ apply {}{} }}",
                "{".repeat(n),
                "}".repeat(n)
            ),
            "nesting is too deep",
        ),
        (
            format!("const int x = {}1;", "-".repeat(n)),
            "nesting is too deep",
        ),
        (
            format!("#if {}1\n#endif\n", "1 ? 1 : ".repeat(n)),
            "#if expression is nested too deeply",
        ),
        (
            format!(
                "#define F(x) (x)\nconst int x = {}1{};",
                "F(".repeat(1000),
                ")".repeat(1000)
            ),
            "macro arguments are nested too deeply",
        ),
    ];
    for (text, expected) in cases {
        let f = fixture(&[("main.p4", &text)]);
        let errors = f.errors();
        assert!(
            errors.iter().any(|e| e.contains(expected)) && errors.len() <= 3,
            "{expected}: {:?}",
            &errors[..errors.len().min(5)]
        );
    }
}
