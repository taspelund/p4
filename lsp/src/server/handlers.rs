// Copyright 2026 Oxide Computer Company

//! Request handlers: translate LSP requests into [`Analysis`] queries.

use super::roots::RootId;
use super::{uri_to_path, Server};
use crate::analysis::{self, Analysis};
use crate::source::FileId;
use lsp_types::*;
use std::collections::{HashMap, HashSet};

type Result<T> = std::result::Result<T, String>;

/// Where a request points: the analysis answering it, and the file within
/// that analysis.
struct Target {
    root: RootId,
    file: FileId,
}

impl Server<'_> {
    /// Find the analysis for a document. `None` for documents that belong
    /// to no root (e.g. non-P4 files), which get empty results.
    fn target(&mut self, uri: &Url) -> Option<Target> {
        let path = uri_to_path(uri)?;
        self.set_active(&path);
        let root = self.root_for(&path)?;
        let r = &self.roots.roots[root];
        r.analysis.as_ref()?;
        Some(Target {
            root,
            file: r.file_id(&path)?,
        })
    }

    fn analysis(&self, t: &Target) -> &Analysis {
        self.roots.roots[t.root]
            .analysis
            .as_ref()
            .expect("target has an analysis")
    }

    /// Resolve a document position to an analysis, file, and byte offset.
    fn at(&mut self, p: &TextDocumentPositionParams) -> Option<(Target, u32)> {
        let t = self.target(&p.text_document.uri)?;
        let offset = self.offset(&self.analysis(&t).db, t.file, p.position);
        Some((t, offset))
    }

    fn locations(
        &self,
        a: &Analysis,
        spans: Vec<crate::source::Span>,
    ) -> Vec<Location> {
        spans
            .into_iter()
            .filter_map(|s| self.location(&a.db, s))
            .collect()
    }

    pub(super) fn definition(
        &mut self,
        p: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let Some((t, offset)) = self.at(&p.text_document_position_params)
        else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let locs = self.locations(a, a.goto_definition(t.file, offset));
        Ok(Some(GotoDefinitionResponse::Array(locs)))
    }

    pub(super) fn type_definition(
        &mut self,
        p: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let Some((t, offset)) = self.at(&p.text_document_position_params)
        else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let locs = self.locations(a, a.goto_type_definition(t.file, offset));
        Ok(Some(GotoDefinitionResponse::Array(locs)))
    }

    pub(super) fn implementation(
        &mut self,
        p: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let Some((t, offset)) = self.at(&p.text_document_position_params)
        else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let locs = self.locations(a, a.goto_implementation(t.file, offset));
        Ok(Some(GotoDefinitionResponse::Array(locs)))
    }

    pub(super) fn hover(&mut self, p: HoverParams) -> Result<Option<Hover>> {
        let Some((t, offset)) = self.at(&p.text_document_position_params)
        else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        Ok(a.hover(t.file, offset).map(|h| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: h.markdown,
            }),
            range: (h.span.file == t.file).then(|| self.range(&a.db, h.span)),
        }))
    }

    pub(super) fn references(
        &mut self,
        p: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let Some((t, offset)) = self.at(&p.text_document_position) else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let spans = a.references(t.file, offset, p.context.include_declaration);
        Ok(Some(self.locations(a, spans)))
    }

    pub(super) fn document_highlight(
        &mut self,
        p: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let Some((t, offset)) = self.at(&p.text_document_position_params)
        else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let highlights = a
            .document_highlight(t.file, offset)
            .into_iter()
            .filter(|(s, _)| s.file == t.file)
            .map(|(s, kind)| DocumentHighlight {
                range: self.range(&a.db, s),
                kind: Some(match kind {
                    analysis::HighlightKind::Read => {
                        DocumentHighlightKind::READ
                    }
                    analysis::HighlightKind::Write => {
                        DocumentHighlightKind::WRITE
                    }
                }),
            })
            .collect();
        Ok(Some(highlights))
    }

    /// Hierarchical symbols when the client supports them; otherwise a
    /// flat list with container names (Helix 25.07 flattens hierarchical
    /// symbols and drops their nesting).
    pub(super) fn document_symbols(
        &mut self,
        p: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(t) = self.target(&p.text_document.uri) else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let symbols = a.document_symbols(t.file);
        if self.caps.hierarchical_symbols {
            let nested = symbols
                .into_iter()
                .filter(|s| s.span.file == t.file)
                .map(|s| self.nested_symbol(a, s))
                .collect();
            return Ok(Some(DocumentSymbolResponse::Nested(nested)));
        }
        let mut flat = Vec::new();
        let uri = p.text_document.uri;
        for s in symbols {
            self.flatten_symbol(a, &uri, s, None, t.file, &mut flat);
        }
        Ok(Some(DocumentSymbolResponse::Flat(flat)))
    }

    #[allow(deprecated)]
    fn nested_symbol(
        &self,
        a: &Analysis,
        s: analysis::DocumentSymbol,
    ) -> DocumentSymbol {
        let children: Vec<_> = s
            .children
            .into_iter()
            .filter(|c| c.span.file == s.span.file)
            .map(|c| self.nested_symbol(a, c))
            .collect();
        let range = self.range(&a.db, s.span);
        let mut selection_range = self.range(&a.db, s.selection);
        // The selection range must be contained in the full range.
        if s.selection.file != s.span.file
            || s.selection.start < s.span.start
            || s.selection.end > s.span.end
        {
            selection_range = range;
        }
        DocumentSymbol {
            name: s.name,
            detail: s.detail,
            kind: symbol_kind(s.kind),
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children: (!children.is_empty()).then_some(children),
        }
    }

    #[allow(deprecated)]
    fn flatten_symbol(
        &self,
        a: &Analysis,
        uri: &Url,
        s: analysis::DocumentSymbol,
        container: Option<&str>,
        file: FileId,
        out: &mut Vec<SymbolInformation>,
    ) {
        if s.selection.file == file {
            out.push(SymbolInformation {
                name: s.name.clone(),
                kind: symbol_kind(s.kind),
                tags: None,
                deprecated: None,
                location: Location::new(
                    uri.clone(),
                    self.range(&a.db, s.selection),
                ),
                container_name: container.map(str::to_string),
            });
        }
        for c in s.children {
            self.flatten_symbol(a, uri, c, Some(&s.name), file, out);
        }
    }

    /// Flat `SymbolInformation` results: Helix 25.07 discards the
    /// `WorkspaceSymbol` variant.
    #[allow(deprecated)]
    pub(super) fn workspace_symbols(
        &mut self,
        p: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let mut paths: Vec<_> = self.docs.keys().cloned().collect();
        paths.sort();
        let mut roots = Vec::new();
        for path in paths {
            if let Some(id) = self.root_for(&path) {
                if !roots.contains(&id) {
                    roots.push(id);
                }
            }
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in roots {
            let Some(a) = &self.roots.roots[id].analysis else {
                continue;
            };
            for s in a.workspace_symbols(&p.query) {
                let path =
                    crate::config::normalize_path(a.db.path(s.span.file));
                if !seen.insert((path, s.span.start, s.name.clone())) {
                    continue;
                }
                let Some(location) = self.location(&a.db, s.span) else {
                    continue;
                };
                out.push(SymbolInformation {
                    name: s.name,
                    kind: symbol_kind(s.kind),
                    tags: None,
                    deprecated: None,
                    location,
                    container_name: s.container,
                });
            }
        }
        Ok(Some(WorkspaceSymbolResponse::Flat(out)))
    }

    pub(super) fn completion(
        &mut self,
        p: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let Some((t, offset)) = self.at(&p.text_document_position) else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let snippets = self.caps.snippets;
        // `<`, `"`, and `/` trigger completion only for `#include` paths.
        let paths_only = p
            .context
            .as_ref()
            .and_then(|c| c.trigger_character.as_deref())
            .is_some_and(|c| c != ".");
        let items = a
            .completions(t.file, offset)
            .into_iter()
            .filter(|c| snippets || c.kind != analysis::SymbolKind::Snippet)
            .filter(|c| {
                !paths_only
                    || matches!(
                        c.kind,
                        analysis::SymbolKind::File
                            | analysis::SymbolKind::Folder
                    )
            })
            .enumerate()
            // Keep the analysis's order (fields in declaration order, inner
            // scopes first) when the client sorts by `sortText`.
            .map(|(i, c)| CompletionItem {
                sort_text: Some(format!("{i:05}")),
                additional_text_edits: (!c.additional_edits.is_empty()).then(
                    || {
                        c.additional_edits
                            .iter()
                            .map(|(s, t)| {
                                TextEdit::new(self.range(&a.db, *s), t.clone())
                            })
                            .collect()
                    },
                ),
                insert_text_format: (c.snippet && snippets)
                    .then_some(InsertTextFormat::SNIPPET),
                text_edit: match (c.replace, &c.insert) {
                    (Some(span), Some(text)) => Some(CompletionTextEdit::Edit(
                        TextEdit::new(self.range(&a.db, span), text.clone()),
                    )),
                    _ => None,
                },
                insert_text: c.insert.filter(|_| {
                    c.replace.is_none() && (snippets || !c.snippet)
                }),
                filter_text: c.replace.is_some().then(|| c.label.clone()),
                label: c.label,
                kind: Some(completion_kind(c.kind)),
                detail: c.detail,
                documentation: c.documentation.map(|d| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: d,
                    })
                }),
                ..Default::default()
            })
            .collect();
        Ok(Some(CompletionResponse::Array(items)))
    }

    pub(super) fn signature_help(
        &mut self,
        p: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let Some((t, offset)) = self.at(&p.text_document_position_params)
        else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let Some(help) = a.signature_help(t.file, offset) else {
            return Ok(None);
        };
        let signatures = help
            .signatures
            .into_iter()
            .map(|s| {
                let parameters = s
                    .params
                    .iter()
                    .map(|&(start, end)| ParameterInformation {
                        label: ParameterLabel::LabelOffsets([
                            utf16_offset(&s.label, start),
                            utf16_offset(&s.label, end),
                        ]),
                        documentation: None,
                    })
                    .collect();
                SignatureInformation {
                    label: s.label,
                    documentation: s.documentation.map(|d| {
                        Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: d,
                        })
                    }),
                    parameters: Some(parameters),
                    active_parameter: None,
                }
            })
            .collect();
        Ok(Some(SignatureHelp {
            signatures,
            active_signature: Some(help.active_signature),
            active_parameter: Some(help.active_param),
        }))
    }

    pub(super) fn prepare_rename(
        &mut self,
        p: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let Some((t, offset)) = self.at(&p) else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        Ok(a.prepare_rename(t.file, offset)
            .filter(|s| s.file == t.file)
            .map(|s| PrepareRenameResponse::Range(self.range(&a.db, s))))
    }

    pub(super) fn rename(
        &mut self,
        p: RenameParams,
    ) -> Result<Option<WorkspaceEdit>> {
        let Some((t, offset)) = self.at(&p.text_document_position) else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let edits = a.rename(t.file, offset, &p.new_name)?;
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for (span, text) in edits {
            let Some(loc) = self.location(&a.db, span) else {
                continue;
            };
            changes
                .entry(loc.uri)
                .or_default()
                .push(TextEdit::new(loc.range, text));
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    pub(super) fn code_action(
        &mut self,
        p: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let Some(t) = self.target(&p.text_document.uri) else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let start = self.offset(&a.db, t.file, p.range.start);
        let end = self.offset(&a.db, t.file, p.range.end).max(start);
        let mut out = Vec::new();
        for action in a.code_actions(t.file, start, end) {
            let kind = match action.kind {
                analysis::CodeActionKind::QuickFix => CodeActionKind::QUICKFIX,
                analysis::CodeActionKind::Generate => CodeActionKind::REFACTOR,
                analysis::CodeActionKind::Rewrite => {
                    CodeActionKind::REFACTOR_REWRITE
                }
                analysis::CodeActionKind::Inline => {
                    CodeActionKind::REFACTOR_INLINE
                }
            };
            if let Some(only) = &p.context.only {
                if !only.iter().any(|o| {
                    kind.as_str() == o.as_str()
                        || kind
                            .as_str()
                            .starts_with(&format!("{}.", o.as_str()))
                }) {
                    continue;
                }
            }
            let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
            for (span, text) in &action.edits {
                let Some(loc) = self.location(&a.db, *span) else {
                    continue;
                };
                changes
                    .entry(loc.uri)
                    .or_default()
                    .push(TextEdit::new(loc.range, text.clone()));
            }
            let diagnostics = action.fixes.map(|s| {
                let range = self.range(&a.db, s);
                p.context
                    .diagnostics
                    .iter()
                    .filter(|d| d.range == range)
                    .cloned()
                    .collect::<Vec<_>>()
            });
            out.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: action.title,
                kind: Some(kind),
                diagnostics: diagnostics.filter(|d| !d.is_empty()),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                is_preferred: action.preferred.then_some(true),
                ..Default::default()
            }));
        }
        Ok(Some(out))
    }

    pub(super) fn inlay_hints(
        &mut self,
        p: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let Some(t) = self.target(&p.text_document.uri) else {
            return Ok(None);
        };
        let a = self.analysis(&t);
        let start = self.offset(&a.db, t.file, p.range.start);
        let end = self.offset(&a.db, t.file, p.range.end);
        let hints = a
            .inlay_hints(t.file, start, end)
            .into_iter()
            .filter(|h| h.file == t.file)
            .map(|h| InlayHint {
                position: self.position(&a.db, h.file, h.offset),
                label: InlayHintLabel::String(h.label),
                kind: (h.kind == analysis::InlayHintKind::Parameter)
                    .then_some(InlayHintKind::PARAMETER),
                text_edits: None,
                tooltip: None,
                padding_left: Some(h.kind == analysis::InlayHintKind::Closing),
                padding_right: Some(
                    h.kind == analysis::InlayHintKind::Parameter,
                ),
                data: None,
            })
            .collect();
        Ok(Some(hints))
    }
}

/// UTF-16 length of `label[..byte]`, for signature parameter offsets.
fn utf16_offset(label: &str, byte: u32) -> u32 {
    let mut end = (byte as usize).min(label.len());
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    label[..end].chars().map(char::len_utf16).sum::<usize>() as u32
}

pub fn symbol_kind(kind: analysis::SymbolKind) -> SymbolKind {
    use analysis::SymbolKind as K;
    match kind {
        K::Constant | K::Macro => SymbolKind::CONSTANT,
        K::Variable | K::Parameter => SymbolKind::VARIABLE,
        K::TypeParameter => SymbolKind::TYPE_PARAMETER,
        K::Typedef => SymbolKind::CLASS,
        K::Struct | K::Header | K::HeaderUnion => SymbolKind::STRUCT,
        K::Field => SymbolKind::FIELD,
        K::Enum => SymbolKind::ENUM,
        K::EnumMember | K::Error | K::MatchKind => SymbolKind::ENUM_MEMBER,
        K::Extern => SymbolKind::INTERFACE,
        K::Method => SymbolKind::METHOD,
        K::Function | K::Action => SymbolKind::FUNCTION,
        K::Table | K::Instance => SymbolKind::OBJECT,
        K::Control | K::Parser => SymbolKind::MODULE,
        K::State => SymbolKind::EVENT,
        K::Package => SymbolKind::PACKAGE,
        K::ValueSet => SymbolKind::ARRAY,
        K::Keyword | K::Snippet => SymbolKind::KEY,
        K::File | K::Folder => SymbolKind::FILE,
    }
}

pub fn completion_kind(kind: analysis::SymbolKind) -> CompletionItemKind {
    use analysis::SymbolKind as K;
    match kind {
        K::Constant | K::Macro => CompletionItemKind::CONSTANT,
        K::Variable | K::Parameter | K::Instance | K::ValueSet => {
            CompletionItemKind::VARIABLE
        }
        K::TypeParameter => CompletionItemKind::TYPE_PARAMETER,
        K::Typedef | K::Table => CompletionItemKind::CLASS,
        K::Struct | K::Header | K::HeaderUnion => CompletionItemKind::STRUCT,
        K::Field => CompletionItemKind::FIELD,
        K::Enum => CompletionItemKind::ENUM,
        K::EnumMember | K::Error | K::MatchKind => {
            CompletionItemKind::ENUM_MEMBER
        }
        K::Extern => CompletionItemKind::INTERFACE,
        K::Method => CompletionItemKind::METHOD,
        K::Function | K::Action => CompletionItemKind::FUNCTION,
        K::Control | K::Parser | K::Package => CompletionItemKind::MODULE,
        K::State => CompletionItemKind::EVENT,
        K::Keyword => CompletionItemKind::KEYWORD,
        K::Snippet => CompletionItemKind::SNIPPET,
        K::File => CompletionItemKind::FILE,
        K::Folder => CompletionItemKind::FOLDER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_offsets_are_utf16() {
        assert_eq!(utf16_offset("f(bit<8> x)", 2), 2);
        // 'é' is 2 bytes, 1 UTF-16 unit.
        assert_eq!(utf16_offset("é(x)", 3), 2);
        assert_eq!(utf16_offset("ab", 99), 2);
    }
}
