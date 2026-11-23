//! rpmls: a language server for RPM spec files.
//!
//! Lints the open specs with rpmcrab as they are edited, publishing its
//! findings as diagnostics, and serves the editor features built on their
//! syntax tree.

mod document;
mod lint;
mod macros;
mod outline;
mod sysmacros;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use anyhow::{Result, anyhow};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
};
use lsp_types::request::{
    DocumentHighlightRequest, DocumentSymbolRequest, FoldingRangeRequest, GotoDefinition,
    PrepareRenameRequest, References, Rename, Request as _,
};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolResponse, FoldingRangeProviderCapability, GotoDefinitionResponse, OneOf,
    RenameOptions, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use tree_sitter::Parser;

use document::Document;
use lint::Job;
use sysmacros::SystemMacros;

const USAGE: &str = "usage: rpmls [--macro-cache DIR]

  --macro-cache DIR  copy the rpm macro files go-to-definition points at
                     to DIR, at their full path: when rpmls runs in a
                     container, DIR is a directory shared with the host at
                     the same path, so the editor can open them";

fn main() -> Result<()> {
    let mut macro_cache = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--macro-cache" => {
                macro_cache = Some(PathBuf::from(args.next().ok_or_else(|| anyhow!(USAGE))?));
            }
            // Passed by some clients for the stdio transport, the only one.
            "--stdio" => {}
            _ => return Err(anyhow!(USAGE)),
        }
    }
    let (connection, io_threads) = Connection::stdio();
    connection.initialize(serde_json::to_value(capabilities())?)?;

    let (jobs, receiver) = mpsc::channel();
    let sender = connection.sender.clone();
    let linter = thread::spawn(move || {
        lint::lint_loop(receiver, |message| {
            let _ = sender.send(message);
        })
    });
    let mut server = Server::new(jobs, macro_cache)?;
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
}

impl Server {
    fn new(jobs: mpsc::Sender<Job>, macro_cache: Option<PathBuf>) -> Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_rpmspec::LANGUAGE.into())?;
        Ok(Self { docs: HashMap::new(), parser, jobs, macro_cache, system_macros: None })
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
