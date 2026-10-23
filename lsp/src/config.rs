// Copyright 2026 Oxide Computer Company

//! Project configuration: compilation roots, include paths, and defines.
//!
//! P4 programs are analyzed from a root file through its `#include` closure.
//! A project describes its roots in an `x4c-lsp.toml` file:
//!
//! ```toml
//! # Paths are relative to the directory containing this file.
//! include = ["/opt/p4c/p4include"]   # searched by every root
//! define = ["__TARGET_TOFINO__=2"]   # NAME or NAME=VALUE
//!
//! [[root]]
//! file = "dpd/p4/sidecar.p4"
//! include = ["dpd/p4"]              # searched before the global list
//! define = ["MULTICAST"]
//! ```
//!
//! The same schema may be supplied by the client as JSON (initialization
//! options or `workspace/didChangeConfiguration` settings). A config file
//! takes precedence for its project; client settings supply defaults.

use crate::preprocess::Options;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

/// Name of the per-project configuration file.
pub const CONFIG_FILE_NAME: &str = "x4c-lsp.toml";

/// Looks up an environment variable. Abstracted for testing.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Configuration as written by the user, before path resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    /// Include directories searched by every root.
    #[serde(default)]
    pub include: Vec<String>,
    /// Macros predefined for every root, as `NAME` or `NAME=VALUE`.
    #[serde(default)]
    pub define: Vec<String>,
    /// Compilation roots.
    #[serde(default, alias = "roots")]
    pub root: Vec<RawRoot>,
    /// Whether to report code skipped by `#if` as faded hints (default
    /// true). Only read from client settings.
    #[serde(default, rename = "inactive-regions")]
    pub inactive_regions: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRoot {
    /// The program's top-level file.
    pub file: String,
    /// Include directories searched before the global ones.
    #[serde(default)]
    pub include: Vec<String>,
    /// Macros defined in addition to (or overriding) the global ones.
    #[serde(default)]
    pub define: Vec<String>,
}

impl RawConfig {
    pub fn from_toml(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Parse client-supplied settings. `null` and `{}` mean "no settings".
    /// A wrapper object `{"x4c-lsp": {...}}` is also accepted.
    pub fn from_json(
        value: &serde_json::Value,
    ) -> Result<Option<Self>, String> {
        use serde_json::Value;
        let value = match value {
            Value::Object(m) if m.len() == 1 && m.contains_key("x4c-lsp") => {
                &m["x4c-lsp"]
            }
            v => v,
        };
        match value {
            Value::Null => Ok(None),
            Value::Object(m) if m.is_empty() => Ok(None),
            v => serde_json::from_value(v.clone())
                .map(Some)
                .map_err(|e| e.to_string()),
        }
    }
}

/// One compilation root with fully resolved preprocessor options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSpec {
    pub file: PathBuf,
    pub options: Options,
}

/// The effective configuration for one project directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Project {
    /// Directory relative paths are resolved against: the config file's
    /// directory, or the workspace folder (or file directory) without one.
    pub base: PathBuf,
    pub config_file: Option<PathBuf>,
    /// Global include directories, including fallback system directories
    /// when none are configured.
    pub include: Vec<PathBuf>,
    pub defines: Vec<(String, String)>,
    /// Configured roots. Empty means roots are inferred.
    pub roots: Vec<RootSpec>,
}

impl Project {
    /// Merge a config file (`file`) with client settings (`client`).
    ///
    /// - include: the file's directories, then the client's; if neither
    ///   configures any, `fallback` is used.
    /// - define: the client's, then the file's (later definitions of the
    ///   same name win).
    /// - roots: the file's if it declares any, otherwise the client's.
    ///
    /// Problems (bad defines, undefined variables) are returned as
    /// warnings; offending entries are skipped.
    pub fn resolve(
        base: &Path,
        config_file: Option<&Path>,
        file: Option<&RawConfig>,
        client: Option<&RawConfig>,
        fallback: &[PathBuf],
        env: Env,
    ) -> (Project, Vec<String>) {
        let mut warnings = Vec::new();
        let layers: Vec<&RawConfig> =
            [file, client].into_iter().flatten().collect();

        let mut include = Vec::new();
        for layer in &layers {
            include.extend(resolve_dirs(
                &layer.include,
                base,
                env,
                &mut warnings,
            ));
        }
        if include.is_empty() {
            include = fallback.to_vec();
        }
        dedup_paths(&mut include);

        let mut defines = Vec::new();
        for layer in layers.iter().rev() {
            merge_defines(
                &mut defines,
                parse_defines(&layer.define, &mut warnings),
            );
        }

        let raw_roots = layers
            .iter()
            .map(|l| &l.root)
            .find(|r| !r.is_empty())
            .cloned()
            .unwrap_or_default();
        let mut roots = Vec::new();
        for r in &raw_roots {
            let Some(file) = expand_path(&r.file, base, env) else {
                warnings.push(format!(
                    "root `{}`: undefined environment variable",
                    r.file
                ));
                continue;
            };
            let mut include_dirs =
                resolve_dirs(&r.include, base, env, &mut warnings);
            include_dirs.extend(include.iter().cloned());
            dedup_paths(&mut include_dirs);
            let mut root_defines = defines.clone();
            merge_defines(
                &mut root_defines,
                parse_defines(&r.define, &mut warnings),
            );
            roots.push(RootSpec {
                file,
                options: Options {
                    include_dirs,
                    defines: root_defines,
                },
            });
        }

        let project = Project {
            base: base.to_path_buf(),
            config_file: config_file.map(Path::to_path_buf),
            include,
            defines,
            roots,
        };
        (project, warnings)
    }

    /// Options for an inferred or standalone root: `extra` directories
    /// are searched before the project's global include directories.
    pub fn options(&self, extra: &[PathBuf]) -> Options {
        let mut include_dirs = extra.to_vec();
        include_dirs.extend(self.include.iter().cloned());
        dedup_paths(&mut include_dirs);
        Options {
            include_dirs,
            defines: self.defines.clone(),
        }
    }
}

fn resolve_dirs(
    dirs: &[String],
    base: &Path,
    env: Env,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        match expand_path(d, base, env) {
            Some(p) => out.push(p),
            None => warnings
                .push(format!("include `{d}`: undefined environment variable")),
        }
    }
    out
}

fn parse_defines(
    defines: &[String],
    warnings: &mut Vec<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for d in defines {
        match parse_define(d) {
            Ok(def) => out.push(def),
            Err(e) => warnings.push(e),
        }
    }
    out
}

/// Parse a `-D`-style definition: `NAME` (defined as `1`) or `NAME=VALUE`.
pub fn parse_define(s: &str) -> Result<(String, String), String> {
    let s = s.trim();
    let (name, value) = match s.split_once('=') {
        Some((n, v)) => (n.trim(), v.trim().to_string()),
        None => (s, "1".to_string()),
    };
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(format!(
            "invalid define `{s}`: expected NAME or NAME=VALUE"
        ));
    }
    Ok((name.to_string(), value))
}

/// Append `new` definitions; a redefinition replaces the earlier one.
pub fn merge_defines(
    defines: &mut Vec<(String, String)>,
    new: Vec<(String, String)>,
) {
    for (name, value) in new {
        defines.retain(|(n, _)| *n != name);
        defines.push((name, value));
    }
}

fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
}

/// Expand `~`, `$VAR`, and `${VAR}`, resolve relative paths against `base`,
/// and normalize. Returns `None` if a variable is undefined.
pub fn expand_path(s: &str, base: &Path, env: Env) -> Option<PathBuf> {
    let mut out = String::new();
    let mut rest = s;
    if rest == "~" || rest.starts_with("~/") {
        out.push_str(&env("HOME")?);
        rest = &rest[1..];
    }
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let (name, next) = if let Some(braced) = after.strip_prefix('{') {
            let end = braced.find('}')?;
            (&braced[..end], &braced[end + 1..])
        } else {
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            (&after[..end], &after[end..])
        };
        if name.is_empty() {
            out.push('$');
        } else {
            out.push_str(&env(name)?);
        }
        rest = next;
    }
    out.push_str(rest);
    Some(normalize_path(&base.join(out)))
}

/// Normalize a path so one file always maps to one path: canonicalize when
/// it exists on disk; otherwise canonicalize the longest existing ancestor
/// and resolve `.` and `..` lexically in the remainder.
pub fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let lexical = lexical_normalize(path);
    let mut ancestor = lexical.as_path();
    let mut tail = Vec::new();
    while let Some(parent) = ancestor.parent() {
        if let Some(name) = ancestor.file_name() {
            tail.push(name.to_os_string());
        }
        ancestor = parent;
        if let Ok(mut p) = ancestor.canonicalize() {
            for name in tail.iter().rev() {
                p.push(name);
            }
            return p;
        }
    }
    lexical
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Find the nearest `x4c-lsp.toml` at or above `file`'s directory. The
/// search stops at the deepest workspace folder containing `file` (which is
/// itself checked); for files outside every folder it continues to the
/// filesystem root.
pub fn find_config_file(file: &Path, workspace: &[PathBuf]) -> Option<PathBuf> {
    let stop = workspace
        .iter()
        .filter(|w| file.starts_with(w))
        .max_by_key(|w| w.components().count());
    for dir in file.parent()?.ancestors() {
        let candidate = dir.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if Some(dir) == stop.map(PathBuf::as_path) {
            break;
        }
    }
    None
}

/// Whether a file's text instantiates a top-level `main` package, which
/// makes it a program root, e.g. `V1Switch(...) main;`.
pub fn is_program_root(text: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\)\s*main\s*;").unwrap())
        .is_match(text)
}

/// System include directories used when none are configured:
///
/// - each entry of `$P4C_INCLUDE` (colon-separated),
/// - `/usr/share/p4c/p4include` and `/usr/local/share/p4c/p4include`,
/// - `$SDE_INSTALL/share/p4c/p4include` and
///   `$SDE/install/share/p4c/p4include` (Intel Tofino SDE),
///
/// plus each one's `tofino` and `tofino2` subdirectories. Only existing
/// directories are returned.
pub fn fallback_include_dirs(env: Env) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(v) = env("P4C_INCLUDE") {
        bases.extend(v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    bases.push("/usr/share/p4c/p4include".into());
    bases.push("/usr/local/share/p4c/p4include".into());
    if let Some(sde) = env("SDE_INSTALL") {
        bases.push(Path::new(&sde).join("share/p4c/p4include"));
    }
    if let Some(sde) = env("SDE") {
        bases.push(Path::new(&sde).join("install/share/p4c/p4include"));
    }
    let mut out = Vec::new();
    for base in bases {
        if !base.is_dir() {
            continue;
        }
        out.push(normalize_path(&base));
        for sub in ["tofino", "tofino2"] {
            let d = base.join(sub);
            if d.is_dir() {
                out.push(normalize_path(&d));
            }
        }
    }
    dedup_paths(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn define_parsing() {
        assert_eq!(parse_define("FOO"), Ok(("FOO".into(), "1".into())));
        assert_eq!(
            parse_define("__TARGET_TOFINO__=2"),
            Ok(("__TARGET_TOFINO__".into(), "2".into()))
        );
        assert_eq!(parse_define(" A = b c "), Ok(("A".into(), "b c".into())));
        assert_eq!(parse_define("EMPTY="), Ok(("EMPTY".into(), "".into())));
        assert!(parse_define("").is_err());
        assert!(parse_define("1X").is_err());
        assert!(parse_define("F(x)=x").is_err());
        assert!(parse_define("=1").is_err());
    }

    #[test]
    fn merge_defines_overrides() {
        let mut d = vec![("A".into(), "1".into()), ("B".into(), "1".into())];
        merge_defines(&mut d, vec![("A".into(), "2".into())]);
        assert_eq!(d, vec![("B".into(), "1".into()), ("A".into(), "2".into())]);
    }

    #[test]
    fn parse_toml_and_json() {
        let toml = r#"
            include = ["/opt/p4c/p4include"]
            define = ["__TARGET_TOFINO__=2"]
            [[root]]
            file = "dpd/p4/sidecar.p4"
            include = ["dpd/p4"]
            define = ["MULTICAST"]
        "#;
        let cfg = RawConfig::from_toml(toml).unwrap();
        assert_eq!(cfg.include, vec!["/opt/p4c/p4include"]);
        assert_eq!(cfg.root.len(), 1);
        assert_eq!(cfg.root[0].file, "dpd/p4/sidecar.p4");
        assert_eq!(cfg.root[0].define, vec!["MULTICAST"]);

        let json = serde_json::json!({
            "include": ["/opt/p4c/p4include"],
            "define": ["__TARGET_TOFINO__=2"],
            "root": [{
                "file": "dpd/p4/sidecar.p4",
                "include": ["dpd/p4"],
                "define": ["MULTICAST"],
            }],
        });
        assert_eq!(RawConfig::from_json(&json).unwrap(), Some(cfg.clone()));
        let wrapped = serde_json::json!({ "x4c-lsp": json });
        assert_eq!(RawConfig::from_json(&wrapped).unwrap(), Some(cfg));
        assert_eq!(RawConfig::from_json(&serde_json::Value::Null), Ok(None));
        assert_eq!(RawConfig::from_json(&serde_json::json!({})), Ok(None));

        assert!(RawConfig::from_toml("includes = []").is_err());
        assert!(RawConfig::from_toml("[[root]]\ninclude = []").is_err());
        assert!(RawConfig::from_json(&serde_json::json!({"x": 1})).is_err());
    }

    #[test]
    fn project_merging() {
        let base = Path::new("/proj");
        let file = RawConfig::from_toml(
            r#"
            include = ["inc"]
            define = ["A=file", "F"]
            [[root]]
            file = "p4/main.p4"
            include = ["p4"]
            define = ["MULTICAST", "A=root"]
            "#,
        )
        .unwrap();
        let client = RawConfig {
            include: vec!["/sys/p4include".into()],
            define: vec!["A=client".into(), "C".into(), "bad name".into()],
            root: vec![RawRoot {
                file: "ignored.p4".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fallback = vec![PathBuf::from("/fallback")];
        let (p, warnings) = Project::resolve(
            base,
            Some(Path::new("/proj/x4c-lsp.toml")),
            Some(&file),
            Some(&client),
            &fallback,
            &no_env,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            p.include,
            vec![PathBuf::from("/proj/inc"), PathBuf::from("/sys/p4include")]
        );
        let def = |n: &str, v: &str| (n.to_string(), v.to_string());
        assert_eq!(
            p.defines,
            vec![def("C", "1"), def("A", "file"), def("F", "1")]
        );
        assert_eq!(p.roots.len(), 1);
        let root = &p.roots[0];
        assert_eq!(root.file, PathBuf::from("/proj/p4/main.p4"));
        assert_eq!(
            root.options.include_dirs,
            vec![
                PathBuf::from("/proj/p4"),
                PathBuf::from("/proj/inc"),
                PathBuf::from("/sys/p4include"),
            ]
        );
        assert_eq!(
            root.options.defines,
            vec![
                def("C", "1"),
                def("F", "1"),
                def("MULTICAST", "1"),
                def("A", "root"),
            ]
        );

        // Client-only configuration: client roots apply, fallback is used
        // when no include directories are configured.
        let client = RawConfig {
            root: vec![RawRoot {
                file: "main.p4".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (p, _) = Project::resolve(
            base,
            None,
            None,
            Some(&client),
            &fallback,
            &no_env,
        );
        assert_eq!(p.include, fallback);
        assert_eq!(p.roots[0].file, PathBuf::from("/proj/main.p4"));
        assert_eq!(
            p.roots[0].options.include_dirs,
            vec![PathBuf::from("/fallback")]
        );
        assert_eq!(
            p.options(&[PathBuf::from("/proj/sub")]).include_dirs,
            vec![PathBuf::from("/proj/sub"), PathBuf::from("/fallback")]
        );
    }

    #[test]
    fn path_expansion() {
        let env = env_from(&[("SDE", "/sde"), ("HOME", "/x4c-nonexistent/me")]);
        let base = Path::new("/proj");
        assert_eq!(
            expand_path("$SDE/install", base, &env),
            Some(PathBuf::from("/sde/install"))
        );
        assert_eq!(
            expand_path("${SDE}x/../y", base, &env),
            Some(PathBuf::from("/y"))
        );
        assert_eq!(
            expand_path("~/p4", base, &env),
            Some(PathBuf::from("/x4c-nonexistent/me/p4"))
        );
        assert_eq!(
            expand_path("./a/./b/../c", base, &env),
            Some(PathBuf::from("/proj/a/c"))
        );
        assert_eq!(expand_path("$NOPE/x", base, &env), None);
    }

    #[test]
    fn normalize_existing_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        std::fs::create_dir(canon.join("a")).unwrap();
        std::fs::write(canon.join("a/f.p4"), "").unwrap();
        // Non-canonical spelling of an existing file.
        let p = dir.path().join("a/../a/./f.p4");
        assert_eq!(normalize_path(&p), canon.join("a/f.p4"));
        // A file that does not exist yet still maps to the canonical
        // directory.
        let p = dir.path().join("a/./new.p4");
        assert_eq!(normalize_path(&p), canon.join("a/new.p4"));
        let p = dir.path().join("missing/../a/new.p4");
        assert_eq!(normalize_path(&p), canon.join("a/new.p4"));
    }

    #[test]
    fn config_file_search() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let sub = ws.join("dpd/p4");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("route_selector.p4");
        assert_eq!(find_config_file(&file, std::slice::from_ref(&ws)), None);
        std::fs::write(ws.join(CONFIG_FILE_NAME), "").unwrap();
        assert_eq!(
            find_config_file(&file, std::slice::from_ref(&ws)),
            Some(ws.join(CONFIG_FILE_NAME))
        );
        std::fs::write(ws.join("dpd").join(CONFIG_FILE_NAME), "").unwrap();
        assert_eq!(
            find_config_file(&file, std::slice::from_ref(&ws)),
            Some(ws.join("dpd").join(CONFIG_FILE_NAME))
        );
        // The search stops at the workspace folder.
        assert_eq!(find_config_file(&file, &[ws.join("dpd/p4")]), None);
    }

    #[test]
    fn program_root_detection() {
        assert!(is_program_root("V1Switch(p(), i(), e()) main;"));
        assert!(is_program_root("Pipeline(a, b)\n  main ;\n"));
        assert!(!is_program_root("control main() {}"));
        assert!(!is_program_root("apply { main(); }"));
    }

    #[test]
    fn fallback_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let sde = dir.path().canonicalize().unwrap();
        let inc = sde.join("install/share/p4c/p4include");
        std::fs::create_dir_all(inc.join("tofino")).unwrap();
        let extra = sde.join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        let p4c = format!("{}:/does/not/exist", extra.display());
        let env =
            env_from(&[("SDE", sde.to_str().unwrap()), ("P4C_INCLUDE", &p4c)]);
        let dirs = fallback_include_dirs(&env);
        let pos = |p: &Path| dirs.iter().position(|d| d == p);
        assert_eq!(pos(&extra), Some(0));
        let i = pos(&inc).expect("SDE include dir");
        assert_eq!(pos(&inc.join("tofino")), Some(i + 1));
        assert!(pos(Path::new("/does/not/exist")).is_none());
    }
}
