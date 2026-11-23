//! rpmls: a language server for RPM spec files.
//!
//! Lints the open specs with rpmcrab as they are edited, publishing its
//! findings as diagnostics, and serves the editor features built on their
//! syntax tree.

mod completion;
mod document;
mod hover;
mod links;
mod lint;
mod macros;
mod outline;
mod sysmacros;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use anyhow::{Result, anyhow};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{
    Completion, DocumentHighlightRequest, DocumentLinkRequest, DocumentSymbolRequest,
    FoldingRangeRequest, GotoDefinition, HoverRequest, PrepareRenameRequest, References, Rename,
    Request as _,
};
use lsp_types::{
    CompletionOptions, CompletionResponse, Diagnostic, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentLinkOptions,
    DocumentSymbolResponse, FoldingRangeProviderCapability, GotoDefinitionResponse, Hover,
    HoverContents, HoverProviderCapability, MarkupContent, MarkupKind, OneOf, Position,
    PublishDiagnosticsParams, RenameOptions, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use tree_sitter::Parser;

use document::Document;
use hover::Manual;
use lint::Job;
use sysmacros::SystemMacros;

const USAGE: &str = "usage: rpmls [--macro-cache DIR] [--spec-doc FILE]

  --macro-cache DIR  copy the rpm macro files go-to-definition points at
                     to DIR, at their full path: when rpmls runs in a
                     container, DIR is a directory shared with the host at
                     the same path, so the editor can open them
  --spec-doc FILE    rpm's spec manual (spec.md) for hover, when it is not
                     installed, as in containers installed without
                     documentation";

#[derive(Default)]
struct Options {
    macro_cache: Option<PathBuf>,
    spec_doc: Option<PathBuf>,
}

fn main() -> Result<()> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().map(PathBuf::from).ok_or_else(|| anyhow!(USAGE));
        match arg.as_str() {
            "--macro-cache" => options.macro_cache = Some(value()?),
            "--spec-doc" => options.spec_doc = Some(value()?),
            // Passed by some clients for the stdio transport, the only one.
            "--stdio" => {}
            _ => return Err(anyhow!(USAGE)),
        }
    }
    let (connection, io_threads) = Connection::stdio();
    connection.initialize(serde_json::to_value(capabilities())?)?;

    let (jobs, receiver) = mpsc::channel();
    let sender = connection.sender.clone();
    let diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>> = Arc::default();
    let latest = diagnostics.clone();
    let linter = thread::spawn(move || {
        lint::lint_loop(receiver, |uri, version, diagnostics| {
            if let Ok(mut latest) = latest.lock() {
                latest.insert(uri.as_str().to_string(), diagnostics.clone());
            }
            let params = PublishDiagnosticsParams { uri, diagnostics, version };
            let notification = Notification::new(PublishDiagnostics::METHOD.to_string(), params);
            let _ = sender.send(notification.into());
        })
    });
    let mut server = Server::new(jobs, options, diagnostics)?;
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    break;
                }
                connection.sender.send(server.request(request).into())?;
            }
            Message::Notification(notification) => {
                if let Err(e) = server.notify(notification) {
                    eprintln!("rpmls: {e:#}");
                }
            }
            Message::Response(_) => {}
        }
    }
    drop(server);
    linter.join().map_err(|_| anyhow!("the lint thread panicked"))?;
    drop(connection);
    io_threads.join()?;
    Ok(())
}

fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec!["%".into(), "{".into()]),
            ..Default::default()
        }),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        }),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        ..Default::default()
    }
}

struct Server {
    /// The open documents, keyed by the URI's text: `Uri` has interior
    /// mutability.
    docs: HashMap<String, Document>,
    parser: Parser,
    jobs: mpsc::Sender<Job>,
    macro_cache: Option<PathBuf>,
    /// Loaded on the first lookup of a macro the spec does not define.
    system_macros: Option<SystemMacros>,
    manual: Option<Manual>,
    /// The latest diagnostics of each document, kept by the lint thread.
    diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>,
    /// rpmcrab's explanation of each check, once asked for.
    explanations: HashMap<String, Option<String>>,
}

impl Server {
    fn new(
        jobs: mpsc::Sender<Job>,
        options: Options,
        diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>,
    ) -> Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_rpmspec::LANGUAGE.into())?;
        Ok(Self {
            docs: HashMap::new(),
            parser,
            jobs,
            macro_cache: options.macro_cache,
            system_macros: None,
            manual: Manual::load(options.spec_doc.as_deref()),
            diagnostics,
            explanations: HashMap::new(),
        })
    }

    /// Hover: the explanation of the diagnostics on the line, then the manual
    /// for the keyword, or the definition of the macro, under the cursor.
    fn hover(&mut self, uri: &Uri, position: Position) -> Result<Option<Hover>> {
        let mut codes: Vec<String> = self
            .diagnostics
            .lock()
            .map_err(|_| anyhow!("the lint thread panicked"))?
            .get(uri.as_str())
            .into_iter()
            .flatten()
            .filter(|d| d.range.start.line <= position.line && position.line <= d.range.end.line)
            .filter_map(|d| match &d.code {
                Some(lsp_types::NumberOrString::String(code)) => Some(code.clone()),
                _ => None,
            })
            .collect();
        codes.sort();
        codes.dedup();
        let mut parts = Vec::new();
        for code in codes {
            let explanation =
                self.explanations.entry(code.clone()).or_insert_with(|| lint::explain(&code).ok());
            if let Some(text) = explanation {
                parts.push(format!("**{code}** (rpmcrab)\n\n{text}"));
            }
        }

        let doc = self.doc(uri)?;
        let mut range = None;
        if let Some((key, keyword)) = hover::keyword_at(doc, doc.offset(position)) {
            if let Some(text) = self.manual.as_ref().and_then(|m| m.get(&key)) {
                parts.push(text.to_string());
                range = Some(keyword);
            }
        } else if let Some(name) = macros::name_at(doc, position) {
            let local = macros::definition_texts(doc, &name);
            let code = |text: &str| format!("```spec\n{text}\n```");
            if local.is_empty() {
                for (path, text) in self.system_macros()?.definition_texts(&name) {
                    parts.push(format!("{}\n\n{}", code(&text), path.display()));
                }
            } else {
                parts.extend(local.iter().map(|text| code(text)));
            }
        }
        Ok((!parts.is_empty()).then(|| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: parts.join("\n\n---\n\n"),
            }),
            range,
        }))
    }

    fn system_macros(&mut self) -> Result<&SystemMacros> {
        if self.system_macros.is_none() {
            self.system_macros = Some(SystemMacros::load(self.macro_cache.clone())?);
        }
        Ok(self.system_macros.as_ref().expect("just loaded"))
    }

    fn doc(&self, uri: &Uri) -> Result<&Document> {
        self.docs.get(uri.as_str()).ok_or_else(|| anyhow!("unknown document {}", uri.as_str()))
    }

    fn notify(&mut self, notification: Notification) -> Result<()> {
        let doc = match notification.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let params: DidOpenTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                let item = params.text_document;
                let doc = Document::new(&mut self.parser, item.uri, item.version, item.text)?;
                self.docs.entry(doc.uri.as_str().to_string()).insert_entry(doc).into_mut()
            }
            DidChangeTextDocument::METHOD => {
                let params: DidChangeTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                let uri = params.text_document.uri;
                let doc = self
                    .docs
                    .get_mut(uri.as_str())
                    .ok_or_else(|| anyhow!("unknown document {}", uri.as_str()))?;
                doc.apply(&mut self.parser, params.text_document.version, params.content_changes)?;
                doc
            }
            DidCloseTextDocument::METHOD => {
                let params: DidCloseTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                let uri = params.text_document.uri;
                self.docs.remove(uri.as_str());
                self.jobs.send(Job::Close(uri))?;
                return Ok(());
            }
            _ => return Ok(()),
        };
        let job = Job::Lint { uri: doc.uri.clone(), version: doc.version, text: doc.text.clone() };
        self.jobs.send(job)?;
        Ok(())
    }

    fn request(&mut self, request: Request) -> Response {
        match request.method.as_str() {
            DocumentSymbolRequest::METHOD => {
                self.respond::<DocumentSymbolRequest>(request, |s, p| {
                    let doc = s.doc(&p.text_document.uri)?;
                    Ok(Some(DocumentSymbolResponse::Nested(outline::symbols(doc))))
                })
            }
            FoldingRangeRequest::METHOD => self.respond::<FoldingRangeRequest>(request, |s, p| {
                Ok(Some(outline::folding_ranges(s.doc(&p.text_document.uri)?)))
            }),
            DocumentLinkRequest::METHOD => self.respond::<DocumentLinkRequest>(request, |s, p| {
                Ok(Some(links::links(s.doc(&p.text_document.uri)?)))
            }),
            Completion::METHOD => self.respond::<Completion>(request, |s, p| {
                let p = p.text_document_position;
                // rpm's macros, loaded before borrowing the document.
                if let Err(e) = s.system_macros() {
                    eprintln!("rpmls: loading rpm's macros: {e:#}");
                }
                let system = s.system_macros.as_ref();
                let doc = s.doc(&p.text_document.uri)?;
                let items = completion::complete(doc, p.position, s.manual.as_ref(), || {
                    system.map(SystemMacros::names).unwrap_or_default()
                });
                Ok(Some(CompletionResponse::Array(items)))
            }),
            HoverRequest::METHOD => self.respond::<HoverRequest>(request, |s, p| {
                let p = p.text_document_position_params;
                s.hover(&p.text_document.uri, p.position)
            }),
            GotoDefinition::METHOD => self.respond::<GotoDefinition>(request, |s, p| {
                let p = p.text_document_position_params;
                let doc = s.doc(&p.text_document.uri)?;
                let mut locations = macros::definitions(doc, p.position);
                // A macro the spec does not define comes from rpm's files.
                if locations.is_empty()
                    && let Some(name) = macros::name_at(doc, p.position)
                {
                    locations = s.system_macros()?.definitions(&name)?;
                }
                Ok(Some(GotoDefinitionResponse::Array(locations)))
            }),
            References::METHOD => self.respond::<References>(request, |s, p| {
                let doc = s.doc(&p.text_document_position.text_document.uri)?;
                let position = p.text_document_position.position;
                Ok(Some(macros::references(doc, position, p.context.include_declaration)))
            }),
            DocumentHighlightRequest::METHOD => {
                self.respond::<DocumentHighlightRequest>(request, |s, p| {
                    let p = p.text_document_position_params;
                    Ok(Some(macros::highlights(s.doc(&p.text_document.uri)?, p.position)))
                })
            }
            PrepareRenameRequest::METHOD => self
                .respond::<PrepareRenameRequest>(request, |s, p| {
                    Ok(macros::prepare_rename(s.doc(&p.text_document.uri)?, p.position))
                }),
            Rename::METHOD => self.respond::<Rename>(request, |s, p| {
                let p_pos = p.text_document_position;
                macros::rename(s.doc(&p_pos.text_document.uri)?, p_pos.position, &p.new_name)
            }),
            method => Response::new_err(
                request.id,
                ErrorCode::MethodNotFound as i32,
                format!("unsupported request: {method}"),
            ),
        }
    }

    fn respond<R: lsp_types::request::Request>(
        &mut self,
        request: Request,
        handle: impl FnOnce(&mut Self, R::Params) -> Result<R::Result>,
    ) -> Response {
        let params = match serde_json::from_value(request.params) {
            Ok(params) => params,
            Err(e) => {
                return Response::new_err(
                    request.id,
                    ErrorCode::InvalidParams as i32,
                    e.to_string(),
                );
            }
        };
        match handle(self, params) {
            Ok(result) => Response::new_ok(request.id, result),
            Err(e) => {
                Response::new_err(request.id, ErrorCode::InternalError as i32, format!("{e:#}"))
            }
        }
    }
}
