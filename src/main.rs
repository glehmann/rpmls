//! rpmls: a language server for RPM spec files.
//!
//! Lints the open specs with rpmcrab, or rpmlint, as they are edited,
//! publishing their findings as diagnostics, and serves the editor features built on their
//! syntax tree.

mod actions;
mod checks;
mod completion;
mod document;
mod hover;
mod links;
mod lint;
mod macros;
mod outline;
mod packages;
mod sysmacros;
mod workspace;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use anyhow::{Result, anyhow};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument,
    Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    CodeActionRequest, Completion, DocumentHighlightRequest, DocumentLinkRequest,
    DocumentSymbolRequest, ExecuteCommand, FoldingRangeRequest, GotoDefinition, HoverRequest,
    PrepareRenameRequest, References, RegisterCapability, Rename, Request as _,
    WorkspaceSymbolRequest,
};
use lsp_types::{
    CodeActionProviderCapability, CompletionOptions, CompletionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentLinkOptions, DocumentSymbolResponse, ExecuteCommandOptions,
    FoldingRangeProviderCapability, GotoDefinitionResponse, Hover, HoverContents,
    HoverProviderCapability, Location, MarkupContent, MarkupKind, NumberOrString, OneOf, Position,
    PublishDiagnosticsParams, RenameOptions, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use tree_sitter::Parser;

use document::Document;
use hover::Manual;
use lint::Job;
use packages::Packages;
use sysmacros::SystemMacros;
use workspace::Workspace;

const USAGE: &str = "usage: rpmls [--macro-cache DIR] [--spec-doc FILE]
             [--linter rpmcrab|rpmlint] [--rpmlint-config PATH]...
             [--rpmlintrc-dir DIR]

  --macro-cache DIR  copy the rpm macro files go-to-definition points at
                     to DIR, at their full path: when rpmls runs in a
                     container, DIR is a directory shared with the host at
                     the same path, so the editor can open them
  --spec-doc FILE    rpm's spec manual (spec.md) for hover, when it is not
                     installed, as in containers installed without
                     documentation
  --linter rpmcrab|rpmlint
                     the linter, rpmcrab by default; rpmlint has rpm parse
                     the spec, which runs its shell and Lua code as it is
                     typed: in a container only, without network nor write
                     access to the project
  --rpmlint-config PATH
                     an rpmlint configuration file or directory of them,
                     for rpmcrab
  --rpmlintrc-dir DIR
                     a directory of the filters of each package, as
                     <package>.rpmlintrc, the package named after the
                     directory of its spec: rpmcrab loads it after the
                     rpmlintrc files beside the spec, and the suppressed
                     findings go there";

/// What a spec holds, for the requests on the whole workspace.
#[derive(Clone)]
struct Spec {
    packages: Packages,
    /// Its sections, macro definitions and packages.
    symbols: Vec<lsp_types::SymbolInformation>,
}

impl Spec {
    fn of(doc: &Document) -> Self {
        let packages = Packages::of(doc);
        let container = links::doc_path(doc)
            .and_then(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let mut symbols = outline::flat_symbols(doc, &container);
        #[allow(deprecated)]
        symbols.extend(packages.built.iter().map(|(name, range)| lsp_types::SymbolInformation {
            name: name.clone(),
            kind: lsp_types::SymbolKind::PACKAGE,
            tags: None,
            deprecated: None,
            location: Location::new(doc.uri.clone(), *range),
            container_name: Some(container.clone()),
        }));
        Self { packages, symbols }
    }
}

/// Publish the diagnostics of a document, of a version, and keep them.
type Publish = Arc<dyn Fn(Uri, Option<i32>, Vec<Diagnostic>) + Send + Sync>;

#[derive(Default)]
struct Options {
    macro_cache: Option<PathBuf>,
    spec_doc: Option<PathBuf>,
    lint: lint::Config,
}

fn main() -> Result<()> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().map(PathBuf::from).ok_or_else(|| anyhow!(USAGE));
        match arg.as_str() {
            "--macro-cache" => options.macro_cache = Some(value()?),
            "--spec-doc" => options.spec_doc = Some(value()?),
            "--linter" => {
                options.lint.linter = match value()?.to_str() {
                    Some("rpmcrab") => lint::Linter::Rpmcrab,
                    Some("rpmlint") => lint::Linter::Rpmlint,
                    _ => return Err(anyhow!(USAGE)),
                }
            }
            "--rpmlint-config" => options.lint.files.push(value()?),
            "--rpmlintrc-dir" => options.lint.rpmlintrc_dir = Some(value()?),
            // Passed by some clients for the stdio transport, the only one.
            "--stdio" => {}
            _ => return Err(anyhow!(USAGE)),
        }
    }
    let (connection, io_threads) = Connection::stdio();
    let params: lsp_types::InitializeParams =
        serde_json::from_value(connection.initialize(serde_json::to_value(capabilities())?)?)?;

    let (jobs, receiver) = mpsc::channel();
    let sender = connection.sender.clone();
    let diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>> = Arc::default();
    let latest = diagnostics.clone();
    let publish: Publish = Arc::new(move |uri, version, diagnostics| {
        if let Ok(mut latest) = latest.lock() {
            latest.insert(uri.as_str().to_string(), diagnostics.clone());
        }
        let params = PublishDiagnosticsParams { uri, diagnostics, version };
        let notification = Notification::new(PublishDiagnostics::METHOD.to_string(), params);
        let _ = sender.send(notification.into());
    });
    let lint_config = Arc::new(std::mem::take(&mut options.lint));
    let (config, lint_publish) = (lint_config.clone(), publish.clone());
    let linter = thread::spawn(move || {
        lint::lint_loop(receiver, &config, |uri, version, diagnostics| {
            lint_publish(uri, version, diagnostics)
        })
    });
    // The specs of the workspace, as on disk, for its diagnostics.
    let workspace = Workspace::scan(&roots(&params), &lint_config);
    jobs.send(Job::Files(workspace.specs()))?;
    let watch = params.capabilities.workspace.as_ref().and_then(|w| w.did_change_watched_files);
    if watch.and_then(|w| w.dynamic_registration) == Some(true) {
        let options =
            lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers: workspace.watchers() };
        let registration = lsp_types::Registration {
            id: "rpmls-watch".into(),
            method: DidChangeWatchedFiles::METHOD.into(),
            register_options: Some(serde_json::to_value(options)?),
        };
        let params = lsp_types::RegistrationParams { registrations: vec![registration] };
        let id = RequestId::from("rpmls-watch".to_string());
        connection
            .sender
            .send(Request::new(id, RegisterCapability::METHOD.into(), params).into())?;
    }
    let mut server = Server::new(jobs, options, lint_config, diagnostics, workspace, publish)?;
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

/// The directories of the workspace: its folders, or its root.
fn roots(params: &lsp_types::InitializeParams) -> Vec<PathBuf> {
    #[allow(deprecated)]
    let uris: Vec<&Uri> = match &params.workspace_folders {
        Some(folders) if !folders.is_empty() => folders.iter().map(|f| &f.uri).collect(),
        _ => params.root_uri.iter().collect(),
    };
    uris.into_iter()
        .filter_map(|uri| url::Url::parse(uri.as_str()).ok()?.to_file_path().ok())
        .collect()
}

fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![actions::SUPPRESS.into()],
            ..Default::default()
        }),
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
    lint_config: Arc<lint::Config>,
    /// The latest diagnostics of each document, kept by the lint thread.
    diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>,
    /// rpmcrab's explanation of each check, once asked for.
    explanations: HashMap<String, Option<String>>,
    workspace: Workspace,
    /// What the specs on disk hold, read once until they change.
    specs: HashMap<PathBuf, Spec>,
    publish: Publish,
}

impl Server {
    fn new(
        jobs: mpsc::Sender<Job>,
        options: Options,
        lint_config: Arc<lint::Config>,
        diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>,
        workspace: Workspace,
        publish: Publish,
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
            lint_config,
            diagnostics,
            explanations: HashMap::new(),
            workspace,
            specs: HashMap::new(),
            publish,
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
        let config = self.lint_config.clone();
        for code in codes {
            let explanation =
                self.explanations.entry(code.clone()).or_insert_with(|| config.explain(&code).ok());
            if let Some(text) = explanation {
                parts.push(format!("**{code}** ({})\n\n{text}", config.linter.name()));
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

    /// What the specs of the workspace hold: the open documents' text, else
    /// the specs on disk.
    fn workspace_specs(&mut self) -> Vec<Spec> {
        let open: HashMap<PathBuf, &Document> =
            self.docs.values().filter_map(|d| Some((links::doc_path(d)?, d))).collect();
        let mut all: Vec<Spec> = open.values().map(|d| Spec::of(d)).collect();
        for spec in self.workspace.specs() {
            if open.contains_key(&spec) {
                continue;
            }
            if !self.specs.contains_key(&spec) {
                let doc = std::fs::read_to_string(&spec).ok().and_then(|text| {
                    Document::new(&mut self.parser, links::file_uri(&spec)?, 0, text).ok()
                });
                let Some(doc) = doc else { continue };
                self.specs.insert(spec.clone(), Spec::of(&doc));
            }
            all.extend(self.specs.get(&spec).cloned());
        }
        all
    }

    /// The locations of the package `name` in the workspace: where it is
    /// built, and with `required`, where it is depended on.
    fn package_locations(&mut self, name: &str, built: bool, required: bool) -> Vec<Location> {
        let mut locations = Vec::new();
        for Spec { packages, .. } in self.workspace_specs() {
            let lists = [(built, &packages.built), (required, &packages.required)];
            for (_, list) in lists.into_iter().filter(|(wanted, _)| *wanted) {
                locations.extend(
                    list.iter()
                        .filter(|(n, _)| n == name)
                        .map(|(_, range)| Location::new(packages.uri.clone(), *range)),
                );
            }
        }
        locations
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
                self.workspace.add(links::doc_path(&doc).unwrap_or_default());
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
            DidChangeWatchedFiles::METHOD => {
                let params: lsp_types::DidChangeWatchedFilesParams =
                    serde_json::from_value(notification.params)?;
                let mut specs = Vec::new();
                for change in params.changes {
                    let Ok(path) = url::Url::parse(change.uri.as_str())?.to_file_path() else {
                        continue;
                    };
                    let deleted = change.typ == lsp_types::FileChangeType::DELETED;
                    self.specs.remove(&path);
                    specs.extend(self.workspace.affected(&path, deleted, &self.lint_config));
                }
                self.jobs.send(Job::Files(specs))?;
                return Ok(());
            }
            _ => return Ok(()),
        };
        let job = Job::Lint { uri: doc.uri.clone(), version: doc.version, text: doc.text.clone() };
        self.jobs.send(job)?;
        // The syntax errors at once, with the linter's latest findings, when they change.
        let syntax = checks::syntax(doc);
        let latest = self.diagnostics.lock().map_err(|_| anyhow!("the lint thread panicked"))?;
        let shown = latest.get(doc.uri.as_str());
        let code = |d: &Diagnostic| d.code == Some(NumberOrString::String(checks::SYNTAX.into()));
        let shown_syntax: Vec<&Diagnostic> =
            shown.into_iter().flatten().filter(|d| code(d)).collect();
        if shown_syntax != syntax.iter().collect::<Vec<_>>() {
            let mut diagnostics: Vec<Diagnostic> =
                shown.into_iter().flatten().filter(|d| !code(d)).cloned().collect();
            diagnostics.extend(syntax);
            let (uri, version) = (doc.uri.clone(), doc.version);
            drop(latest);
            (self.publish)(uri, Some(version), diagnostics);
        }
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
            CodeActionRequest::METHOD => self.respond::<CodeActionRequest>(request, |s, p| {
                let doc = s.doc(&p.text_document.uri)?;
                let rpmlintrc =
                    links::doc_path(doc).and_then(|p| s.lint_config.package_rpmlintrc(&p));
                Ok(Some(actions::actions(doc, &p.context.diagnostics, rpmlintrc.as_deref())))
            }),
            ExecuteCommand::METHOD => self.respond::<ExecuteCommand>(request, |s, p| {
                let [path, filter] = p.arguments.as_slice() else {
                    return Err(anyhow!("{} takes a path and a filter", p.command));
                };
                let (Some(path), Some(filter)) = (path.as_str(), filter.as_str()) else {
                    return Err(anyhow!("{} takes a path and a filter", p.command));
                };
                if p.command != actions::SUPPRESS {
                    return Err(anyhow!("unknown command {}", p.command));
                }
                let path = std::path::Path::new(path);
                actions::add_filter(path, filter)?;
                // Without waiting for the editor to report the change, if it does.
                s.jobs.send(Job::Files(s.workspace.affected(path, false, &s.lint_config)))?;
                Ok(None)
            }),
            WorkspaceSymbolRequest::METHOD => {
                self.respond::<WorkspaceSymbolRequest>(request, |s, p| {
                    let query = p.query.to_lowercase();
                    let symbols = s
                        .workspace_specs()
                        .into_iter()
                        .flat_map(|spec| spec.symbols)
                        .filter(|symbol| symbol.name.to_lowercase().contains(&query))
                        .collect();
                    Ok(Some(lsp_types::WorkspaceSymbolResponse::Flat(symbols)))
                })
            }
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
                let macro_name = macros::name_at(doc, p.position);
                let dependency = Packages::of(doc)
                    .at(p.position)
                    .filter(|(_, required)| *required)
                    .map(|(name, _)| name.to_string());
                // A dependency, to the spec building it.
                if locations.is_empty()
                    && let Some(name) = dependency
                {
                    locations = s.package_locations(&name, true, false);
                }
                // A macro the spec does not define comes from rpm's files.
                if locations.is_empty()
                    && let Some(name) = macro_name
                {
                    locations = s.system_macros()?.definitions(&name)?;
                }
                Ok(Some(GotoDefinitionResponse::Array(locations)))
            }),
            References::METHOD => self.respond::<References>(request, |s, p| {
                let doc = s.doc(&p.text_document_position.text_document.uri)?;
                let position = p.text_document_position.position;
                let declaration = p.context.include_declaration;
                // A package name, but on a macro in it: its dependents in the workspace.
                if macros::name_at(doc, position).is_none()
                    && let Some((name, _)) = Packages::of(doc).at(position)
                {
                    let name = name.to_string();
                    return Ok(Some(s.package_locations(&name, declaration, true)));
                }
                Ok(Some(macros::references(doc, position, declaration)))
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
