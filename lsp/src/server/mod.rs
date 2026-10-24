// Copyright 2026 Oxide Computer Company

//! Language Server Protocol front end.
//!
//! The server is single-threaded. Each turn of the main loop blocks for one
//! message, then drains every message already queued (so a burst of
//! keystrokes is applied together), then re-analyzes the roots affected by
//! the changes and publishes diagnostics. Requests re-analyze the root they
//! need before answering, so they always see the latest text.

mod document;
mod handlers;
mod roots;

use crate::config::{self, RawConfig};
use crate::diag::Severity;
use crate::source::{FileId, LineCol, PositionEncoding, SourceDb, Span};
use document::Document;
use lsp_server::{
    Connection, ErrorCode, Message, Notification, Request, RequestId, Response,
    ResponseError,
};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::{
    notification as notif, request as req, ClientCapabilities,
    InitializeParams, PositionEncodingKind, Url,
};
use roots::{Overlays, RootId, RootManager};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Diagnostic `source` reported to the client.
const SOURCE: &str = "x4c-lsp";

macro_rules! log {
    ($($arg:tt)*) => { eprintln!("x4c-lsp: {}", format_args!($($arg)*)) };
}

/// Run the server over stdio until the client sends `exit`.
///
/// Returns an error if the connection fails or the client exits without a
/// preceding `shutdown` request (the process should then exit with status
/// 1, as the specification requires).
pub fn run() -> Result<(), Error> {
    log!("starting version {}", env!("CARGO_PKG_VERSION"));
    let (conn, io_threads) = Connection::stdio();
    let result = serve(&conn);
    drop(conn);
    io_threads.join()?;
    log!("exiting");
    result
}

/// Run the server on an established connection.
pub fn serve(conn: &Connection) -> Result<(), Error> {
    let Some((id, params)) = wait_for_initialize(conn)? else {
        return Err("exit before initialize".into());
    };
    let mut server = Server::new(conn, params);
    let result = server.initialize_result();
    conn.sender
        .send(Response::new_ok(id, result).into())
        .map_err(|e| e.to_string())?;
    server.main_loop()
}

/// Wait for `initialize`, rejecting other requests. Returns `None` if the
/// client sends `exit` first.
fn wait_for_initialize(
    conn: &Connection,
) -> Result<Option<(RequestId, InitializeParams)>, Error> {
    loop {
        match conn.receiver.recv()? {
            Message::Request(r) if r.method == req::Initialize::METHOD => {
                match serde_json::from_value(r.params) {
                    Ok(params) => return Ok(Some((r.id, params))),
                    Err(e) => send_error(
                        conn,
                        r.id,
                        ErrorCode::InvalidParams,
                        format!("invalid initialize params: {e}"),
                    ),
                }
            }
            Message::Request(r) => send_error(
                conn,
                r.id,
                ErrorCode::ServerNotInitialized,
                "server not initialized".into(),
            ),
            Message::Notification(n) if n.method == notif::Exit::METHOD => {
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn send_error(conn: &Connection, id: RequestId, code: ErrorCode, msg: String) {
    let _ = conn
        .sender
        .send(Response::new_err(id, code as i32, msg).into());
}

/// What the client supports, as far as it affects our responses.
#[derive(Debug, Default)]
struct ClientCaps {
    hierarchical_symbols: bool,
    related_information: bool,
    watch_files: bool,
    snippets: bool,
}

/// The last diagnostics published for one file.
struct Published {
    version: Option<i32>,
    diagnostics: Vec<lsp_types::Diagnostic>,
}

pub struct Server<'a> {
    conn: &'a Connection,
    enc: PositionEncoding,
    caps: ClientCaps,
    docs: HashMap<PathBuf, Document>,
    roots: RootManager,
    published: HashMap<PathBuf, Published>,
    /// Set when something may have changed diagnostics.
    needs_publish: bool,
    shutdown: bool,
    next_request: i32,
}

impl<'a> Server<'a> {
    fn new(conn: &'a Connection, params: InitializeParams) -> Self {
        let enc = negotiate_encoding(&params.capabilities);
        let caps = client_caps(&params.capabilities);
        let mut roots = RootManager::new(Box::new(config::process_env));
        roots.workspace_folders = workspace_folders(&params);
        log!(
            "position encoding {enc:?}; workspace folders {:?}; \
             fallback include dirs {:?}",
            roots.workspace_folders,
            roots.fallback_includes(),
        );
        let mut server = Self {
            conn,
            enc,
            caps,
            docs: HashMap::new(),
            roots,
            published: HashMap::new(),
            needs_publish: false,
            shutdown: false,
            next_request: 0,
        };
        if let Some(opts) = &params.initialization_options {
            server.set_client_config(opts);
        }
        server
    }

    fn initialize_result(&self) -> Value {
        use lsp_types::*;
        let encoding = match self.enc {
            PositionEncoding::Utf8 => PositionEncodingKind::UTF8,
            PositionEncoding::Utf16 => PositionEncodingKind::UTF16,
            PositionEncoding::Utf32 => PositionEncodingKind::UTF32,
        };
        let capabilities = ServerCapabilities {
            position_encoding: Some(encoding),
            text_document_sync: Some(TextDocumentSyncCapability::Options(
                TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: Some(TextDocumentSyncKind::INCREMENTAL),
                    save: Some(TextDocumentSyncSaveOptions::SaveOptions(
                        SaveOptions {
                            include_text: Some(false),
                        },
                    )),
                    ..Default::default()
                },
            )),
            definition_provider: Some(OneOf::Left(true)),
            declaration_provider: Some(DeclarationCapability::Simple(true)),
            type_definition_provider: Some(
                TypeDefinitionProviderCapability::Simple(true),
            ),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            references_provider: Some(OneOf::Left(true)),
            document_highlight_provider: Some(OneOf::Left(true)),
            document_symbol_provider: Some(OneOf::Left(true)),
            workspace_symbol_provider: Some(OneOf::Left(true)),
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(vec![
                    ".".into(),
                    "<".into(),
                    "\"".into(),
                    "/".into(),
                ]),
                resolve_provider: Some(false),
                ..Default::default()
            }),
            signature_help_provider: Some(SignatureHelpOptions {
                trigger_characters: Some(vec!["(".into(), ",".into()]),
                retrigger_characters: Some(vec![",".into()]),
                ..Default::default()
            }),
            code_action_provider: Some(CodeActionProviderCapability::Options(
                CodeActionOptions {
                    code_action_kinds: Some(vec![
                        CodeActionKind::QUICKFIX,
                        CodeActionKind::REFACTOR,
                        CodeActionKind::REFACTOR_REWRITE,
                        CodeActionKind::REFACTOR_INLINE,
                    ]),
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                },
            )),
            inlay_hint_provider: Some(OneOf::Left(true)),
            rename_provider: Some(OneOf::Right(RenameOptions {
                prepare_provider: Some(true),
                work_done_progress_options: Default::default(),
            })),
            workspace: Some(WorkspaceServerCapabilities {
                workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                    supported: Some(true),
                    change_notifications: Some(OneOf::Left(true)),
                }),
                file_operations: None,
            }),
            ..Default::default()
        };
        serde_json::json!({
            "capabilities": capabilities,
            "serverInfo": {
                "name": "x4c-lsp",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })
    }

    fn main_loop(&mut self) -> Result<(), Error> {
        loop {
            let msg = self.conn.receiver.recv()?;
            if self.handle(msg) {
                return self.exit_result();
            }
            while let Ok(msg) = self.conn.receiver.try_recv() {
                if self.handle(msg) {
                    return self.exit_result();
                }
            }
            if !self.shutdown {
                self.flush();
            }
        }
    }

    fn exit_result(&self) -> Result<(), Error> {
        if self.shutdown {
            Ok(())
        } else {
            Err("received exit without shutdown".into())
        }
    }

    /// Handle one message. Returns `true` on `exit`.
    fn handle(&mut self, msg: Message) -> bool {
        match msg {
            Message::Request(r) => {
                let id = r.id.clone();
                let resp = match self.handle_request(r) {
                    Ok(v) => Response::new_ok(id, v),
                    Err(e) => Response {
                        id,
                        result: None,
                        error: Some(e),
                    },
                };
                self.send(resp.into());
                false
            }
            Message::Notification(n) => {
                if n.method == notif::Exit::METHOD {
                    return true;
                }
                if !self.shutdown {
                    self.handle_notification(n);
                }
                false
            }
            Message::Response(r) => {
                if let Some(e) = r.error {
                    log!("client returned error for {:?}: {}", r.id, e.message);
                }
                false
            }
        }
    }

    fn handle_request(&mut self, r: Request) -> Result<Value, ResponseError> {
        if r.method == req::Shutdown::METHOD {
            self.shutdown = true;
            return Ok(Value::Null);
        }
        if self.shutdown {
            return Err(response_error(
                ErrorCode::InvalidRequest,
                "server is shutting down".into(),
            ));
        }
        match r.method.as_str() {
            req::GotoDefinition::METHOD => {
                self.dispatch::<req::GotoDefinition>(r, Self::definition)
            }
            req::GotoDeclaration::METHOD => {
                self.dispatch::<req::GotoDeclaration>(r, Self::definition)
            }
            req::GotoTypeDefinition::METHOD => self
                .dispatch::<req::GotoTypeDefinition>(r, Self::type_definition),
            req::HoverRequest::METHOD => {
                self.dispatch::<req::HoverRequest>(r, Self::hover)
            }
            req::References::METHOD => {
                self.dispatch::<req::References>(r, Self::references)
            }
            req::DocumentHighlightRequest::METHOD => {
                self.dispatch::<req::DocumentHighlightRequest>(
                    r,
                    Self::document_highlight,
                )
            }
            req::DocumentSymbolRequest::METHOD => self
                .dispatch::<req::DocumentSymbolRequest>(
                r,
                Self::document_symbols,
            ),
            req::WorkspaceSymbolRequest::METHOD => self
                .dispatch::<req::WorkspaceSymbolRequest>(
                r,
                Self::workspace_symbols,
            ),
            req::Completion::METHOD => {
                self.dispatch::<req::Completion>(r, Self::completion)
            }
            req::SignatureHelpRequest::METHOD => self
                .dispatch::<req::SignatureHelpRequest>(r, Self::signature_help),
            req::PrepareRenameRequest::METHOD => self
                .dispatch::<req::PrepareRenameRequest>(r, Self::prepare_rename),
            req::Rename::METHOD => {
                self.dispatch::<req::Rename>(r, Self::rename)
            }
            req::CodeActionRequest::METHOD => {
                self.dispatch::<req::CodeActionRequest>(r, Self::code_action)
            }
            req::InlayHintRequest::METHOD => {
                self.dispatch::<req::InlayHintRequest>(r, Self::inlay_hints)
            }
            m => Err(response_error(
                ErrorCode::MethodNotFound,
                format!("unknown request `{m}`"),
            )),
        }
    }

    /// Decode params, run a handler, and encode its result. Malformed
    /// params yield `InvalidParams`; a panicking handler yields
    /// `InternalError` instead of taking the server down.
    fn dispatch<R: req::Request>(
        &mut self,
        r: Request,
        f: fn(&mut Self, R::Params) -> Result<R::Result, String>,
    ) -> Result<Value, ResponseError> {
        let params: R::Params =
            serde_json::from_value(r.params).map_err(|e| {
                response_error(
                    ErrorCode::InvalidParams,
                    format!("invalid params for `{}`: {e}", R::METHOD),
                )
            })?;
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                f(self, params)
            }));
        match result {
            Ok(Ok(v)) => Ok(serde_json::to_value(v).unwrap_or(Value::Null)),
            Ok(Err(msg)) => Err(response_error(ErrorCode::RequestFailed, msg)),
            Err(_) => Err(response_error(
                ErrorCode::InternalError,
                format!("internal error handling `{}`", R::METHOD),
            )),
        }
    }

    fn handle_notification(&mut self, n: Notification) {
        macro_rules! params {
            ($ty:ty) => {
                match serde_json::from_value::<
                    <$ty as lsp_types::notification::Notification>::Params,
                >(n.params)
                {
                    Ok(p) => p,
                    Err(e) => {
                        log!("invalid params for `{}`: {e}", n.method);
                        return;
                    }
                }
            };
        }
        match n.method.as_str() {
            notif::Initialized::METHOD => self.register_watchers(),
            notif::DidOpenTextDocument::METHOD => {
                let p = params!(notif::DidOpenTextDocument);
                let doc = p.text_document;
                let Some(path) = uri_to_path(&doc.uri) else {
                    return;
                };
                self.docs.insert(
                    path.clone(),
                    Document::new(doc.uri, doc.version, doc.text),
                );
                self.text_changed(&path);
            }
            notif::DidChangeTextDocument::METHOD => {
                let p = params!(notif::DidChangeTextDocument);
                let Some(path) = uri_to_path(&p.text_document.uri) else {
                    return;
                };
                let Some(doc) = self.docs.get_mut(&path) else {
                    log!("change to unopened document {}", path.display());
                    return;
                };
                doc.apply_changes(
                    p.text_document.version,
                    p.content_changes,
                    self.enc,
                );
                self.text_changed(&path);
            }
            notif::DidSaveTextDocument::METHOD => {
                let p = params!(notif::DidSaveTextDocument);
                if let Some(path) = uri_to_path(&p.text_document.uri) {
                    // The buffer is already authoritative; saving can only
                    // change which files in its directory are programs.
                    if let Some(dir) = path.parent() {
                        self.roots.invalidate_dir(dir);
                    }
                    self.needs_publish = true;
                }
            }
            notif::DidCloseTextDocument::METHOD => {
                let p = params!(notif::DidCloseTextDocument);
                if let Some(path) = uri_to_path(&p.text_document.uri) {
                    if self.docs.remove(&path).is_some() {
                        // Fall back to the on-disk contents.
                        self.text_changed(&path);
                    }
                }
            }
            notif::DidChangeConfiguration::METHOD => {
                let p = params!(notif::DidChangeConfiguration);
                self.set_client_config(&p.settings);
            }
            notif::DidChangeWatchedFiles::METHOD => {
                let p = params!(notif::DidChangeWatchedFiles);
                self.files_changed_on_disk(p.changes);
            }
            notif::DidChangeWorkspaceFolders::METHOD => {
                let p = params!(notif::DidChangeWorkspaceFolders);
                let removed: HashSet<PathBuf> = p
                    .event
                    .removed
                    .iter()
                    .filter_map(|f| uri_to_path(&f.uri))
                    .collect();
                let folders = &mut self.roots.workspace_folders;
                folders.retain(|f| !removed.contains(f));
                for f in &p.event.added {
                    if let Some(path) = uri_to_path(&f.uri) {
                        if !folders.contains(&path) {
                            folders.push(path);
                        }
                    }
                }
                self.roots.reset();
                self.needs_publish = true;
            }
            // `$/cancelRequest`, `$/setTrace`, and anything else: requests
            // are answered in order, so there is nothing to cancel.
            _ => {}
        }
    }

    fn text_changed(&mut self, path: &Path) {
        if !self.roots.file_changed(path) {
            // Possibly a file an unresolved `#include` was looking for.
            self.roots.mark_all_dirty();
        }
        self.needs_publish = true;
    }

    fn files_changed_on_disk(&mut self, changes: Vec<lsp_types::FileEvent>) {
        let mut config_changed = false;
        for change in changes {
            let Some(path) = uri_to_path(&change.uri) else {
                continue;
            };
            if RootManager::is_config_file(&path) {
                config_changed = true;
                continue;
            }
            if let Some(dir) = path.parent() {
                self.roots.invalidate_dir(dir);
            }
            // Open buffers take precedence over the disk.
            if !self.docs.contains_key(&path) && !self.roots.file_changed(&path)
            {
                self.roots.mark_all_dirty();
            }
        }
        if config_changed {
            log!("configuration file changed; reloading");
            self.roots.reset();
        }
        self.needs_publish = true;
    }

    fn set_client_config(&mut self, settings: &Value) {
        match RawConfig::from_json(settings) {
            Ok(cfg) => {
                if self.roots.set_client_config(cfg) {
                    log!("client configuration updated");
                    self.needs_publish = true;
                }
            }
            Err(e) => self.show_warning(format!("invalid settings: {e}")),
        }
    }

    /// Ask the client to watch P4 sources and config files, if it supports
    /// dynamic registration.
    fn register_watchers(&mut self) {
        if !self.caps.watch_files {
            return;
        }
        use lsp_types::*;
        let watcher = |glob: &str| FileSystemWatcher {
            glob_pattern: GlobPattern::String(glob.into()),
            kind: None,
        };
        let options = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![
                watcher("**/*.p4"),
                watcher(&format!("**/{}", config::CONFIG_FILE_NAME)),
            ],
        };
        let params = RegistrationParams {
            registrations: vec![Registration {
                id: "x4c-lsp-watched-files".into(),
                method: notif::DidChangeWatchedFiles::METHOD.into(),
                register_options: serde_json::to_value(options).ok(),
            }],
        };
        self.next_request += 1;
        let r = Request::new(
            RequestId::from(self.next_request),
            req::RegisterCapability::METHOD.into(),
            params,
        );
        self.send(r.into());
    }

    fn send(&self, msg: Message) {
        if let Err(e) = self.conn.sender.send(msg) {
            log!("failed to send message: {e}");
        }
    }

    fn notify<N: lsp_types::notification::Notification>(
        &self,
        params: N::Params,
    ) {
        self.send(Notification::new(N::METHOD.into(), params).into());
    }

    fn show_warning(&self, message: String) {
        log!("{message}");
        self.notify::<notif::ShowMessage>(lsp_types::ShowMessageParams {
            typ: lsp_types::MessageType::WARNING,
            message: format!("x4c-lsp: {message}"),
        });
    }

    fn overlays(&mut self) -> Overlays {
        self.docs
            .iter_mut()
            .map(|(path, doc)| (path.clone(), doc.shared()))
            .collect()
    }

    /// The up-to-date root used for queries about `path`.
    fn root_for(&mut self, path: &Path) -> Option<RootId> {
        let overlays = self.overlays();
        let id = self.roots.root_for(path, &overlays)?;
        self.roots.refresh(id, &overlays);
        for w in self.roots.take_warnings() {
            self.show_warning(w);
        }
        Some(id)
    }

    /// Re-analyze active roots and publish diagnostics, if anything changed.
    fn flush(&mut self) {
        if !self.needs_publish {
            return;
        }
        self.needs_publish = false;
        let overlays = self.overlays();
        let mut open: Vec<PathBuf> = self.docs.keys().cloned().collect();
        open.sort();

        // Assign open documents to roots and bring those roots up to date.
        // Analyses can change closures and hence assignments; repeat until
        // stable (bounded, in case of pathological flip-flopping).
        let mut owner: BTreeMap<PathBuf, RootId> = BTreeMap::new();
        for _ in 0..3 {
            owner.clear();
            for path in &open {
                if let Some(id) = self.roots.root_for(path, &overlays) {
                    self.roots.refresh(id, &overlays);
                    owner.insert(path.clone(), id);
                }
            }
            let stable = open.iter().all(|p| {
                self.roots.root_for(p, &overlays) == owner.get(p).copied()
            });
            if stable {
                break;
            }
        }
        for w in self.roots.take_warnings() {
            self.show_warning(w);
        }

        // Unopened workspace files in an active root's closure get that
        // root's diagnostics; the first active root wins.
        let mut active: Vec<RootId> = owner.values().copied().collect();
        active.sort();
        active.dedup();
        for &id in &active {
            for path in self.roots.roots[id].files.keys() {
                if !self.docs.contains_key(path)
                    && self.roots.in_workspace(path)
                {
                    owner.entry(path.clone()).or_insert(id);
                }
            }
        }

        let mut seen = HashSet::new();
        for (path, id) in &owner {
            let root = &self.roots.roots[*id];
            let (Some(analysis), Some(file)) =
                (&root.analysis, root.file_id(path))
            else {
                continue;
            };
            let diagnostics = self.convert_diagnostics(
                &analysis.db,
                file,
                analysis.diagnostics(),
            );
            let version = self.docs.get(path).map(|d| d.version);
            seen.insert(path.clone());
            self.publish(path, version, diagnostics);
        }
        let stale: Vec<PathBuf> = self
            .published
            .keys()
            .filter(|p| !seen.contains(*p))
            .cloned()
            .collect();
        for path in stale {
            let version = self.docs.get(&path).map(|d| d.version);
            self.publish(&path, version, Vec::new());
            self.published.remove(&path);
        }
    }

    /// Publish diagnostics for `path` unless the client already has them.
    fn publish(
        &mut self,
        path: &Path,
        version: Option<i32>,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) {
        if let Some(p) = self.published.get(path) {
            if p.version == version && p.diagnostics == diagnostics {
                return;
            }
        } else if diagnostics.is_empty() {
            // Nothing was published and nothing is to be cleared.
            return;
        }
        let Some(uri) = self.uri_for(path) else {
            return;
        };
        self.notify::<notif::PublishDiagnostics>(
            lsp_types::PublishDiagnosticsParams {
                uri,
                diagnostics: diagnostics.clone(),
                version,
            },
        );
        self.published.insert(
            path.to_path_buf(),
            Published {
                version,
                diagnostics,
            },
        );
    }

    fn convert_diagnostics(
        &self,
        db: &SourceDb,
        file: FileId,
        diagnostics: &[crate::diag::Diagnostic],
    ) -> Vec<lsp_types::Diagnostic> {
        use lsp_types::{
            DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag,
        };
        let inactive = self.roots.inactive_regions();
        diagnostics
            .iter()
            .filter(|d| {
                d.span.file == file && (inactive || d.code != Some("inactive"))
            })
            .map(|d| {
                let mut message = d.message.clone();
                let mut related = Vec::new();
                for (span, msg) in &d.related {
                    let Some(location) = self.location(db, *span) else {
                        continue;
                    };
                    if self.caps.related_information {
                        related.push(DiagnosticRelatedInformation {
                            location,
                            message: msg.clone(),
                        });
                    } else {
                        // Clients without related-information support
                        // (e.g. Helix) would drop it; fold it into the
                        // message instead.
                        let lc = db.line_col(span.file, span.start, self.enc);
                        message.push_str(&format!(
                            "\n{}:{}:{}: {msg}",
                            self.display_path(db.path(span.file)),
                            lc.line + 1,
                            lc.col + 1,
                        ));
                    }
                }
                lsp_types::Diagnostic {
                    range: self.range(db, d.span),
                    severity: Some(match d.severity {
                        Severity::Error => DiagnosticSeverity::ERROR,
                        Severity::Warning => DiagnosticSeverity::WARNING,
                        Severity::Info => DiagnosticSeverity::INFORMATION,
                        Severity::Hint => DiagnosticSeverity::HINT,
                    }),
                    source: Some(SOURCE.into()),
                    code: d
                        .code
                        .map(|c| lsp_types::NumberOrString::String(c.into())),
                    message,
                    related_information: (!related.is_empty())
                        .then_some(related),
                    tags: d
                        .unnecessary
                        .then(|| vec![DiagnosticTag::UNNECESSARY]),
                    ..Default::default()
                }
            })
            .collect()
    }

    /// A path for messages: relative to its workspace folder if inside one.
    fn display_path(&self, path: &Path) -> String {
        let path = config::normalize_path(path);
        match self.roots.workspace_folder(&path) {
            Some(w) => path.strip_prefix(w).unwrap_or(&path).display(),
            None => path.display(),
        }
        .to_string()
    }

    /// The URI to report for a file: the client's own URI for open
    /// documents, otherwise a `file://` URI of the normalized path.
    fn uri_for(&self, path: &Path) -> Option<Url> {
        match self.docs.get(path) {
            Some(doc) => Some(doc.uri.clone()),
            None => Url::from_file_path(path).ok(),
        }
    }

    fn range(&self, db: &SourceDb, span: Span) -> lsp_types::Range {
        lsp_types::Range {
            start: self.position(db, span.file, span.start),
            end: self.position(db, span.file, span.end),
        }
    }

    fn position(
        &self,
        db: &SourceDb,
        file: FileId,
        offset: u32,
    ) -> lsp_types::Position {
        let lc = db.line_col(file, offset, self.enc);
        lsp_types::Position::new(lc.line, lc.col)
    }

    fn offset(
        &self,
        db: &SourceDb,
        file: FileId,
        pos: lsp_types::Position,
    ) -> u32 {
        db.offset(
            file,
            LineCol {
                line: pos.line,
                col: pos.character,
            },
            self.enc,
        )
    }

    fn location(
        &self,
        db: &SourceDb,
        span: Span,
    ) -> Option<lsp_types::Location> {
        let path = config::normalize_path(db.path(span.file));
        Some(lsp_types::Location {
            uri: self.uri_for(&path)?,
            range: self.range(db, span),
        })
    }
}

fn response_error(code: ErrorCode, message: String) -> ResponseError {
    ResponseError {
        code: code as i32,
        message,
        data: None,
    }
}

/// Prefer UTF-8 (byte offsets need no conversion), then UTF-32, and fall
/// back to the mandatory UTF-16.
pub fn negotiate_encoding(caps: &ClientCapabilities) -> PositionEncoding {
    let offered = caps
        .general
        .as_ref()
        .and_then(|g| g.position_encodings.as_deref())
        .unwrap_or_default();
    if offered.contains(&PositionEncodingKind::UTF8) {
        PositionEncoding::Utf8
    } else if offered.contains(&PositionEncodingKind::UTF32) {
        PositionEncoding::Utf32
    } else {
        PositionEncoding::Utf16
    }
}

fn client_caps(caps: &ClientCapabilities) -> ClientCaps {
    let text = caps.text_document.as_ref();
    ClientCaps {
        hierarchical_symbols: text
            .and_then(|t| t.document_symbol.as_ref())
            .and_then(|s| s.hierarchical_document_symbol_support)
            .unwrap_or(false),
        related_information: text
            .and_then(|t| t.publish_diagnostics.as_ref())
            .and_then(|d| d.related_information)
            .unwrap_or(false),
        watch_files: caps
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref())
            .and_then(|d| d.dynamic_registration)
            .unwrap_or(false),
        snippets: text
            .and_then(|t| t.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|i| i.snippet_support)
            .unwrap_or(false),
    }
}

#[allow(deprecated)]
fn workspace_folders(params: &InitializeParams) -> Vec<PathBuf> {
    if let Some(folders) = &params.workspace_folders {
        return folders.iter().filter_map(|f| uri_to_path(&f.uri)).collect();
    }
    if let Some(uri) = &params.root_uri {
        return uri_to_path(uri).into_iter().collect();
    }
    params
        .root_path
        .as_ref()
        .map(|p| vec![config::normalize_path(Path::new(p))])
        .unwrap_or_default()
}

/// Convert a `file://` URI to a normalized path.
pub fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    if uri.scheme() != "file" {
        return None;
    }
    let path = uri.to_file_path().ok()?;
    Some(config::normalize_path(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_negotiation() {
        let caps =
            |encs: Option<Vec<PositionEncodingKind>>| ClientCapabilities {
                general: Some(lsp_types::GeneralClientCapabilities {
                    position_encodings: encs,
                    ..Default::default()
                }),
                ..Default::default()
            };
        use PositionEncodingKind as K;
        assert_eq!(
            negotiate_encoding(&caps(Some(vec![K::UTF16, K::UTF8]))),
            PositionEncoding::Utf8
        );
        assert_eq!(
            negotiate_encoding(&caps(Some(vec![K::UTF16, K::UTF32]))),
            PositionEncoding::Utf32
        );
        assert_eq!(
            negotiate_encoding(&caps(Some(vec![K::UTF16]))),
            PositionEncoding::Utf16
        );
        assert_eq!(negotiate_encoding(&caps(None)), PositionEncoding::Utf16);
        assert_eq!(
            negotiate_encoding(&ClientCapabilities::default()),
            PositionEncoding::Utf16
        );
    }

    #[test]
    fn uri_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        std::fs::write(canon.join("a.p4"), "").unwrap();
        // A non-canonical spelling (on macOS the temp dir is typically
        // behind the /var -> /private/var symlink) maps to the canonical
        // path.
        let uri = Url::from_file_path(dir.path().join("./x/../a.p4")).unwrap();
        assert_eq!(uri_to_path(&uri), Some(canon.join("a.p4")));
        // Percent-encoded characters are decoded.
        std::fs::write(canon.join("b c.p4"), "").unwrap();
        let uri = Url::from_file_path(canon.join("b c.p4")).unwrap();
        assert!(uri.as_str().contains("b%20c.p4"));
        assert_eq!(uri_to_path(&uri), Some(canon.join("b c.p4")));
        // Round trip.
        let back = Url::from_file_path(uri_to_path(&uri).unwrap()).unwrap();
        assert_eq!(back, uri);
        // Non-file URIs are not documents we can analyze.
        let uri = Url::parse("untitled:Untitled-1").unwrap();
        assert_eq!(uri_to_path(&uri), None);
    }
}
