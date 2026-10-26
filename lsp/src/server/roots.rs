// Copyright 2026 Oxide Computer Company

//! Compilation roots: which program a file belongs to, and its analysis.

use crate::analysis::Analysis;
use crate::config::{self, Project, RawConfig, CONFIG_FILE_NAME};
use crate::preprocess::Options;
use crate::source::{FileId, SourceDb};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Index into [`RootManager::roots`]. Stable until the next
/// [`RootManager::reset`].
pub type RootId = usize;

/// Unsaved editor buffers, keyed by normalized path.
pub type Overlays = HashMap<PathBuf, Arc<str>>;

/// Environment variable lookup, injectable for tests.
pub type EnvFn = Box<dyn Fn(&str) -> Option<String>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    /// Declared in a config file or client settings.
    Configured,
    /// A file in the opened file's directory that instantiates `main`.
    Inferred,
    /// A file analyzed on its own because no root includes it.
    Standalone,
}

/// Configuration changes made from the editor for this session, applied on
/// top of `x4c-lsp.toml` and client settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    /// Macros defined (or redefined) for every root.
    pub define: Vec<(String, String)>,
    /// Macros undefined for every root.
    pub undefine: Vec<String>,
    /// A root to try before the others: its file and configured options.
    pub prefer: Option<(PathBuf, Options)>,
}

impl Overrides {
    /// Define `name`, or undefine it.
    pub fn set_define(&mut self, name: &str, value: Option<&str>) {
        self.define.retain(|(n, _)| n != name);
        self.undefine.retain(|n| n != name);
        match value {
            Some(v) => self.define.push((name.into(), v.into())),
            None => self.undefine.push(name.into()),
        }
    }
}

pub struct Root {
    pub file: PathBuf,
    /// The options analyzed with: `base` with the session's overrides.
    pub options: Options,
    /// The options from configuration (or inference).
    pub base: Options,
    pub kind: RootKind,
    pub analysis: Option<Analysis>,
    /// Every file loaded by the last analysis, by normalized path.
    pub files: HashMap<PathBuf, FileId>,
    dirty: bool,
}

impl Root {
    pub fn contains(&self, path: &Path) -> bool {
        self.file == path || self.files.contains_key(path)
    }

    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.files.get(path).copied()
    }
}

pub struct RootManager {
    client_config: Option<RawConfig>,
    pub workspace_folders: Vec<PathBuf>,
    fallback_includes: Vec<PathBuf>,
    env: EnvFn,
    projects: HashMap<PathBuf, Arc<Project>>,
    /// Program roots found in each directory, for inference.
    inferred: HashMap<PathBuf, Vec<PathBuf>>,
    pub roots: Vec<Root>,
    /// Root used to answer queries about each file.
    assignment: HashMap<PathBuf, Option<RootId>>,
    /// Set when an analysis loaded a different set of files than before,
    /// which may change which root a file belongs to.
    closures_changed: bool,
    warnings: Vec<String>,
    reported: HashSet<String>,
    overrides: Overrides,
}

impl RootManager {
    pub fn new(env: EnvFn) -> Self {
        let fallback_includes = config::fallback_include_dirs(&*env);
        Self {
            client_config: None,
            workspace_folders: Vec::new(),
            fallback_includes,
            env,
            projects: HashMap::new(),
            inferred: HashMap::new(),
            roots: Vec::new(),
            assignment: HashMap::new(),
            closures_changed: false,
            warnings: Vec::new(),
            reported: HashSet::new(),
            overrides: Overrides::default(),
        }
    }

    pub fn fallback_includes(&self) -> &[PathBuf] {
        &self.fallback_includes
    }

    /// Whether inactive preprocessor regions are reported.
    pub fn inactive_regions(&self) -> bool {
        self.client_config
            .as_ref()
            .and_then(|c| c.inactive_regions)
            .unwrap_or(true)
    }

    /// Replace the client settings. Returns whether they changed.
    pub fn set_client_config(&mut self, config: Option<RawConfig>) -> bool {
        if self.client_config == config {
            return false;
        }
        self.client_config = config;
        self.reset();
        true
    }

    pub fn overrides(&self) -> &Overrides {
        &self.overrides
    }

    /// Replace the session's overrides. Returns whether they changed.
    pub fn set_overrides(&mut self, overrides: Overrides) -> bool {
        if self.overrides == overrides {
            return false;
        }
        self.overrides = overrides;
        self.reset();
        true
    }

    /// `base` with the session's defines applied.
    fn effective(&self, base: &Options) -> Options {
        let mut o = base.clone();
        o.defines
            .retain(|(n, _)| !self.overrides.undefine.contains(n));
        config::merge_defines(&mut o.defines, self.overrides.define.clone());
        o
    }

    /// Forget all configuration, roots, and analyses.
    pub fn reset(&mut self) {
        self.projects.clear();
        self.inferred.clear();
        self.roots.clear();
        self.assignment.clear();
        self.reported.clear();
        self.closures_changed = false;
    }

    /// Configuration problems found since the last call, each reported once
    /// per configuration.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    fn warn(&mut self, msg: String) {
        if self.reported.insert(msg.clone()) {
            self.warnings.push(msg);
        }
    }

    /// The deepest workspace folder containing `path`.
    pub fn workspace_folder(&self, path: &Path) -> Option<&Path> {
        self.workspace_folders
            .iter()
            .filter(|w| path.starts_with(w))
            .max_by_key(|w| w.components().count())
            .map(PathBuf::as_path)
    }

    pub fn in_workspace(&self, path: &Path) -> bool {
        self.workspace_folder(path).is_some()
    }

    fn project_for(&mut self, path: &Path) -> Arc<Project> {
        let config_file =
            config::find_config_file(path, &self.workspace_folders);
        let base = match &config_file {
            Some(f) => f.parent().unwrap_or(Path::new("/")).to_path_buf(),
            None => match self.workspace_folder(path) {
                Some(w) => w.to_path_buf(),
                None => path.parent().unwrap_or(Path::new("/")).to_path_buf(),
            },
        };
        if let Some(p) = self.projects.get(&base) {
            return p.clone();
        }
        let file_config = config_file.as_ref().and_then(|f| {
            let parsed = std::fs::read_to_string(f)
                .map_err(|e| e.to_string())
                .and_then(|text| RawConfig::from_toml(&text));
            match parsed {
                Ok(c) => Some(c),
                Err(e) => {
                    self.warn(format!("{}: {e}", f.display()));
                    None
                }
            }
        });
        let (project, warnings) = Project::resolve(
            &base,
            config_file.as_deref(),
            file_config.as_ref(),
            self.client_config.as_ref(),
            &self.fallback_includes,
            &*self.env,
        );
        let origin = config_file
            .as_ref()
            .map(|f| f.display().to_string())
            .unwrap_or_else(|| "client configuration".into());
        for w in warnings {
            self.warn(format!("{origin}: {w}"));
        }
        for r in &project.roots {
            if !r.file.is_file() {
                self.warn(format!(
                    "{origin}: root `{}` does not exist",
                    r.file.display()
                ));
            }
        }
        let project = Arc::new(project);
        self.projects.insert(base, project.clone());
        project
    }

    fn ensure_root(
        &mut self,
        file: &Path,
        base: Options,
        kind: RootKind,
    ) -> RootId {
        if let Some(id) = self
            .roots
            .iter()
            .position(|r| r.file == file && r.base == base)
        {
            return id;
        }
        let options = self.effective(&base);
        eprintln!(
            "x4c-lsp: {kind:?} root {} with {:?}",
            file.display(),
            options
        );
        self.roots.push(Root {
            file: file.to_path_buf(),
            options,
            base,
            kind,
            analysis: None,
            files: HashMap::new(),
            dirty: true,
        });
        self.roots.len() - 1
    }

    /// Re-analyze `id` if anything in its closure changed.
    pub fn refresh(&mut self, id: RootId, overlays: &Overlays) {
        let root = &mut self.roots[id];
        if !root.dirty {
            return;
        }
        root.dirty = false;
        let mut db = SourceDb::new(overlays.clone());
        let file_id = match db.load(&root.file) {
            Some(id) => id,
            None => db.add(root.file.clone(), Arc::from("")),
        };
        let analysis =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Analysis::new(db, file_id, &root.options)
            }));
        let files: HashMap<PathBuf, FileId> = match &analysis {
            Ok(a) => a
                .files()
                .map(|f| (config::normalize_path(a.db.path(f)), f))
                .collect(),
            Err(_) => {
                eprintln!(
                    "x4c-lsp: analysis of {:?} root {} panicked",
                    root.kind,
                    root.file.display()
                );
                HashMap::new()
            }
        };
        let old: HashSet<&PathBuf> = root.files.keys().collect();
        let new: HashSet<&PathBuf> = files.keys().collect();
        if old != new {
            self.closures_changed = true;
        }
        root.files = files;
        root.analysis = analysis.ok();
    }

    /// The root used for queries about `path`, analyzing candidates as
    /// needed:
    ///
    /// 1. the first configured root whose include closure contains it;
    /// 2. without configured roots, the first program in the file's
    ///    directory (a file instantiating `main`) whose closure contains it;
    /// 3. otherwise the file itself, if it is a `.p4` file.
    pub fn root_for(
        &mut self,
        path: &Path,
        overlays: &Overlays,
    ) -> Option<RootId> {
        if std::mem::take(&mut self.closures_changed) {
            self.assignment.clear();
        }
        if let Some(id) = self.assignment.get(path) {
            return *id;
        }
        let id = self.resolve(path, overlays);
        self.assignment.insert(path.to_path_buf(), id);
        id
    }

    fn resolve(&mut self, path: &Path, overlays: &Overlays) -> Option<RootId> {
        if let Some(&id) = self.choices(path, overlays, true).first() {
            return Some(id);
        }
        // A header outside the project (an architecture file reached by
        // going to a definition) belongs to the program that includes it.
        if let Some(id) = self
            .roots
            .iter()
            .position(|r| r.kind != RootKind::Standalone && r.contains(path))
        {
            return Some(id);
        }
        if path.extension().is_some_and(|e| e == "p4") {
            let project = self.project_for(path);
            let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            let options = project.options(std::slice::from_ref(&dir));
            return Some(self.ensure_root(path, options, RootKind::Standalone));
        }
        None
    }

    /// The configured roots (or, without any, the programs in the file's
    /// directory) whose include closure contains `path`, the preferred one
    /// first. With `first`, stops at the first match.
    pub fn choices(
        &mut self,
        path: &Path,
        overlays: &Overlays,
        first: bool,
    ) -> Vec<RootId> {
        let project = self.project_for(path);
        let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let (mut entries, kind): (Vec<(PathBuf, Options)>, _) =
            if !project.roots.is_empty() {
                let e = project
                    .roots
                    .iter()
                    .map(|r| (r.file.clone(), r.options.clone()))
                    .collect();
                (e, RootKind::Configured)
            } else {
                let options = project.options(std::slice::from_ref(&dir));
                let e = self
                    .candidates(&dir, overlays)
                    .into_iter()
                    .map(|c| (c, options.clone()))
                    .collect();
                (e, RootKind::Inferred)
            };
        if let Some(i) = self
            .overrides
            .prefer
            .as_ref()
            .and_then(|p| entries.iter().position(|e| e == p))
        {
            let e = entries.remove(i);
            entries.insert(0, e);
        }
        let mut out = Vec::new();
        for (file, base) in entries {
            let id = self.ensure_root(&file, base, kind);
            self.refresh(id, overlays);
            if self.roots[id].contains(path) {
                out.push(id);
                if first {
                    break;
                }
            }
        }
        out
    }

    /// Program roots in `dir`, sorted by path. Cached until
    /// [`Self::invalidate_dir`].
    fn candidates(&mut self, dir: &Path, overlays: &Overlays) -> Vec<PathBuf> {
        if let Some(c) = self.inferred.get(dir) {
            return c.clone();
        }
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = config::normalize_path(&entry.path());
                if path.extension().is_none_or(|e| e != "p4") {
                    continue;
                }
                let text = match overlays.get(&path) {
                    Some(t) => Some(t.to_string()),
                    None => std::fs::read_to_string(&path).ok(),
                };
                if text.is_some_and(|t| config::is_program_root(&t)) {
                    found.push(path);
                }
            }
        }
        // Unsaved buffers that are not on disk yet.
        for (path, text) in overlays {
            if path.parent() == Some(dir)
                && path.extension().is_some_and(|e| e == "p4")
                && !found.contains(path)
                && config::is_program_root(text)
            {
                found.push(path.clone());
            }
        }
        found.sort();
        self.inferred.insert(dir.to_path_buf(), found.clone());
        found
    }

    /// Forget cached program roots for `dir`, e.g. after a file in it was
    /// saved, created, or deleted.
    pub fn invalidate_dir(&mut self, dir: &Path) {
        if self.inferred.remove(dir).is_some() {
            // A new or removed program root may change assignments.
            self.assignment.clear();
        }
    }

    /// Mark every root whose closure contains `path` for re-analysis.
    /// Returns whether any root contained it.
    pub fn file_changed(&mut self, path: &Path) -> bool {
        let mut any = false;
        for root in &mut self.roots {
            if root.contains(path) {
                root.dirty = true;
                any = true;
            }
        }
        any
    }

    /// Mark every root for re-analysis, e.g. when a file appeared that a
    /// previously unresolved `#include` may now find.
    pub fn mark_all_dirty(&mut self) {
        for root in &mut self.roots {
            root.dirty = true;
        }
    }

    pub fn is_config_file(path: &Path) -> bool {
        path.file_name().is_some_and(|n| n == CONFIG_FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn manager(ws: &Path) -> RootManager {
        let mut m = RootManager::new(Box::new(|_| None));
        m.workspace_folders = vec![ws.to_path_buf()];
        m
    }

    #[test]
    fn standalone_and_non_p4() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let lib = ws.join("lib.p4");
        write(&lib, "const bit<8> X = 1;\n");
        let mut m = manager(&ws);
        let overlays = Overlays::new();
        let id = m.root_for(&lib, &overlays).unwrap();
        assert_eq!(m.roots[id].kind, RootKind::Standalone);
        assert_eq!(m.roots[id].file, lib);
        assert_eq!(
            m.roots[id].options.include_dirs.first(),
            Some(&ws),
            "the file's directory is searched first"
        );
        assert_eq!(m.root_for(&ws.join("notes.txt"), &overlays), None);
    }

    #[test]
    fn inferred_roots() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let a = ws.join("a.p4");
        let b = ws.join("b.p4");
        write(&a, "control c() { apply {} }\nPkg(c()) main;\n");
        write(&b, "Pkg(d()) main;\n");
        let mut m = manager(&ws);
        let overlays = Overlays::new();
        // A program root is its own root.
        let id = m.root_for(&b, &overlays).unwrap();
        assert_eq!(m.roots[id].file, b);
        assert_eq!(m.roots[id].kind, RootKind::Inferred);
        let id = m.root_for(&a, &overlays).unwrap();
        assert_eq!(m.roots[id].file, a);
        // A file no candidate includes is analyzed on its own.
        let frag = ws.join("frag.p4");
        write(&frag, "apply {}\n");
        let id = m.root_for(&frag, &overlays).unwrap();
        assert_eq!(m.roots[id].kind, RootKind::Standalone);
        assert_eq!(m.roots[id].file, frag);
    }

    #[test]
    fn configured_roots() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let main = ws.join("dpd/p4/sidecar.p4");
        write(&main, "Pipeline(i(), e()) main;\n");
        write(
            &ws.join(CONFIG_FILE_NAME),
            r#"
            define = ["__TARGET_TOFINO__=2"]
            [[root]]
            file = "dpd/p4/sidecar.p4"
            include = ["dpd/p4"]
            [[root]]
            file = "dpd/p4/sidecar.p4"
            include = ["dpd/p4"]
            define = ["MULTICAST"]
            "#,
        );
        let mut m = manager(&ws);
        let overlays = Overlays::new();
        // The first configured root containing the file wins.
        let id = m.root_for(&main, &overlays).unwrap();
        assert_eq!(m.roots[id].kind, RootKind::Configured);
        assert_eq!(id, 0);
        assert_eq!(
            m.roots[id].options.include_dirs.first(),
            Some(&ws.join("dpd/p4"))
        );
        assert_eq!(
            m.roots[id].options.defines,
            vec![("__TARGET_TOFINO__".to_string(), "2".to_string())]
        );
        assert!(m.take_warnings().is_empty());

        // A file outside every configured closure is analyzed on its own
        // with the project's options.
        let other = ws.join("other/x.p4");
        write(&other, "");
        let id = m.root_for(&other, &overlays).unwrap();
        assert_eq!(m.roots[id].kind, RootKind::Standalone);
        assert_eq!(
            m.roots[id].options.defines,
            vec![("__TARGET_TOFINO__".to_string(), "2".to_string())]
        );

        // Preferring the second root (as chosen from the editor) makes it
        // win; session defines apply on top of its configuration.
        let both = m.choices(&main, &overlays, false);
        assert_eq!(both.len(), 2);
        let second = &m.roots[both[1]];
        let mut ov = Overrides {
            prefer: Some((second.file.clone(), second.base.clone())),
            ..Default::default()
        };
        ov.set_define("__TARGET_TOFINO__", None);
        ov.set_define("EXTRA", Some("3"));
        assert!(m.set_overrides(ov));
        let id = m.root_for(&main, &overlays).unwrap();
        let def = |n: &str, v: &str| (n.to_string(), v.to_string());
        assert_eq!(
            m.roots[id].options.defines,
            vec![def("MULTICAST", "1"), def("EXTRA", "3")]
        );
    }

    #[test]
    fn outside_headers_join_the_including_program() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        let arch = base.join("include/arch.p4");
        write(&arch, "control C();\n");
        let main = ws.join("main.p4");
        write(&main, "#include <arch.p4>\nPkg(c()) main;\n");
        let mut m = manager(&ws);
        m.set_client_config(
            RawConfig::from_json(&serde_json::json!({
                "include": [base.join("include").display().to_string()],
            }))
            .unwrap(),
        );
        let overlays = Overlays::new();
        let prog = m.root_for(&main, &overlays).unwrap();
        assert_eq!(m.root_for(&arch, &overlays), Some(prog));
    }

    #[test]
    fn bad_config_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        write(&ws.join(CONFIG_FILE_NAME), "bogus = 1\n");
        let f = ws.join("x.p4");
        write(&f, "");
        let mut m = manager(&ws);
        let overlays = Overlays::new();
        assert!(m.root_for(&f, &overlays).is_some());
        assert_eq!(m.take_warnings().len(), 1);
        m.projects.clear();
        m.assignment.clear();
        m.root_for(&f, &overlays);
        assert!(m.take_warnings().is_empty());
    }

    #[test]
    fn dirty_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let f = ws.join("x.p4");
        write(&f, "");
        let mut m = manager(&ws);
        let overlays = Overlays::new();
        let id = m.root_for(&f, &overlays).unwrap();
        m.refresh(id, &overlays);
        assert!(!m.roots[id].dirty);
        assert!(!m.file_changed(&ws.join("y.p4")));
        assert!(m.file_changed(&f));
        assert!(m.roots[id].dirty);
    }
}
