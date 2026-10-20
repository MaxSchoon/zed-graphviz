//! The language server: document state, the event loop and request handlers.
//!
//! All state lives on the main thread. Renders run on a small worker pool
//! and come back as events, so a slow graph never blocks hover or typing.
//! Every document carries a generation counter, bumped on each change; a
//! render result older than what is already on screen is dropped.

use crate::cache;
use crate::diagnostics;
use crate::document;
use crate::render::{self, Renderer};
use crate::settings::{Appearance, RenderMode, Settings, SECTION};
use crate::{appearance, log};
use base64::Engine as _;
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::{
    notification as notif, request as req, CodeAction, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, Command, ConfigurationItem, ConfigurationParams, Diagnostic,
    DiagnosticSeverity, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    ExecuteCommandOptions, ExecuteCommandParams, Hover, HoverContents, HoverParams,
    HoverProviderCapability, InitializeParams, MarkupContent, MarkupKind, MessageType, Position,
    PublishDiagnosticsParams, Range, SaveOptions, ServerCapabilities, ShowDocumentParams,
    ShowMessageParams, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri,
};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CMD_OPEN: &str = "graphviz.openPreview";
pub const CMD_EXPORT_SVG: &str = "graphviz.exportSvg";
pub const CMD_EXPORT_PNG: &str = "graphviz.exportPng";

const WORKERS: usize = 2;
/// Stack for render threads. The bundled Graphviz runs on them and may use
/// up to 16 MiB of it; the rest is headroom for wasmtime and host calls.
/// It is reserved, not committed, so unused stack costs no memory.
const RENDER_STACK: usize = 32 << 20;
const HOVER_MAX_WIDTH: f32 = 480.0;
const HOVER_MAX_BYTES: usize = 1_500_000;
const HOVER_CACHE_ENTRIES: usize = 16;
const HOVER_HINT: &str =
    "Run **Open Graphviz preview** from the code actions menu (`cmd-.` / `ctrl-.`) for a live preview.";

pub fn run() -> Result<(), String> {
    let (connection, io_threads) = Connection::stdio();
    let (id, params) = connection
        .initialize_start()
        .map_err(|e| format!("initialize failed: {e}"))?;
    let params: InitializeParams =
        serde_json::from_value(params).map_err(|e| format!("invalid initialize params: {e}"))?;
    let result = json!({
        "capabilities": capabilities(),
        "serverInfo": {"name": "graphviz-preview", "version": env!("CARGO_PKG_VERSION")},
    });
    connection
        .initialize_finish(id, result)
        .map_err(|e| format!("initialize failed: {e}"))?;

    let mut server = Server::new(connection.sender.clone(), &params);
    server.main_loop(&connection.receiver);
    drop(server);
    drop(connection);
    io_threads
        .join()
        .map_err(|e| format!("stdio threads failed: {e}"))
}

fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..Default::default()
            },
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![
                CMD_OPEN.to_string(),
                CMD_EXPORT_SVG.to_string(),
                CMD_EXPORT_PNG.to_string(),
            ],
            work_done_progress_options: Default::default(),
        }),
        ..Default::default()
    }
}

struct Doc {
    uri: Uri,
    text: Arc<String>,
    version: i32,
    /// Bumped on every change; shared with workers so queued live renders
    /// of outdated text can be skipped.
    gen: Arc<AtomicU64>,
    /// Generation of the preview image on disk.
    applied_gen: u64,
    /// Generation of the last published diagnostics.
    diag_gen: u64,
    /// Shared with workers: a diagnostics-only check is redundant once the
    /// preview renders the document anyway.
    preview_open: Arc<AtomicBool>,
    source_path: Option<PathBuf>,
    preview_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExportFormat {
    Svg,
    Png,
}

impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            ExportFormat::Svg => "svg",
            ExportFormat::Png => "png",
        }
    }
}

#[derive(Debug)]
enum JobKind {
    /// A debounced render while typing.
    Live,
    /// A render after a save.
    Save,
    /// Diagnostics only, for a document without an open preview.
    Check,
    Open(RequestId),
    Hover(RequestId, u64),
    Export(RequestId, ExportFormat, PathBuf),
}

impl JobKind {
    fn request_id(&self) -> Option<RequestId> {
        match self {
            JobKind::Open(id) | JobKind::Hover(id, _) | JobKind::Export(id, ..) => Some(id.clone()),
            JobKind::Live | JobKind::Save | JobKind::Check => None,
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

struct Job {
    key: String,
    gen: u64,
    latest: Arc<AtomicU64>,
    preview_open: Arc<AtomicBool>,
    version: i32,
    text: Arc<String>,
    kind: JobKind,
    renderer: Renderer,
    args: Vec<String>,
    /// Render with the dark defaults (never for exports).
    dark: bool,
    timeout: Duration,
    /// `(scale, max width)` when a PNG is wanted.
    png: Option<(f32, Option<f32>)>,
    resources_dir: Option<PathBuf>,
    preview_path: Option<PathBuf>,
}

struct Outcome {
    svg: Option<Vec<u8>>,
    png: Option<Result<Vec<u8>, String>>,
    stderr: String,
    timed_out: bool,
    failure: Option<String>,
    svg_ms: u128,
    png_ms: u128,
}

enum Event {
    Done(Job, Outcome),
    Skipped(Job),
    /// Rendering panicked; the request, if any, still needs an answer.
    Crashed {
        key: String,
        request: Option<RequestId>,
        message: String,
    },
    /// The OS switched between light and dark.
    Appearance(bool),
}

enum Outgoing {
    Configuration,
    ShowDocument(PathBuf),
}

struct Server {
    sender: Sender<Message>,
    docs: HashMap<String, Doc>,
    settings: Settings,
    renderer: Result<Renderer, String>,
    debounce: HashMap<String, Instant>,
    outgoing: HashMap<RequestId, Outgoing>,
    next_request: i32,
    jobs: Sender<Job>,
    events: Receiver<Event>,
    event_tx: Sender<Event>,
    watching_appearance: bool,
    hover_cache: Vec<(u64, Arc<String>)>,
    cache_root: Option<PathBuf>,
    client_pulls_configuration: bool,
    client_shows_documents: bool,
    shutting_down: bool,
}

impl Server {
    fn new(sender: Sender<Message>, params: &InitializeParams) -> Self {
        let (jobs, job_rx) = unbounded::<Job>();
        let (event_tx, events) = unbounded::<Event>();
        for i in 0..WORKERS {
            let job_rx = job_rx.clone();
            let event_tx = event_tx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("render-{i}"))
                .stack_size(RENDER_STACK)
                .spawn(move || {
                    for job in job_rx {
                        let key = job.key.clone();
                        let request = job.kind.request_id();
                        // A panic in a dependency must not take the worker
                        // (and the pending request) down with it.
                        let event =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(job)))
                                .unwrap_or_else(|panic| Event::Crashed {
                                    key,
                                    request,
                                    message: panic_message(&*panic),
                                });
                        if event_tx.send(event).is_err() {
                            break;
                        }
                    }
                });
            if let Err(e) = spawned {
                log(format!("could not start render worker {i}: {e}"));
            }
        }

        let caps = &params.capabilities;
        let client_pulls_configuration = caps
            .workspace
            .as_ref()
            .and_then(|w| w.configuration)
            .unwrap_or(false);
        let client_shows_documents = caps
            .window
            .as_ref()
            .and_then(|w| w.show_document.as_ref())
            .is_some_and(|s| s.support);

        let cache_root = cache::cache_root();
        match &cache_root {
            Some(root) => log(format!("previews are written under {}", root.display())),
            None => log("no cache directory available; previews are disabled"),
        }

        let mut server = Self {
            sender,
            docs: HashMap::new(),
            settings: Settings::default(),
            renderer: Err("not resolved yet".into()),
            debounce: HashMap::new(),
            outgoing: HashMap::new(),
            next_request: 0,
            jobs,
            events,
            event_tx,
            watching_appearance: false,
            hover_cache: Vec::new(),
            cache_root,
            client_pulls_configuration,
            client_shows_documents,
            shutting_down: false,
        };
        let (settings, warnings) = Settings::from_json(
            params
                .initialization_options
                .as_ref()
                .unwrap_or(&Value::Null),
        );
        for w in warnings {
            log(format!("settings: {w}"));
        }
        server.settings = settings;
        server.resolve_renderer();
        if !client_shows_documents {
            log("the client does not support window/showDocument; previews are written to disk only");
        }
        if client_pulls_configuration {
            server.request_configuration();
        }
        server
    }

    fn main_loop(&mut self, receiver: &Receiver<Message>) {
        loop {
            let timeout = self
                .debounce
                .values()
                .min()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(3600));
            select! {
                recv(receiver) -> msg => match msg {
                    Ok(Message::Request(request)) => self.on_request(request),
                    Ok(Message::Notification(n)) => {
                        if n.method == notif::Exit::METHOD {
                            log("exit");
                            return;
                        }
                        self.on_notification(n);
                    }
                    Ok(Message::Response(response)) => self.on_response(response),
                    Err(_) => {
                        log("client connection closed");
                        return;
                    }
                },
                recv(self.events) -> event => {
                    if let Ok(event) = event {
                        self.on_event(event);
                    }
                },
                default(timeout) => {}
            }
            self.fire_debounced();
        }
    }

    // ---- messages to the client -------------------------------------------------

    fn send(&self, message: Message) {
        if self.sender.send(message).is_err() {
            log("could not send a message: the client connection is closed");
        }
    }

    fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) {
        self.send(Message::Notification(Notification::new(
            N::METHOD.to_string(),
            params,
        )));
    }

    fn respond(&self, id: RequestId, result: Value) {
        self.send(Message::Response(Response::new_ok(id, result)));
    }

    fn respond_err(&self, id: RequestId, code: ErrorCode, message: String) {
        log(format!("request failed: {message}"));
        self.send(Message::Response(Response::new_err(
            id,
            code as i32,
            message,
        )));
    }

    fn request<R: lsp_types::request::Request>(&mut self, params: R::Params, kind: Outgoing) {
        self.next_request += 1;
        let id = RequestId::from(self.next_request);
        self.outgoing.insert(id.clone(), kind);
        self.send(Message::Request(Request::new(
            id,
            R::METHOD.to_string(),
            params,
        )));
    }

    fn show_message(&self, typ: MessageType, message: String) {
        self.notify::<notif::ShowMessage>(ShowMessageParams { typ, message });
    }

    fn request_configuration(&mut self) {
        self.request::<req::WorkspaceConfiguration>(
            ConfigurationParams {
                items: vec![ConfigurationItem {
                    scope_uri: None,
                    section: Some(SECTION.to_string()),
                }],
            },
            Outgoing::Configuration,
        );
    }

    // ---- settings ---------------------------------------------------------------

    fn resolve_renderer(&mut self) {
        self.renderer = render::resolve(&self.settings).map(|(renderer, why)| {
            match &renderer {
                Renderer::Dot(path) => log(format!(
                    "renderer: engine=dot path={} ({why})",
                    path.display()
                )),
                #[cfg(feature = "bundled")]
                Renderer::Bundled => {
                    log(format!(
                        "renderer: engine=bundled wasm-sha256={} ({why})",
                        crate::bundled::wasm_sha256()
                    ));
                    std::thread::spawn(crate::bundled::warm_up);
                }
            }
            renderer
        });
        if let Err(e) = &self.renderer {
            log(format!("renderer: none available: {e}"));
        }
    }

    fn apply_settings(&mut self, value: &Value) {
        let (settings, warnings) = Settings::from_json(value);
        for w in warnings {
            log(format!("settings: {w}"));
        }
        if settings == self.settings {
            return;
        }
        let old = std::mem::replace(&mut self.settings, settings);
        log(format!("settings: {:?}", self.settings));
        if old.renderer != self.settings.renderer || old.dot_path != self.settings.dot_path {
            self.resolve_renderer();
        }
        let looks_changed = old.renderer != self.settings.renderer
            || old.dot_path != self.settings.dot_path
            || old.engine != self.settings.engine
            || old.scale != self.settings.scale
            || old.appearance != self.settings.appearance;
        if self.settings.render == RenderMode::Save {
            self.debounce.clear();
        }
        if looks_changed {
            self.rerender_previews();
        }
        self.watch_appearance();
    }

    fn rerender_previews(&mut self) {
        self.hover_cache.clear();
        let open: Vec<String> = self
            .docs
            .iter()
            .filter(|(_, d)| d.preview_open.load(Ordering::Relaxed))
            .map(|(k, _)| k.clone())
            .collect();
        for key in open {
            self.queue(&key, JobKind::Live);
        }
    }

    /// Follows OS light/dark switches while a preview is open in "auto".
    fn watch_appearance(&mut self) {
        let wanted = self.settings.appearance == Appearance::Auto
            && self
                .docs
                .values()
                .any(|d| d.preview_open.load(Ordering::Relaxed));
        if !wanted || self.watching_appearance {
            return;
        }
        self.watching_appearance = true;
        let tx = self.event_tx.clone();
        appearance::watch(move |dark| {
            let _ = tx.send(Event::Appearance(dark));
        });
    }

    // ---- dispatch ---------------------------------------------------------------

    fn on_request(&mut self, request: Request) {
        if self.shutting_down {
            self.respond_err(
                request.id,
                ErrorCode::InvalidRequest,
                "server is shutting down".into(),
            );
            return;
        }
        let Request { id, method, params } = request;
        match method.as_str() {
            req::Shutdown::METHOD => {
                self.shutting_down = true;
                log("shutdown");
                self.respond(id, Value::Null);
            }
            req::HoverRequest::METHOD => match serde_json::from_value::<HoverParams>(params) {
                Ok(p) => self.on_hover(id, p),
                Err(e) => self.respond_err(id, ErrorCode::InvalidParams, format!("hover: {e}")),
            },
            req::CodeActionRequest::METHOD => {
                match serde_json::from_value::<CodeActionParams>(params) {
                    Ok(p) => self.on_code_action(id, p),
                    Err(e) => {
                        self.respond_err(id, ErrorCode::InvalidParams, format!("codeAction: {e}"))
                    }
                }
            }
            req::ExecuteCommand::METHOD => {
                match serde_json::from_value::<ExecuteCommandParams>(params) {
                    Ok(p) => self.on_execute_command(id, p),
                    Err(e) => self.respond_err(
                        id,
                        ErrorCode::InvalidParams,
                        format!("executeCommand: {e}"),
                    ),
                }
            }
            other => self.respond_err(
                id,
                ErrorCode::MethodNotFound,
                format!("unsupported request {other}"),
            ),
        }
    }

    fn on_notification(&mut self, n: Notification) {
        let Notification { method, params } = n;
        match method.as_str() {
            notif::DidOpenTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(params) {
                    self.on_open(p);
                }
            }
            notif::DidChangeTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(params) {
                    self.on_change(p);
                }
            }
            notif::DidSaveTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidSaveTextDocumentParams>(params) {
                    self.on_save(p);
                }
            }
            notif::DidCloseTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(params) {
                    self.on_close(p);
                }
            }
            notif::DidChangeConfiguration::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeConfigurationParams>(params) {
                    // Zed pushes `{"graphviz-preview": {...}}`; other clients may
                    // push nothing and expect a pull.
                    if p.settings.get(SECTION).is_some() {
                        self.apply_settings(&p.settings);
                    }
                    if self.client_pulls_configuration {
                        self.request_configuration();
                    }
                }
            }
            _ => {}
        }
    }

    fn on_response(&mut self, response: Response) {
        let Some(kind) = self.outgoing.remove(&response.id) else {
            return;
        };
        let result = match response.response_result {
            Ok(result) => result,
            Err(error) => {
                match kind {
                    Outgoing::Configuration => {
                        log(format!("workspace/configuration failed: {}", error.message))
                    }
                    Outgoing::ShowDocument(path) => log(format!(
                        "window/showDocument for {} failed: {}",
                        path.display(),
                        error.message
                    )),
                }
                return;
            }
        };
        match kind {
            Outgoing::Configuration => {
                let first = result
                    .as_array()
                    .and_then(|items| items.first())
                    .cloned()
                    .unwrap_or(Value::Null);
                if !first.is_null() {
                    self.apply_settings(&first);
                }
            }
            Outgoing::ShowDocument(path) => {
                if result.get("success").and_then(Value::as_bool) != Some(true) {
                    log(format!("the client did not open {}", path.display()));
                }
            }
        }
    }

    // ---- documents --------------------------------------------------------------

    fn on_open(&mut self, p: DidOpenTextDocumentParams) {
        let uri = p.text_document.uri;
        let key = uri.as_str().to_string();
        let source_path = file_path(&uri);
        let preview_path = self.preview_path_for(&key, source_path.as_deref());
        self.docs.insert(
            key.clone(),
            Doc {
                uri,
                text: Arc::new(p.text_document.text),
                version: p.text_document.version,
                gen: Arc::new(AtomicU64::new(1)),
                applied_gen: 0,
                diag_gen: 0,
                preview_open: Arc::new(AtomicBool::new(false)),
                source_path,
                preview_path,
            },
        );
        self.queue(&key, JobKind::Check);
    }

    fn on_change(&mut self, p: DidChangeTextDocumentParams) {
        let key = p.text_document.uri.as_str().to_string();
        let Some(doc) = self.docs.get_mut(&key) else {
            return;
        };
        // Full sync: the last change carries the whole text.
        let Some(change) = p.content_changes.into_iter().last() else {
            return;
        };
        doc.text = Arc::new(change.text);
        doc.version = p.text_document.version;
        doc.gen.fetch_add(1, Ordering::SeqCst);
        if doc.preview_open.load(Ordering::Relaxed) && self.settings.render == RenderMode::Live {
            let deadline = Instant::now() + Duration::from_millis(self.settings.debounce_ms);
            self.debounce.insert(key, deadline);
        }
    }

    fn on_save(&mut self, p: DidSaveTextDocumentParams) {
        let key = p.text_document.uri.as_str().to_string();
        let Some(doc) = self.docs.get(&key) else {
            return;
        };
        let kind = if doc.preview_open.load(Ordering::Relaxed) {
            JobKind::Save
        } else {
            JobKind::Check
        };
        self.debounce.remove(&key);
        self.queue(&key, kind);
    }

    fn on_close(&mut self, p: DidCloseTextDocumentParams) {
        let key = p.text_document.uri.as_str().to_string();
        self.debounce.remove(&key);
        if let Some(doc) = self.docs.remove(&key) {
            self.notify::<notif::PublishDiagnostics>(PublishDiagnosticsParams {
                uri: doc.uri,
                diagnostics: Vec::new(),
                version: None,
            });
        }
    }

    fn fire_debounced(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .debounce
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            self.debounce.remove(&key);
            self.queue(&key, JobKind::Live);
        }
    }

    fn preview_path_for(&self, key: &str, source: Option<&Path>) -> Option<PathBuf> {
        let root = self.cache_root.as_ref()?;
        let id = source.map_or_else(|| key.to_string(), |p| p.to_string_lossy().into_owned());
        let stem = source.map_or_else(|| "graph".to_string(), cache::stem);
        Some(cache::preview_path(root, &id, &stem))
    }

    // ---- requests ---------------------------------------------------------------

    fn on_code_action(&mut self, id: RequestId, p: CodeActionParams) {
        // Explicit kind filters come from format-on-save style requests; these
        // actions are only meant for the code actions menu.
        if p.context.only.as_ref().is_some_and(|only| !only.is_empty()) {
            self.respond(id, json!([]));
            return;
        }
        let uri = json!(p.text_document.uri.as_str());
        let action = |title: &str, command: &str| {
            CodeActionOrCommand::CodeAction(CodeAction {
                title: title.to_string(),
                command: Some(Command {
                    title: title.to_string(),
                    command: command.to_string(),
                    arguments: Some(vec![uri.clone()]),
                }),
                ..Default::default()
            })
        };
        let actions = vec![
            action("Open Graphviz preview", CMD_OPEN),
            action("Export graph as SVG", CMD_EXPORT_SVG),
            action("Export graph as PNG", CMD_EXPORT_PNG),
        ];
        self.respond(id, serde_json::to_value(actions).unwrap_or(Value::Null));
    }

    fn on_execute_command(&mut self, id: RequestId, p: ExecuteCommandParams) {
        let target = p
            .arguments
            .first()
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                (self.docs.len() == 1)
                    .then(|| self.docs.keys().next().cloned())
                    .flatten()
            });
        let Some(key) = target else {
            self.respond_err(
                id,
                ErrorCode::InvalidParams,
                format!("{}: missing document URI", p.command),
            );
            return;
        };
        if !self.docs.contains_key(&key) && !self.adopt_from_disk(&key) {
            self.respond_err(
                id,
                ErrorCode::InvalidParams,
                format!("{}: unknown document {key}", p.command),
            );
            return;
        }
        match p.command.as_str() {
            CMD_OPEN => {
                if let Some(doc) = self.docs.get_mut(&key) {
                    doc.preview_open.store(true, Ordering::Relaxed);
                }
                self.debounce.remove(&key);
                self.queue(&key, JobKind::Open(id));
                self.watch_appearance();
            }
            CMD_EXPORT_SVG | CMD_EXPORT_PNG => {
                let format = if p.command == CMD_EXPORT_SVG {
                    ExportFormat::Svg
                } else {
                    ExportFormat::Png
                };
                let Some(source) = self.docs.get(&key).and_then(|d| d.source_path.clone()) else {
                    self.show_message(
                        MessageType::ERROR,
                        "Save the graph to a file before exporting it.".into(),
                    );
                    self.respond(id, Value::Null);
                    return;
                };
                let dest = source.with_file_name(format!(
                    "{}.{}",
                    cache::stem(&source),
                    format.extension()
                ));
                self.queue(&key, JobKind::Export(id, format, dest));
            }
            other => self.respond_err(
                id,
                ErrorCode::InvalidParams,
                format!("unknown command {other}"),
            ),
        }
    }

    /// Tracks a file the client asked about without opening it first.
    fn adopt_from_disk(&mut self, key: &str) -> bool {
        let Ok(uri) = Uri::from_str(key) else {
            return false;
        };
        let Some(path) = file_path(&uri) else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return false;
        };
        let preview_path = self.preview_path_for(key, Some(&path));
        self.docs.insert(
            key.to_string(),
            Doc {
                uri,
                text: Arc::new(text),
                version: 0,
                gen: Arc::new(AtomicU64::new(1)),
                applied_gen: 0,
                diag_gen: 0,
                preview_open: Arc::new(AtomicBool::new(false)),
                source_path: Some(path),
                preview_path,
            },
        );
        true
    }

    fn on_hover(&mut self, id: RequestId, p: HoverParams) {
        let key = p
            .text_document_position_params
            .text_document
            .uri
            .as_str()
            .to_string();
        let line = p.text_document_position_params.position.line;
        let Some(doc) = self.docs.get(&key) else {
            self.respond(id, Value::Null);
            return;
        };
        if document::header_line(&doc.text) != Some(line) {
            self.respond(id, Value::Null);
            return;
        }
        let Ok(renderer) = self.renderer.clone() else {
            self.respond(id, Value::Null);
            return;
        };
        let dark = appearance::is_dark(self.settings.appearance);
        let mut hasher = DefaultHasher::new();
        doc.text.hash(&mut hasher);
        render::graphviz_args(self.settings.engine.as_deref()).hash(&mut hasher);
        dark.hash(&mut hasher);
        renderer.hash(&mut hasher);
        self.settings.scale.to_bits().hash(&mut hasher);
        let hash = hasher.finish();
        if let Some((_, markdown)) = self.hover_cache.iter().find(|(h, _)| *h == hash) {
            let markdown = markdown.clone();
            self.respond(id, hover_value(&markdown, line, &doc.text));
            return;
        }
        self.queue(&key, JobKind::Hover(id, hash));
    }

    // ---- rendering --------------------------------------------------------------

    fn queue(&mut self, key: &str, kind: JobKind) {
        let Some(doc) = self.docs.get(key) else {
            return;
        };
        let renderer = match &self.renderer {
            Ok(renderer) => renderer.clone(),
            Err(e) => {
                let message = format!("Graphviz preview: no renderer available: {e}");
                let diagnostic = diagnostics::header_diagnostic(
                    &doc.text,
                    DiagnosticSeverity::ERROR,
                    message.clone(),
                );
                let (uri, version) = (doc.uri.clone(), doc.version);
                self.publish(uri, version, vec![diagnostic]);
                match kind {
                    JobKind::Open(id) | JobKind::Export(id, ..) => {
                        self.show_message(MessageType::ERROR, message);
                        self.respond(id, Value::Null);
                    }
                    JobKind::Hover(id, _) => self.respond(id, Value::Null),
                    _ => {}
                }
                return;
            }
        };
        let export = matches!(kind, JobKind::Export(..));
        let dark = !export && appearance::is_dark(self.settings.appearance);
        let png = match &kind {
            JobKind::Check | JobKind::Export(_, ExportFormat::Svg, _) => None,
            JobKind::Hover(..) => Some((self.settings.scale, Some(HOVER_MAX_WIDTH))),
            _ => Some((self.settings.scale, None)),
        };
        let job = Job {
            key: key.to_string(),
            gen: doc.gen.load(Ordering::SeqCst),
            latest: doc.gen.clone(),
            preview_open: doc.preview_open.clone(),
            version: doc.version,
            text: doc.text.clone(),
            kind,
            renderer,
            args: render::graphviz_args(self.settings.engine.as_deref()),
            dark,
            timeout: Duration::from_millis(self.settings.timeout_ms),
            png,
            resources_dir: doc
                .source_path
                .as_ref()
                .and_then(|p| p.parent().map(Path::to_path_buf)),
            preview_path: doc.preview_path.clone(),
        };
        if let Err(e) = self.jobs.send(job) {
            if let Some(id) = e.0.kind.request_id() {
                self.respond_err(
                    id,
                    ErrorCode::InternalError,
                    "the render workers have stopped".into(),
                );
            } else {
                log("render queue closed: the render workers have stopped");
            }
        }
    }

    fn publish(&self, uri: Uri, version: i32, diagnostics: Vec<Diagnostic>) {
        self.notify::<notif::PublishDiagnostics>(PublishDiagnosticsParams {
            uri,
            diagnostics,
            version: Some(version),
        });
    }

    fn on_event(&mut self, event: Event) {
        let (job, outcome) = match event {
            Event::Done(job, outcome) => (job, outcome),
            Event::Skipped(job) => {
                log(format!(
                    "render: skipped superseded {} render of {} (gen {})",
                    job_label(&job.kind),
                    job.key,
                    job.gen
                ));
                return;
            }
            Event::Crashed {
                key,
                request,
                message,
            } => {
                log(format!("render: rendering {key} panicked: {message}"));
                if let Some(id) = request {
                    self.respond_err(
                        id,
                        ErrorCode::InternalError,
                        format!("rendering failed: {message}"),
                    );
                }
                return;
            }
            Event::Appearance(dark) => {
                if self.settings.appearance == Appearance::Auto {
                    log(format!(
                        "appearance: the OS switched to {}; re-rendering open previews",
                        if dark { "dark" } else { "light" }
                    ));
                    self.rerender_previews();
                }
                return;
            }
        };
        let label = job_label(&job.kind);
        let svg_bytes = outcome.svg.as_ref().map_or(0, Vec::len);
        log(format!(
            "render: engine={} layout={} kind={label} gen={} svg {} ms ({svg_bytes} bytes), png {} ms{}",
            job.renderer.name(),
            self.settings.engine.as_deref().unwrap_or("file"),
            job.gen,
            outcome.svg_ms,
            outcome.png_ms,
            if outcome.timed_out { ", timed out" } else { "" },
        ));
        if let Some(failure) = &outcome.failure {
            log(format!("render: {} failed: {failure}", job.key));
        }

        let problems = self.problems(&job, &outcome);
        let rendered = problems.is_empty() || outcome.svg.is_some();

        match &job.kind {
            JobKind::Live | JobKind::Save | JobKind::Check | JobKind::Open(_) => {
                self.publish_if_current(&job, problems);
                self.apply_preview(&job, &outcome);
                if let JobKind::Open(id) = &job.kind {
                    self.finish_open(id.clone(), &job, rendered);
                }
            }
            JobKind::Hover(id, hash) => {
                let (id, hash) = (id.clone(), *hash);
                let markdown = hover_markdown(&outcome, &problems);
                let markdown = Arc::new(markdown);
                if outcome.svg.is_some() {
                    self.hover_cache.push((hash, markdown.clone()));
                    if self.hover_cache.len() > HOVER_CACHE_ENTRIES {
                        self.hover_cache.remove(0);
                    }
                }
                let line = document::header_line(&job.text).unwrap_or(0);
                self.respond(id, hover_value(&markdown, line, &job.text));
            }
            JobKind::Export(id, format, dest) => {
                self.finish_export(id.clone(), *format, dest, &outcome, &problems);
            }
        }
    }

    /// Graphviz messages plus problems the server itself ran into.
    fn problems(&self, job: &Job, outcome: &Outcome) -> Vec<Diagnostic> {
        let messages = diagnostics::parse_stderr(&outcome.stderr);
        let mut problems = diagnostics::to_diagnostics(&messages, &job.text);
        let mut extra = |message: String| {
            problems.push(diagnostics::header_diagnostic(
                &job.text,
                DiagnosticSeverity::ERROR,
                message,
            ));
        };
        if outcome.timed_out {
            extra(format!(
                "Graphviz did not finish within {} ms and was stopped; the preview shows the last good render. Raise \"timeout_ms\" for large graphs.",
                job.timeout.as_millis()
            ));
        }
        if let Some(failure) = &outcome.failure {
            extra(format!("Graphviz could not run: {failure}"));
        }
        if let Some(Err(e)) = &outcome.png {
            extra(format!(
                "The graph rendered but could not be rasterized: {e}"
            ));
        }
        if outcome.svg.is_none()
            && messages.is_empty()
            && !outcome.timed_out
            && outcome.failure.is_none()
        {
            extra("Graphviz failed without an error message.".into());
        }
        problems
    }

    fn publish_if_current(&mut self, job: &Job, problems: Vec<Diagnostic>) {
        let Some(doc) = self.docs.get_mut(&job.key) else {
            return;
        };
        if job.gen < doc.diag_gen {
            return;
        }
        doc.diag_gen = job.gen;
        let uri = doc.uri.clone();
        self.publish(uri, job.version, problems);
    }

    fn apply_preview(&mut self, job: &Job, outcome: &Outcome) {
        let Some(Ok(png)) = &outcome.png else {
            return;
        };
        let Some(doc) = self.docs.get_mut(&job.key) else {
            return;
        };
        if job.gen < doc.applied_gen {
            log(format!(
                "render: dropped stale render of {} (gen {} < {})",
                job.key, job.gen, doc.applied_gen
            ));
            return;
        }
        let Some(path) = job.preview_path.as_ref() else {
            return;
        };
        match cache::write_atomic(path, png) {
            Ok(()) => {
                doc.applied_gen = job.gen;
                log(format!(
                    "preview: wrote {} bytes to {}",
                    png.len(),
                    path.display()
                ));
            }
            Err(e) => log(format!("preview: could not write {}: {e}", path.display())),
        }
    }

    fn finish_open(&mut self, id: RequestId, job: &Job, rendered: bool) {
        let Some(path) = job.preview_path.clone() else {
            self.show_message(
                MessageType::ERROR,
                "Graphviz preview: no cache directory is available.".into(),
            );
            self.respond(id, Value::Null);
            return;
        };
        if !path.is_file() {
            let why = if rendered {
                "the image could not be written (see the language server log)"
            } else {
                "the graph has errors (see the diagnostics)"
            };
            self.show_message(MessageType::ERROR, format!("Graphviz preview: {why}."));
            self.respond(id, Value::Null);
            return;
        }
        let uri = url::Url::from_file_path(&path)
            .ok()
            .and_then(|u| Uri::from_str(u.as_str()).ok());
        match uri {
            Some(uri) if self.client_shows_documents => {
                log(format!("preview: showing {}", path.display()));
                self.request::<req::ShowDocument>(
                    ShowDocumentParams {
                        uri,
                        external: Some(false),
                        take_focus: Some(true),
                        selection: None,
                    },
                    Outgoing::ShowDocument(path),
                );
            }
            _ => self.show_message(
                MessageType::INFO,
                format!("Graphviz preview written to {}", path.display()),
            ),
        }
        self.respond(id, Value::Null);
    }

    fn finish_export(
        &mut self,
        id: RequestId,
        format: ExportFormat,
        dest: &Path,
        outcome: &Outcome,
        problems: &[Diagnostic],
    ) {
        let bytes = match format {
            ExportFormat::Svg => outcome.svg.as_deref(),
            ExportFormat::Png => match &outcome.png {
                Some(Ok(png)) => Some(png.as_slice()),
                _ => None,
            },
        };
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dest.display().to_string());
        match bytes {
            Some(bytes) => match cache::write_atomic(dest, bytes) {
                Ok(()) => {
                    log(format!(
                        "export: wrote {} bytes to {}",
                        bytes.len(),
                        dest.display()
                    ));
                    self.show_message(MessageType::INFO, format!("Exported {name}"));
                }
                Err(e) => {
                    log(format!("export: could not write {}: {e}", dest.display()));
                    self.show_message(MessageType::ERROR, format!("Could not write {name}: {e}"));
                }
            },
            None => {
                let first = problems
                    .iter()
                    .find(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                    .map(|d| d.message.lines().next().unwrap_or_default().to_string())
                    .unwrap_or_else(|| "Graphviz produced no output".into());
                log(format!("export: {} not written: {first}", dest.display()));
                self.show_message(
                    MessageType::ERROR,
                    format!("Could not export {name}: {first}"),
                );
            }
        }
        self.respond(id, Value::Null);
    }
}

fn job_label(kind: &JobKind) -> &'static str {
    match kind {
        JobKind::Live => "live",
        JobKind::Save => "save",
        JobKind::Check => "check",
        JobKind::Open(_) => "open",
        JobKind::Hover(..) => "hover",
        JobKind::Export(_, ExportFormat::Svg, _) => "export-svg",
        JobKind::Export(_, ExportFormat::Png, _) => "export-png",
    }
}

fn file_path(uri: &Uri) -> Option<PathBuf> {
    url::Url::parse(uri.as_str()).ok()?.to_file_path().ok()
}

fn hover_markdown(outcome: &Outcome, problems: &[Diagnostic]) -> String {
    match &outcome.png {
        Some(Ok(png)) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(png);
            if encoded.len() > HOVER_MAX_BYTES {
                format!("This graph is too large for a hover thumbnail.\n\n{HOVER_HINT}")
            } else {
                format!("![Graphviz preview](data:image/png;base64,{encoded})\n\n{HOVER_HINT}")
            }
        }
        _ => {
            let first = problems
                .first()
                .map(|d| d.message.lines().next().unwrap_or_default().to_string())
                .unwrap_or_else(|| "Graphviz produced no output".into());
            format!("Graphviz could not render this graph: {first}\n\n{HOVER_HINT}")
        }
    }
}

fn hover_value(markdown: &str, line: u32, text: &str) -> Value {
    let width = text
        .lines()
        .nth(line as usize)
        .map_or(0, |l| l.encode_utf16().count() as u32);
    let hover = Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown.to_string(),
        }),
        range: Some(Range::new(
            Position::new(line, 0),
            Position::new(line, width),
        )),
    };
    serde_json::to_value(hover).unwrap_or(Value::Null)
}

/// Runs one job on a worker thread.
fn work(job: Job) -> Event {
    let superseded = match job.kind {
        // A newer live render is already scheduled.
        JobKind::Live => job.latest.load(Ordering::SeqCst) > job.gen,
        // The preview render reports the same diagnostics.
        JobKind::Check => job.preview_open.load(Ordering::Relaxed),
        _ => false,
    };
    if superseded {
        return Event::Skipped(job);
    }
    let started = Instant::now();
    let dark_source = if job.dark {
        render::with_dark_defaults(&job.text)
    } else {
        None
    };
    let source = dark_source.as_deref().unwrap_or(&job.text);
    let out = render::render_svg(&job.renderer, source, &job.args, job.timeout);
    let svg_ms = started.elapsed().as_millis();
    let started = Instant::now();
    let png = match (&out.svg, job.png) {
        (Some(svg), Some((scale, max_width))) => Some(render::svg_to_png(
            svg,
            scale,
            max_width,
            job.resources_dir.as_deref(),
        )),
        _ => None,
    };
    let png_ms = started.elapsed().as_millis();
    Event::Done(
        job,
        Outcome {
            svg: out.svg,
            png,
            stderr: out.stderr,
            timed_out: out.timed_out,
            failure: out.failure,
            svg_ms,
            png_ms,
        },
    )
}
