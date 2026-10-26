// Copyright 2026 Oxide Computer Company

//! `workspace/executeCommand`: switching the build configuration from the
//! editor (Helix: `:lsp-workspace-command`).

use super::roots::{Overrides, RootId};
use super::{uri_to_path, Server};
use crate::config;
use lsp_server::{Request, RequestId};
use lsp_types::request::Request as _;
use lsp_types::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Choose a define to toggle or a root to analyze with, from a menu. An
/// optional argument names the document (a path or `file://` URI);
/// otherwise the last P4 document opened, edited, or queried is used.
pub const CONFIGURE: &str = "x4c.configure";
/// Show a document's root and defines (same argument as [`CONFIGURE`]).
pub const SHOW: &str = "x4c.showConfiguration";
/// Toggle a define given as `"NAME"` or `"NAME=VALUE"`.
pub const TOGGLE_DEFINE: &str = "x4c.toggleDefine";
/// Drop all changes made with these commands.
pub const RESET: &str = "x4c.resetConfiguration";

pub const ALL: &[&str] = &[CONFIGURE, SHOW, TOGGLE_DEFINE, RESET];

/// An entry in the configuration menu.
#[derive(Clone, Debug)]
pub enum Choice {
    Define(String, String),
    Undefine(String),
    Prefer(PathBuf, crate::preprocess::Options),
    Reset,
}

impl Server<'_> {
    pub(super) fn execute_command(
        &mut self,
        p: ExecuteCommandParams,
    ) -> Result<Option<Value>, String> {
        if matches!(p.command.as_str(), CONFIGURE | SHOW) {
            if let Some(arg) = p.arguments.first() {
                let doc = arg.as_str().ok_or("expected a document path")?;
                let path = match Url::parse(doc) {
                    Ok(uri) if uri.scheme() == "file" => uri_to_path(&uri)
                        .ok_or_else(|| format!("invalid document `{doc}`"))?,
                    _ => config::normalize_path(std::path::Path::new(doc)),
                };
                self.active = Some(path);
            }
        }
        match p.command.as_str() {
            CONFIGURE => self.configure_menu()?,
            SHOW => {
                let summary = self.summary()?;
                self.show_info(summary);
            }
            TOGGLE_DEFINE => {
                let arg = p
                    .arguments
                    .first()
                    .and_then(Value::as_str)
                    .ok_or("expected a macro name, e.g. \"MULTICAST\"")?;
                let (name, value) = config::parse_define(arg)?;
                let root = self.active_root()?;
                let defined = self.roots.roots[root]
                    .options
                    .defines
                    .iter()
                    .any(|(n, _)| *n == name);
                let choice = if defined && !arg.contains('=') {
                    Choice::Undefine(name)
                } else {
                    Choice::Define(name, value)
                };
                self.apply_choice(choice)?;
            }
            RESET => self.apply_choice(Choice::Reset)?,
            c => return Err(format!("unknown command `{c}`")),
        }
        Ok(None)
    }

    /// The root of the document the user last worked in.
    fn active_root(&mut self) -> Result<RootId, String> {
        let path = self.active.clone().ok_or("no P4 document is active")?;
        self.root_for(&path).ok_or_else(|| {
            format!("{} is not part of a P4 program", path.display())
        })
    }

    /// One line describing what the active document is analyzed with.
    fn summary(&mut self) -> Result<String, String> {
        let id = self.active_root()?;
        let path = self.active.clone().unwrap_or_default();
        let root = &self.roots.roots[id];
        let mut defines: Vec<String> = root
            .options
            .defines
            .iter()
            .map(|(n, v)| {
                if v == "1" {
                    n.clone()
                } else {
                    format!("{n}={v}")
                }
            })
            .collect();
        defines.sort();
        let defines = if defines.is_empty() {
            "no defines".to_string()
        } else {
            format!("defines {}", defines.join(", "))
        };
        let file = self.display_path(&root.file);
        let what = if root.file == path {
            format!("{file} is analyzed with {defines}")
        } else {
            format!(
                "{} is analyzed as part of {file} with {defines}",
                self.display_path(&path)
            )
        };
        let changed = *self.roots.overrides() != Overrides::default();
        Ok(if changed {
            format!("{what} (changed from the configuration)")
        } else {
            what
        })
    }

    /// Ask the client to pick a define to toggle or a root to switch to.
    fn configure_menu(&mut self) -> Result<(), String> {
        let id = self.active_root()?;
        let path = self.active.clone().unwrap_or_default();
        let mut choices: Vec<(String, Choice)> = Vec::new();

        // Macros the program tests in conditionals (in workspace files, so
        // not every guard in the architecture headers), plus those already
        // defined or changed.
        let root = &self.roots.roots[id];
        let mut names: BTreeSet<String> = BTreeSet::new();
        if let Some(a) = &root.analysis {
            let in_source: BTreeSet<&str> =
                a.pp.macros
                    .iter()
                    .filter(|m| m.span.is_some())
                    .map(|m| m.name.as_str())
                    .collect();
            let mut directives = Vec::new();
            for &(close, open) in &a.pp.conditionals {
                directives.push(open);
                directives.push(close);
            }
            for span in directives {
                let file = config::normalize_path(a.db.path(span.file));
                if !self.roots.in_workspace(&file) {
                    continue;
                }
                let text =
                    a.db.slice(span).trim_start_matches('#').trim_start();
                let Some(cond) = ["ifdef", "ifndef", "if", "elif"]
                    .iter()
                    .find_map(|k| text.strip_prefix(k))
                else {
                    continue;
                };
                for w in cond
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                {
                    let ident = w.starts_with(|c: char| {
                        c.is_ascii_alphabetic() || c == '_'
                    });
                    if ident && w != "defined" && !in_source.contains(w) {
                        names.insert(w.to_string());
                    }
                }
            }
        }
        names.extend(root.options.defines.iter().map(|(n, _)| n.clone()));
        let ov = self.roots.overrides().clone();
        names.extend(ov.undefine.iter().cloned());
        names.extend(ov.define.iter().map(|(n, _)| n.clone()));
        for name in names {
            let value = root
                .options
                .defines
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone());
            choices.push(match value {
                Some(v) if v == "1" => {
                    (format!("Undefine {name}"), Choice::Undefine(name))
                }
                Some(v) => {
                    (format!("Undefine {name} (= {v})"), Choice::Undefine(name))
                }
                None => {
                    (format!("Define {name}"), Choice::Define(name, "1".into()))
                }
            });
        }

        // Other roots that include this file.
        let overlays = self.overlays();
        let current_base = self.roots.roots[id].base.clone();
        for other in self.roots.choices(&path, &overlays, false) {
            if other == id {
                continue;
            }
            let r = &self.roots.roots[other];
            let has = |o: &crate::preprocess::Options, d: &(String, String)| {
                o.defines.contains(d)
            };
            let mut diff: Vec<String> = r
                .base
                .defines
                .iter()
                .filter(|d| !has(&current_base, d))
                .map(|(n, v)| {
                    if v == "1" {
                        n.clone()
                    } else {
                        format!("{n}={v}")
                    }
                })
                .collect();
            diff.extend(
                current_base
                    .defines
                    .iter()
                    .filter(|d| !has(&r.base, d))
                    .map(|(n, _)| format!("without {n}")),
            );
            let mut title = format!("Use root {}", self.display_path(&r.file));
            if !diff.is_empty() {
                title.push_str(&format!(" ({})", diff.join(", ")));
            }
            choices
                .push((title, Choice::Prefer(r.file.clone(), r.base.clone())));
        }
        if ov != Overrides::default() {
            choices.push(("Reset to the configuration".into(), Choice::Reset));
        }
        if choices.is_empty() {
            let summary = self.summary()?;
            self.show_info(format!("{summary}; nothing to change"));
            return Ok(());
        }
        // Helix shows the message in a box above the options; keep it a
        // short title (the options say what is defined).
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let params = ShowMessageRequestParams {
            typ: MessageType::INFO,
            message: format!("Configure x4c-lsp for {name}"),
            actions: Some(
                choices
                    .iter()
                    .map(|(title, _)| MessageActionItem {
                        title: title.clone(),
                        properties: Default::default(),
                    })
                    .collect(),
            ),
        };
        self.next_request += 1;
        let rid = RequestId::from(self.next_request);
        self.pending_choice = Some((rid.clone(), choices));
        self.send(
            Request::new(
                rid,
                request::ShowMessageRequest::METHOD.into(),
                params,
            )
            .into(),
        );
        Ok(())
    }

    /// The client's answer to the configuration menu.
    pub(super) fn menu_response(
        &mut self,
        id: &RequestId,
        result: Option<Value>,
    ) -> bool {
        if self
            .pending_choice
            .as_ref()
            .is_none_or(|(rid, _)| rid != id)
        {
            return false;
        }
        let (_, choices) = self.pending_choice.take().unwrap();
        let picked = result
            .and_then(|v| {
                serde_json::from_value::<Option<MessageActionItem>>(v).ok()
            })
            .flatten();
        if let Some(item) = picked {
            if let Some((_, c)) =
                choices.into_iter().find(|(t, _)| *t == item.title)
            {
                if let Err(e) = self.apply_choice(c) {
                    self.show_warning(e);
                }
            }
        }
        true
    }

    fn apply_choice(&mut self, choice: Choice) -> Result<(), String> {
        let mut ov = self.roots.overrides().clone();
        match choice {
            Choice::Define(n, v) => ov.set_define(&n, Some(&v)),
            Choice::Undefine(n) => ov.set_define(&n, None),
            Choice::Prefer(file, base) => ov.prefer = Some((file, base)),
            Choice::Reset => ov = Overrides::default(),
        }
        if self.roots.set_overrides(ov) {
            log!("configuration overrides: {:?}", self.roots.overrides());
            self.needs_publish = true;
        }
        let summary = self.summary()?;
        self.show_info(summary);
        Ok(())
    }

    fn show_info(&self, message: String) {
        log!("{message}");
        self.notify::<notification::ShowMessage>(ShowMessageParams {
            typ: MessageType::INFO,
            message: format!("x4c-lsp: {message}"),
        });
    }
}
