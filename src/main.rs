//! rpmls: a language server for RPM spec files.
//!
//! Lints the open specs with rpmcrab as they are edited, publishing its
//! findings as diagnostics, and serves the editor features built on their
//! syntax tree.

mod document;
mod lint;
mod outline;

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use anyhow::{Result, anyhow};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
};
use lsp_types::request::{DocumentSymbolRequest, FoldingRangeRequest, Request as _};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolResponse, FoldingRangeProviderCapability, OneOf, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use tree_sitter::Parser;

use document::Document;
use lint::Job;

fn main() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    connection.initialize(serde_json::to_value(capabilities())?)?;

    let (jobs, receiver) = mpsc::channel();
    let sender = connection.sender.clone();
    let linter = thread::spawn(move || {
        lint::lint_loop(receiver, |message| {
            let _ = sender.send(message);
        })
    });
    let mut server = Server::new(jobs)?;
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
        ..Default::default()
    }
}

struct Server {
    /// The open documents, keyed by the URI's text: `Uri` has interior
    /// mutability.
    docs: HashMap<String, Document>,
    parser: Parser,
    jobs: mpsc::Sender<Job>,
}

impl Server {
    fn new(jobs: mpsc::Sender<Job>) -> Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_rpmspec::LANGUAGE.into())?;
        Ok(Self { docs: HashMap::new(), parser, jobs })
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
