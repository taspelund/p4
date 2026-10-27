// Copyright 2026 Oxide Computer Company

//! An index of the declarations in headers on the include path, for
//! importing names from headers a program does not include yet.
//!
//! Each header is preprocessed and parsed on its own, without splicing its
//! includes, so that conditional blocks are honored and per-file results
//! can be cached by modification time and length. The include closure of a
//! header is computed from its `#include` directives, including those in
//! inactive conditional blocks (`tna.p4` includes different files depending
//! on `__TARGET_TOFINO__`).

use crate::analysis::{Analysis, SymbolKind};
use crate::lexer::{self, TokenKind};
use crate::preprocess::{self, Options};
use crate::source::{FileId, SourceDb, Span};
use crate::syntax::{self, ast::Decl, ast::StructKind};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// How many levels of subdirectories below an include directory to scan.
const MAX_DEPTH: usize = 3;
/// Candidates returned by [`HeaderIndex::lookup`].
const MAX_CANDIDATES: usize = 3;
/// Names returned by [`HeaderIndex::complete`].
const MAX_COMPLETIONS: usize = 200;
/// Maximum length of [`Candidate::detail`] in bytes.
const MAX_DETAIL: usize = 120;

/// A header that declares a name, as it would be written in `#include <...>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The path relative to the include directory, e.g. `core.p4` or
    /// `tofino/stratum.p4`.
    pub include: String,
    /// The candidate header itself.
    pub header: PathBuf,
    /// The declaring file (which may be a header the candidate includes).
    pub file: PathBuf,
    pub kind: SymbolKind,
    /// One-line rendering of the declaration.
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct HeaderIndex {
    /// Candidates per name, best first.
    by_name: HashMap<String, Vec<Candidate>>,
    /// `(lowercased name, name)`, sorted.
    names: Vec<(String, String)>,
}

impl HeaderIndex {
    /// The index for a list of include directories, cached across calls and
    /// refreshed when header files change.
    pub fn for_dirs(dirs: &[PathBuf]) -> Arc<HeaderIndex> {
        static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
        let scanned = scan(dirs);
        let snapshot: Snapshot =
            scanned.iter().map(|s| (s.path.clone(), s.stamp)).collect();
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((snap, index)) = cache.indexes.get(dirs) {
            if *snap == snapshot {
                return index.clone();
            }
        }
        let mut infos = Vec::with_capacity(scanned.len());
        for s in &scanned {
            let info = match cache.files.get(&s.path) {
                Some(info) if info.stamp == s.stamp => Some(info.clone()),
                _ => analyze_file(&s.path, s.stamp).map(Arc::new),
            };
            if let Some(info) = &info {
                cache.files.insert(s.path.clone(), info.clone());
            }
            infos.push(info);
        }
        let index = Arc::new(build(dirs, &scanned, &infos));
        cache
            .indexes
            .insert(dirs.to_vec(), (snapshot, index.clone()));
        index
    }

    /// Headers to include for `name`, best first.
    pub fn lookup(&self, name: &str) -> Vec<Candidate> {
        self.lookup_near(name, &[])
    }

    /// Like [`lookup`](Self::lookup), but headers in one of the `near`
    /// directories (where the program's existing includes live) come first,
    /// so a Tofino program gets Tofino headers.
    pub fn lookup_near(&self, name: &str, near: &[PathBuf]) -> Vec<Candidate> {
        let Some(all) = self.by_name.get(name) else {
            return Vec::new();
        };
        let is_near = |c: &&Candidate| {
            c.header
                .parent()
                .is_some_and(|d| near.iter().any(|n| n == d))
        };
        // Near candidates first, each group in its ranked order.
        all.iter()
            .filter(is_near)
            .chain(all.iter().filter(|c| !is_near(c)))
            .take(MAX_CANDIDATES)
            .cloned()
            .collect()
    }

    /// Names starting with `prefix` (case-insensitively), with their best
    /// candidate (see [`lookup_near`](Self::lookup_near)), for completion.
    pub fn complete(
        &self,
        prefix: &str,
        near: &[PathBuf],
    ) -> Vec<(String, Candidate)> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let prefix = prefix.to_lowercase();
        let start = self.names.partition_point(|(l, _)| *l < prefix);
        self.names[start..]
            .iter()
            .take_while(|(l, _)| l.starts_with(&prefix))
            .filter_map(|(_, n)| {
                let c = self.lookup_near(n, near).into_iter().next()?;
                Some((n.clone(), c))
            })
            .take(MAX_COMPLETIONS)
            .collect()
    }
}

/// File identity for cache validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
}

#[derive(Debug)]
struct DeclInfo {
    name: String,
    kind: SymbolKind,
    detail: String,
}

/// What one header declares and includes, independent of other files.
#[derive(Debug)]
struct FileInfo {
    stamp: Stamp,
    decls: Vec<DeclInfo>,
    /// `#include` operands and whether they were bracketed.
    includes: Vec<(String, bool)>,
}

/// The files an index was built from, with their stamps.
type Snapshot = Vec<(PathBuf, Stamp)>;

#[derive(Default)]
struct Cache {
    files: HashMap<PathBuf, Arc<FileInfo>>,
    /// Per list of include directories: the scanned files and the index
    /// built from them.
    indexes: HashMap<Vec<PathBuf>, (Snapshot, Arc<HeaderIndex>)>,
}

/// A `*.p4` file found under an include directory.
struct Scanned {
    /// Path relative to that directory, with `/` separators.
    rel: String,
    path: PathBuf,
    stamp: Stamp,
}

fn scan(dirs: &[PathBuf]) -> Vec<Scanned> {
    fn walk(dir: &Path, rel: &str, depth: usize, out: &mut Vec<Scanned>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = e.path();
            // Follow symlinks.
            let Ok(md) = std::fs::metadata(&path) else {
                continue;
            };
            let rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if md.is_dir() {
                if depth < MAX_DEPTH {
                    walk(&path, &rel, depth + 1, out);
                }
            } else if md.is_file() && name.ends_with(".p4") {
                let stamp = Stamp {
                    mtime: md.modified().ok(),
                    len: md.len(),
                };
                out.push(Scanned {
                    rel,
                    path: normalize(&path),
                    stamp,
                });
            }
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        walk(&normalize(d), "", 0, &mut out);
    }
    out
}

/// Preprocess and parse one header in isolation.
fn analyze_file(path: &Path, stamp: Stamp) -> Option<FileInfo> {
    let mut text = std::fs::read_to_string(path).ok()?;
    if text.contains("\r\n") {
        text = text.replace("\r\n", "\n");
    }
    let file_name = path.file_name()?;
    std::panic::catch_unwind(|| analyze_text(file_name.as_ref(), &text))
        .ok()
        .map(|(decls, includes)| FileInfo {
            stamp,
            decls,
            includes,
        })
}

type Analyzed = (Vec<DeclInfo>, Vec<(String, bool)>);

fn analyze_text(file_name: &Path, text: &str) -> Analyzed {
    // A directory that cannot exist (paths with NUL bytes are rejected by
    // the OS), so that the preprocessor does not splice included files.
    let fake = Path::new("/\0x4c-header-index").join(file_name);
    let mut db = SourceDb::default();
    let id = db.add(fake, Arc::from(text));
    let pp = preprocess::preprocess(&mut db, id, &Options::default());
    let parse = syntax::parse(&pp.tokens);
    let tokens: Vec<_> =
        pp.tokens.iter().filter(|t| t.span.file == id).collect();
    let detail = |d: &Decl| -> String {
        let start = d
            .annotations()
            .iter()
            .map(|a| a.span.end)
            .max()
            .unwrap_or(0)
            .max(d.span().start);
        let end = d.span().end;
        let first = tokens.partition_point(|t| t.span.start < start);
        let mut out = String::new();
        for t in &tokens[first..] {
            if t.span.start >= end || t.is(TokenKind::LBrace) {
                break;
            }
            if t.is(TokenKind::Semi) {
                break;
            }
            if !out.is_empty() && t.ws_before {
                out.push(' ');
            }
            out.push_str(&t.text);
            if out.len() > MAX_DETAIL {
                let mut cut = MAX_DETAIL;
                while !out.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.truncate(cut);
                out.push('…');
                break;
            }
        }
        out
    };
    let mut decls = Vec::new();
    for d in &parse.program.decls {
        match d {
            Decl::MatchKinds(m) => {
                for n in m.members.iter().filter(|n| !n.is_missing()) {
                    decls.push(DeclInfo {
                        name: n.text.clone(),
                        kind: SymbolKind::MatchKind,
                        detail: format!("match_kind {}", n.text),
                    });
                }
            }
            Decl::Typedef(t) => {
                if let Some(inner) = &t.inline_decl {
                    if let Some(n) = inner.name().filter(|n| !n.is_missing()) {
                        decls.push(DeclInfo {
                            name: n.text.clone(),
                            kind: decl_kind(inner),
                            detail: detail(inner),
                        });
                    }
                }
            }
            _ => {}
        }
        if let Some(n) = d.name().filter(|n| !n.is_missing()) {
            decls.push(DeclInfo {
                name: n.text.clone(),
                kind: decl_kind(d),
                detail: detail(d),
            });
        }
    }
    (decls, include_operands(text))
}

fn decl_kind(d: &Decl) -> SymbolKind {
    match d {
        Decl::Const(_) => SymbolKind::Constant,
        Decl::Var(_) => SymbolKind::Variable,
        Decl::Typedef(_) => SymbolKind::Typedef,
        Decl::Struct(s) => match s.kind {
            StructKind::Struct => SymbolKind::Struct,
            StructKind::Header => SymbolKind::Header,
            StructKind::HeaderUnion => SymbolKind::HeaderUnion,
        },
        Decl::Enum(_) => SymbolKind::Enum,
        Decl::Errors(_) => SymbolKind::Error,
        Decl::MatchKinds(_) => SymbolKind::MatchKind,
        Decl::Extern(_) => SymbolKind::Extern,
        Decl::ExternFunction(_) | Decl::Function(_) => SymbolKind::Function,
        Decl::Action(_) => SymbolKind::Action,
        Decl::ParserType(_) | Decl::Parser(_) => SymbolKind::Parser,
        Decl::ControlType(_) | Decl::Control(_) => SymbolKind::Control,
        Decl::Package(_) => SymbolKind::Package,
        Decl::Instantiation(_) => SymbolKind::Instance,
        Decl::ValueSet(_) => SymbolKind::ValueSet,
        Decl::Table(_) => SymbolKind::Table,
        Decl::Error(_) => SymbolKind::Variable,
    }
}

/// Every `#include` operand in `text`, including those in inactive
/// conditional blocks, with whether it was bracketed.
fn include_operands(text: &str) -> Vec<(String, bool)> {
    let toks = lexer::lex(FileId(0), text).tokens;
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if !(t.is(TokenKind::Hash) && t.nl_before) {
            continue;
        }
        let Some(kw) = toks.get(i + 1) else { continue };
        if kw.nl_before || !kw.is_word("include") {
            continue;
        }
        let Some(op) = toks.get(i + 2).filter(|t| !t.nl_before) else {
            continue;
        };
        match op.kind {
            TokenKind::Str => {
                let name = op.text.trim_start_matches('"');
                let name = name.strip_suffix('"').unwrap_or(name);
                out.push((name.to_string(), false));
            }
            TokenKind::Lt => {
                let gt = toks[i + 3..]
                    .iter()
                    .take_while(|t| !t.nl_before)
                    .find(|t| t.is(TokenKind::Gt));
                if let Some(gt) = gt {
                    let s = op.span.end as usize;
                    let e = gt.span.start as usize;
                    if let Some(name) = text.get(s..e) {
                        out.push((name.trim().to_string(), true));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn build(
    dirs: &[PathBuf],
    scanned: &[Scanned],
    infos: &[Option<Arc<FileInfo>>],
) -> HeaderIndex {
    let dirs: Vec<PathBuf> = dirs.iter().map(|d| normalize(d)).collect();
    let by_path: HashMap<&Path, usize> = scanned
        .iter()
        .enumerate()
        .filter(|(i, _)| infos[*i].is_some())
        .map(|(i, s)| (s.path.as_path(), i))
        .collect();
    // Resolve includes as the preprocessor does: quoted includes search the
    // including file's directory first; all search it last.
    let n = scanned.len();
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    // Per file: (included by another header, included with brackets).
    let mut included = vec![(false, false); n];
    for (i, s) in scanned.iter().enumerate() {
        let Some(info) = &infos[i] else { continue };
        let own = s.path.parent().map(Path::to_path_buf).unwrap_or_default();
        for (name, angle) in &info.includes {
            let mut search: Vec<&Path> = Vec::new();
            if !angle {
                search.push(&own);
            }
            search.extend(dirs.iter().map(PathBuf::as_path));
            search.push(&own);
            let target = search
                .iter()
                .find_map(|d| by_path.get(normalize(&d.join(name)).as_path()));
            if let Some(&t) = target {
                if t != i {
                    edges[i].push(t);
                    included[t].0 = true;
                    included[t].1 |= angle;
                }
            }
        }
    }
    // Headers whose relative path is shadowed by an earlier directory are
    // not candidates (but may be included by relative path).
    let mut seen = HashSet::new();
    let mut cands: Vec<(usize, &str)> = Vec::new();
    for (i, s) in scanned.iter().enumerate() {
        if infos[i].is_some() && seen.insert(s.rel.as_str()) {
            cands.push((i, &s.rel));
        }
    }
    // (internal, closure size, include, declared in the candidate itself)
    type Key<'a> = (bool, usize, &'a str, bool);
    let mut found: HashMap<&str, Vec<(Key, usize, usize, usize)>> =
        HashMap::new();
    for &(h, include) in &cands {
        let closure = closure(h, &edges);
        // A header only included by quoted paths from other headers is an
        // implementation detail (`tofino1_base.p4` behind `tna.p4`). The
        // standard library is public regardless of how it is included.
        let name = scanned[h].path.file_name();
        let internal =
            included[h].0 && !included[h].1 && name != Some("core.p4".as_ref());
        for &f in &closure {
            let Some(info) = &infos[f] else { continue };
            for (di, d) in info.decls.iter().enumerate() {
                let key = (internal, closure.len(), include, f != h);
                found
                    .entry(d.name.as_str())
                    .or_default()
                    .push((key, h, f, di));
            }
        }
    }
    let mut by_name = HashMap::with_capacity(found.len());
    for (name, mut list) in found {
        list.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out: Vec<Candidate> = Vec::new();
        for ((_, _, include, _), h, f, di) in list {
            if out.iter().any(|c| c.include == include) {
                continue;
            }
            let Some(info) = &infos[f] else { continue };
            let d = &info.decls[di];
            out.push(Candidate {
                include: include.to_string(),
                header: scanned[h].path.clone(),
                file: scanned[f].path.clone(),
                kind: d.kind,
                detail: d.detail.clone(),
            });
        }
        by_name.insert(name.to_string(), out);
    }
    let mut names: Vec<(String, String)> = by_name
        .keys()
        .map(|n| (n.to_lowercase(), n.clone()))
        .collect();
    names.sort();
    HeaderIndex { by_name, names }
}

/// `h` and every header it transitively includes.
fn closure(h: usize, edges: &[Vec<usize>]) -> Vec<usize> {
    let mut seen = vec![h];
    let mut stack = vec![h];
    while let Some(i) = stack.pop() {
        for &t in &edges[i] {
            if !seen.contains(&t) {
                seen.push(t);
                stack.push(t);
            }
        }
    }
    seen
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

/// Where to insert `#include <include>` for a name used in `file`: an edit
/// (zero-width span and text) in `file` itself, or in the root when `file`
/// is a fragment without includes of its own.
pub fn include_edit(
    a: &Analysis,
    file: FileId,
    include: &str,
) -> Option<(Span, String)> {
    let has_includes =
        |f: FileId| lines(a.db.text(f)).iter().any(|l| l.include.is_some());
    let target = if file == a.root || has_includes(file) {
        file
    } else {
        a.root
    };
    let text = a.db.text(target);
    let lines = lines(text);
    if lines
        .iter()
        .any(|l| l.include.as_ref().is_some_and(|(n, _)| n == include))
    {
        return None;
    }
    let directive = format!("#include <{include}>\n");
    let at = |offset: usize, insert: String| {
        Some((Span::new(target, offset as u32, offset as u32), insert))
    };
    // The leading block of comments, blank lines, and directives.
    let leading = lines
        .iter()
        .position(|l| l.kind == LineKind::Code)
        .unwrap_or(lines.len());
    let block = &lines[..leading];
    let min_depth = block
        .iter()
        .filter_map(|l| l.include.as_ref().map(|_| l.depth))
        .min();
    if let Some(depth) = min_depth {
        let last = block
            .iter()
            .rev()
            .find(|l| l.include.is_some() && l.depth == depth)?;
        if last.next == text.len() && !text.ends_with('\n') {
            return at(text.len(), format!("\n{directive}"));
        }
        return at(last.next, directive);
    }
    let comments = lines
        .iter()
        .take_while(|l| l.kind == LineKind::Comment)
        .count();
    if comments == 0 {
        let sep = match lines.first() {
            Some(l) if l.kind != LineKind::Blank => "\n",
            _ => "",
        };
        return at(0, format!("{directive}{sep}"));
    }
    let blanks = lines[comments..]
        .iter()
        .take_while(|l| l.kind == LineKind::Blank)
        .count();
    let k = comments + blanks;
    let offset = lines.get(k).map(|l| l.start).unwrap_or(text.len());
    let mut before = String::new();
    if offset == text.len() && !text.is_empty() && !text.ends_with('\n') {
        before.push('\n');
    }
    if blanks == 0 {
        before.push('\n');
    }
    let after = if k < lines.len() { "\n" } else { "" };
    at(offset, format!("{before}{directive}{after}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    Blank,
    Comment,
    Directive,
    Code,
}

struct Line {
    start: usize,
    /// Start of the next line (or the end of the text).
    next: usize,
    kind: LineKind,
    /// Conditional nesting depth at the start of the line.
    depth: usize,
    /// The operand of an `#include` and whether it is bracketed.
    include: Option<(String, bool)>,
}

/// Classify the lines of a file for placing `#include` directives.
fn lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut in_comment = false;
    let mut continued = false;
    let mut depth = 0usize;
    let mut start = 0;
    while start < text.len() {
        let next = text[start..]
            .find('\n')
            .map(|i| start + i + 1)
            .unwrap_or(text.len());
        let line = text[start..next].trim_end_matches(['\n', '\r']);
        let trimmed = line.trim();
        let mut include = None;
        let line_depth = depth;
        let kind = if continued {
            LineKind::Directive
        } else if in_comment {
            LineKind::Comment
        } else if trimmed.is_empty() {
            LineKind::Blank
        } else if trimmed.starts_with("//") || trimmed.starts_with("/*") {
            LineKind::Comment
        } else if let Some(rest) = trimmed.strip_prefix('#') {
            let rest = rest.trim_start();
            let word: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            match word.as_str() {
                "if" | "ifdef" | "ifndef" => depth += 1,
                "endif" => depth = depth.saturating_sub(1),
                "include" => include = include_operand(&rest[7..]),
                _ => {}
            }
            LineKind::Directive
        } else {
            LineKind::Code
        };
        if kind == LineKind::Directive {
            continued = line.ends_with('\\');
        }
        // Track block comments opened or closed on this line.
        {
            let mut rest = line;
            loop {
                if in_comment {
                    match rest.find("*/") {
                        Some(i) => {
                            in_comment = false;
                            rest = &rest[i + 2..];
                        }
                        None => break,
                    }
                } else {
                    let open = rest.find("/*");
                    let line_comment = rest.find("//");
                    match open {
                        Some(i) if line_comment.is_none_or(|l| i < l) => {
                            in_comment = true;
                            rest = &rest[i + 2..];
                        }
                        _ => break,
                    }
                }
            }
        }
        out.push(Line {
            start,
            next,
            kind,
            depth: line_depth,
            include,
        });
        start = next;
    }
    out
}

/// The operand of an `#include` directive: `<x>` or `"x"`.
fn include_operand(rest: &str) -> Option<(String, bool)> {
    let rest = rest.trim_start();
    if let Some(r) = rest.strip_prefix('<') {
        let end = r.find('>')?;
        Some((r[..end].trim().to_string(), true))
    } else if let Some(r) = rest.strip_prefix('"') {
        let end = r.find('"')?;
        Some((r[..end].to_string(), false))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A temporary directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "x4c-headers-{}-{}-{nanos}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const CORE: &str = "\
#ifndef _CORE_P4_
#define _CORE_P4_
error { NoError }
extern packet_in {
    void extract<T>(out T hdr);
}
match_kind { exact, ternary }
#if 0
const bit<8> HIDDEN = 1;
#endif
#endif
";

    /// `inc` has core.p4, v1model.p4 (including core.p4), and
    /// tofino/stratum.p4; `other` shadows core.p4.
    fn fixture() -> (TempDir, Vec<PathBuf>) {
        let t = TempDir::new();
        t.write("inc/core.p4", CORE);
        t.write(
            "inc/v1model.p4",
            "#include \"core.p4\"\n\
             struct standard_metadata_t { bit<9> ingress_port; }\n\
             @pkginfo(arch=\"v1model\")\n\
             package V1Switch<H>(Parser<H> p);\n",
        );
        t.write(
            "inc/tofino/stratum.p4",
            "#include <core.p4>\nheader stratum_h { bit<8> x; }\n",
        );
        t.write("other/core.p4", "extern Shadowed {}\n");
        t.write("other/extra.p4", "#include <core.p4>\nconst int E = 1;\n");
        let dirs = vec![t.0.join("inc"), t.0.join("other")];
        (t, dirs)
    }

    #[test]
    fn headers_lookup_ranking() {
        let (_t, dirs) = fixture();
        let idx = HeaderIndex::for_dirs(&dirs);
        let c = idx.lookup("packet_in");
        let incs: Vec<&str> = c.iter().map(|c| c.include.as_str()).collect();
        assert_eq!(incs[0], "core.p4");
        assert_eq!(incs.len(), MAX_CANDIDATES);
        assert!(c[0].file.ends_with("inc/core.p4"));
        assert_eq!(c[0].kind, SymbolKind::Extern);
        assert_eq!(c[0].detail, "extern packet_in");

        let c = idx.lookup("V1Switch");
        assert_eq!(c[0].include, "v1model.p4");
        assert_eq!(c[0].kind, SymbolKind::Package);
        assert_eq!(c[0].detail, "package V1Switch<H>(Parser<H> p)");
        assert_eq!(idx.lookup("standard_metadata_t")[0].include, "v1model.p4");
        assert_eq!(idx.lookup("exact")[0].kind, SymbolKind::MatchKind);
        assert_eq!(idx.lookup("exact")[0].include, "core.p4");
        // Inactive code, error members, and shadowed headers are skipped.
        assert!(idx.lookup("HIDDEN").is_empty());
        assert!(idx.lookup("NoError").is_empty());
        assert!(idx.lookup("Shadowed").is_empty());
        // A header in a later directory including <core.p4> gets the first.
        assert_eq!(idx.lookup("E")[0].include, "extra.p4");
        assert!(idx
            .lookup("packet_in")
            .iter()
            .all(|c| c.file.ends_with("inc/core.p4")));
    }

    #[test]
    fn headers_subdirectory_include_path() {
        let (_t, dirs) = fixture();
        let idx = HeaderIndex::for_dirs(&dirs);
        let c = idx.lookup("stratum_h");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].include, "tofino/stratum.p4");
        assert_eq!(c[0].kind, SymbolKind::Header);
    }

    #[test]
    fn headers_complete() {
        let (_t, dirs) = fixture();
        let idx = HeaderIndex::for_dirs(&dirs);
        assert!(idx.complete("", &[]).is_empty());
        let c = idx.complete("PACK", &[]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, "packet_in");
        assert_eq!(c[0].1.include, "core.p4");
        let names: Vec<String> =
            idx.complete("s", &[]).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["standard_metadata_t", "stratum_h"]);
    }

    #[test]
    fn headers_cache_refresh() {
        let (t, dirs) = fixture();
        let a = HeaderIndex::for_dirs(&dirs);
        let b = HeaderIndex::for_dirs(&dirs);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(a.lookup("Added").is_empty());
        t.write("inc/core.p4", &format!("{CORE}extern Added {{}}\n"));
        let c = HeaderIndex::for_dirs(&dirs);
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(c.lookup("Added")[0].include, "core.p4");
        // Headers including core.p4 see the change too.
        assert_eq!(c.lookup("Added").len(), MAX_CANDIDATES);
        std::fs::remove_file(t.0.join("inc/v1model.p4")).unwrap();
        let d = HeaderIndex::for_dirs(&dirs);
        assert!(d.lookup("V1Switch").is_empty());
    }

    fn analysis(files: &[(&str, &str)]) -> Analysis {
        let overlays = files
            .iter()
            .map(|(p, t)| (PathBuf::from(p), Arc::from(*t)))
            .collect();
        let mut db = SourceDb::new(overlays);
        let root = db.load(Path::new(files[0].0)).unwrap();
        Analysis::new(db, root, &Options::default())
    }

    /// Apply an edit to its file's text.
    fn apply(a: &Analysis, (span, text): (Span, String)) -> String {
        let mut s = a.db.text(span.file).to_string();
        s.insert_str(span.start as usize, &text);
        s
    }

    #[test]
    fn headers_include_edit_after_last_include() {
        let a = analysis(&[(
            "/t/main.p4",
            "// License\n#include <core.p4>\n#include \"x.p4\"\n\n\
             control C() { apply {} }\n",
        )]);
        let edit = include_edit(&a, a.root, "v1model.p4").unwrap();
        assert_eq!(edit.0.file, a.root);
        assert_eq!(
            apply(&a, edit),
            "// License\n#include <core.p4>\n#include \"x.p4\"\n\
             #include <v1model.p4>\n\ncontrol C() { apply {} }\n"
        );
        assert_eq!(include_edit(&a, a.root, "core.p4"), None);
        assert_eq!(include_edit(&a, a.root, "x.p4"), None);
    }

    #[test]
    fn headers_include_edit_skips_conditional_includes() {
        let a = analysis(&[(
            "/t/main.p4",
            "#include <core.p4>\n#ifdef X\n#include <x.p4>\n#endif\n\
             control C() { apply {} }\n",
        )]);
        let edit = include_edit(&a, a.root, "v1model.p4").unwrap();
        assert_eq!(edit.0.start, "#include <core.p4>\n".len() as u32);
    }

    #[test]
    fn headers_include_edit_after_license() {
        let a = analysis(&[(
            "/t/main.p4",
            "/*\n * License\n */\n\ncontrol C() { apply {} }\n",
        )]);
        let edit = include_edit(&a, a.root, "core.p4").unwrap();
        assert_eq!(
            apply(&a, edit),
            "/*\n * License\n */\n\n#include <core.p4>\n\n\
             control C() { apply {} }\n"
        );

        let a = analysis(&[("/t/main.p4", "control C() { apply {} }\n")]);
        let edit = include_edit(&a, a.root, "core.p4").unwrap();
        assert_eq!(edit.0.start, 0);
        assert_eq!(edit.1, "#include <core.p4>\n\n");
    }

    #[test]
    fn headers_include_edit_fragment_goes_to_root() {
        let a = analysis(&[
            (
                "/t/main.p4",
                "#include <core.p4>\ncontrol C() {\n\
                 #include \"frag.p4\"\n    apply {}\n}\n",
            ),
            ("/t/frag.p4", "// Fragment\naction a() {}\n"),
        ]);
        let frag = a.db.file_id(Path::new("/t/frag.p4")).unwrap();
        let edit = include_edit(&a, frag, "v1model.p4").unwrap();
        assert_eq!(edit.0.file, a.root);
        assert_eq!(
            apply(&a, edit),
            "#include <core.p4>\n#include <v1model.p4>\ncontrol C() {\n\
             #include \"frag.p4\"\n    apply {}\n}\n"
        );
        assert_eq!(include_edit(&a, frag, "core.p4"), None);
    }
}
