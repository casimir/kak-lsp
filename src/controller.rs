use crossbeam_channel::{bounded, Receiver, Sender};
use editor_transport;
use fnv::FnvHashMap;
use jsonrpc_core::{self, Call, Id, Output, Params, Version};
use language_server_transport;
use languageserver_types::*;
use languageserver_types::notification::Notification;
use languageserver_types::request::Request;
use regex::Regex;
use serde_json::{self, Value};
use std::fs::{remove_file, File};
use std::io::Read;
use std::process;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use types::*;
use url::Url;

fn get_server_cmd(config: &Config, language_id: &str) -> Option<(String, Vec<String>)> {
    if let Some(language) = config.language.get(language_id) {
        return Some((language.command.clone(), language.args.clone()));
    }
    None
}

pub fn start(config: &Config) {
    let (editor_tx, editor_rx) = editor_transport::start(config);
    let mut controllers: FnvHashMap<Route, Sender<EditorRequest>> = FnvHashMap::default();
    for request in editor_rx {
        let route = request.route.clone();
        let (_, language_id, root_path) = route.clone();
        let controller = controllers.get(&route).cloned();
        match controller {
            Some(controller_tx) => {
                controller_tx
                    .send(request.request)
                    .expect("Failed to route editor request");
            }
            None => {
                let (lang_srv_cmd, lang_srv_args) = get_server_cmd(config, &language_id).unwrap();
                // NOTE 1024 is arbitrary
                let (controller_tx, controller_rx) = bounded(1024);
                controllers.insert(route, controller_tx);
                let editor_tx = editor_tx.clone();
                thread::spawn(move || {
                    let (lang_srv_tx, lang_srv_rx): (
                        Sender<ServerMessage>,
                        Receiver<ServerMessage>,
                    ) = language_server_transport::start(&lang_srv_cmd, &lang_srv_args);
                    let controller = Controller::start(
                        &language_id,
                        &root_path,
                        lang_srv_tx,
                        lang_srv_rx,
                        editor_tx,
                        controller_rx,
                        request.request,
                    );
                    controller.wait().expect("Failed to wait for controller");
                });
            }
        }
    }
}

struct Controller {
    editor_reader_handle: JoinHandle<()>,
}

struct Context {
    capabilities: Option<ServerCapabilities>,
    editor_tx: Sender<EditorResponse>,
    lang_srv_tx: Sender<ServerMessage>,
    language_id: String,
    pending_requests: Vec<EditorRequest>,
    request_counter: u64,
    response_waitlist: FnvHashMap<Id, (EditorMeta, String, Params)>,
    versions: FnvHashMap<String, u64>,
}

impl Context {
    fn call(&mut self, id: Id, method: String, params: impl ToParams) {
        let call = jsonrpc_core::MethodCall {
            jsonrpc: Some(Version::V2),
            id,
            method,
            params: Some(params.to_params().expect("Failed to convert params")),
        };
        self.lang_srv_tx
            .send(ServerMessage::Request(Call::MethodCall(call)))
            .expect("Failed to send request to language server transport");
    }

    fn notify(&mut self, method: String, params: impl ToParams) {
        let notification = jsonrpc_core::Notification {
            jsonrpc: Some(Version::V2),
            method,
            params: Some(params.to_params().expect("Failed to convert params")),
        };
        self.lang_srv_tx
            .send(ServerMessage::Request(Call::Notification(notification)))
            .expect("Failed to send request to language server transport");
    }
}

impl Controller {
    fn start(
        language_id: &str,
        root_path: &str,
        lang_srv_tx: Sender<ServerMessage>,
        lang_srv_rx: Receiver<ServerMessage>,
        editor_tx: Sender<EditorResponse>,
        editor_rx: Receiver<EditorRequest>,
        initial_request: EditorRequest,
    ) -> Self {
        let initial_request_meta = initial_request.meta.clone();
        let ctx_src = Arc::new(Mutex::new(Context {
            capabilities: None,
            editor_tx,
            lang_srv_tx,
            language_id: language_id.to_string(),
            pending_requests: vec![initial_request],
            request_counter: 0,
            response_waitlist: FnvHashMap::default(),
            versions: FnvHashMap::default(),
        }));

        let ctx = Arc::clone(&ctx_src);
        let editor_reader_handle = thread::spawn(move || {
            for msg in editor_rx {
                let mut ctx = ctx.lock().expect("Failed to lock context");
                if ctx.capabilities.is_some() {
                    dispatch_editor_request(msg, &mut ctx);
                } else {
                    ctx.pending_requests.push(msg);
                }
            }
        });

        let ctx = Arc::clone(&ctx_src);
        thread::spawn(move || {
            for msg in lang_srv_rx {
                match msg {
                    ServerMessage::Request(_) => {
                        //println!("Requests from language server are not supported yet");
                        //println!("{:?}", request);
                    }
                    ServerMessage::Response(output) => {
                        let mut ctx = ctx.lock().expect("Failed to lock context");
                        match output {
                            Output::Success(success) => {
                                if let Some(request) = ctx.response_waitlist.remove(&success.id) {
                                    let (meta, method, params) = request;
                                    dispatch_server_response(
                                        &meta,
                                        &method,
                                        params,
                                        success.result,
                                        &mut ctx,
                                    );
                                } else {
                                    println!("Id {:?} is not in waitlist!", success.id);
                                }
                            }
                            Output::Failure(failure) => {
                                println!("Error response from server: {:?}", failure);
                                ctx.response_waitlist.remove(&failure.id);
                            }
                        }
                    }
                }
            }
        });

        let mut ctx = ctx_src.lock().expect("Failed to lock context");
        let req_id = Id::Num(ctx.request_counter);
        let req = jsonrpc_core::MethodCall {
            jsonrpc: Some(Version::V2),
            id: req_id.clone(),
            method: request::Initialize::METHOD.into(),
            params: Some(initialize(root_path)),
        };
        ctx.response_waitlist.insert(
            req_id,
            (
                initial_request_meta,
                req.method.clone(),
                req.params.clone().unwrap(),
            ),
        );
        ctx.lang_srv_tx
            .send(ServerMessage::Request(Call::MethodCall(req)))
            .expect("Failed to send request to language server transport");

        Controller {
            editor_reader_handle,
        }
    }

    pub fn wait(self) -> thread::Result<()> {
        self.editor_reader_handle.join()
        // TODO lang_srv_reader_handle
    }
}

fn initialize(root_path: &str) -> Params {
    let params = InitializeParams {
        capabilities: ClientCapabilities {
            workspace: None,
            text_document: Some(TextDocumentClientCapabilities {
                synchronization: None,
                completion: Some(CompletionCapability {
                    dynamic_registration: None,
                    completion_item: Some(CompletionItemCapability {
                        snippet_support: None,
                        commit_characters_support: None,
                        documentation_format: None,
                    }),
                }),
                hover: None,
                signature_help: None,
                references: None,
                document_highlight: None,
                document_symbol: None,
                formatting: None,
                range_formatting: None,
                on_type_formatting: None,
                definition: None,
                code_action: None,
                code_lens: None,
                document_link: None,
                rename: None,
            }),
            experimental: None,
        },
        initialization_options: None,
        process_id: Some(process::id().into()),
        root_uri: Some(Url::parse(&format!("file://{}", root_path)).unwrap()),
        root_path: Some(root_path.to_string()),
        trace: Some(TraceOption::Off),
    };

    params.to_params().unwrap()
}

fn dispatch_editor_request(request: EditorRequest, mut ctx: &mut Context) {
    let buffile = &request.meta.buffile;
    if !ctx.versions.contains_key(buffile) {
        text_document_did_open(
            (TextDocumentDidOpenParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: Url::parse(&format!("file://{}", buffile)).unwrap(),
                    version: Some(request.meta.version),
                },
            }).to_params()
                .unwrap(),
            &request.meta,
            &mut ctx,
        );
    }
    match request.call {
        Call::Notification(notification) => {
            dispatch_editor_notification(&request.meta, notification, &mut ctx);
        }
        Call::MethodCall(call) => {
            dispatch_editor_call(&request.meta, &call, &mut ctx);
        }
        Call::Invalid(m) => {
            panic!("Invalid call, shall not pass! {:?}", m);
        }
    }
}

fn dispatch_editor_call(
    _meta: &EditorMeta,
    _call: &jsonrpc_core::MethodCall,
    mut _ctx: &mut Context,
) {
    println!("Method calls (requests which require response with id) from editor are not supported at the moment, how did you run into this branch of code?");
}

fn dispatch_editor_notification(
    meta: &EditorMeta,
    notification: jsonrpc_core::Notification,
    mut ctx: &mut Context,
) {
    let params = notification
        .params
        .expect("All editor notifications must have parameters");
    let method: &str = &notification.method;
    match method {
        notification::DidOpenTextDocument::METHOD => {
            text_document_did_open(params, meta, &mut ctx);
        }
        notification::DidChangeTextDocument::METHOD => {
            text_document_did_change(params, meta, &mut ctx);
        }
        notification::DidCloseTextDocument::METHOD => {
            text_document_did_close(params, meta, &mut ctx);
        }
        notification::DidSaveTextDocument::METHOD => {
            text_document_did_save(params, meta, &mut ctx);
        }
        request::Completion::METHOD => {
            text_document_completion(params, meta, &mut ctx);
        }
        request::HoverRequest::METHOD => {
            text_document_hover(params, meta, &mut ctx);
        }
        request::GotoDefinition::METHOD => {
            text_document_definition(params, meta, &mut ctx);
        }
        _ => {
            println!("Unsupported method: {}", notification.method);
        }
    }
}

fn dispatch_server_response(
    meta: &EditorMeta,
    method: &str,
    params: Params,
    response: Value,
    mut ctx: &mut Context,
) {
    match method {
        request::Completion::METHOD => {
            editor_completion(
                meta,
                &params.parse().expect("Failed to parse params"),
                serde_json::from_value(response).expect("Failed to parse completion response"),
                &mut ctx,
            );
        }
        request::HoverRequest::METHOD => {
            editor_hover(
                meta,
                &params.parse().expect("Failed to parse params"),
                serde_json::from_value(response).expect("Failed to parse hover response"),
                &mut ctx,
            );
        }
        request::GotoDefinition::METHOD => {
            editor_definition(
                meta,
                &params.parse().expect("Failed to parse params"),
                serde_json::from_value(response).expect("Failed to parse definition response"),
                &mut ctx,
            );
        }
        request::Initialize::METHOD => {
            initialized(
                meta,
                &params.parse().unwrap(),
                serde_json::from_value(response).expect("Failed to parse initialized response"),
                &mut ctx,
            );
        }
        _ => {
            println!("Don't know how to handle response for method: {}", method);
        }
    }
}

fn text_document_did_open(params: Params, meta: &EditorMeta, ctx: &mut Context) {
    let params: TextDocumentDidOpenParams = params
        .parse()
        .expect("Params should follow TextDocumentDidOpenParams structure");
    let language_id = ctx.language_id.clone();
    let mut file = File::open(&meta.buffile).expect("Failed to open file");
    let mut text = String::new();
    file.read_to_string(&mut text)
        .expect("Failed to read from file");
    let params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: params.text_document.uri,
            language_id,
            version: params.text_document.version.unwrap(),
            text,
        },
    };
    ctx.versions.insert(meta.buffile.clone(), meta.version);
    ctx.notify(notification::DidOpenTextDocument::METHOD.into(), params);
}

fn text_document_did_change(params: Params, meta: &EditorMeta, ctx: &mut Context) {
    let params: TextDocumentDidChangeParams = params
        .parse()
        .expect("Params should follow TextDocumentDidChangeParams structure");
    let uri = params.text_document.uri;
    let version = params.text_document.version.unwrap_or(0);
    let old_version = ctx.versions.get(&meta.buffile).cloned().unwrap_or(0);
    if old_version >= version {
        return;
    }
    ctx.versions.insert(meta.buffile.clone(), version);
    let file_path = params.text_document.draft;
    let mut text = String::new();
    {
        let mut file = File::open(&file_path).expect("Failed to open file");
        file.read_to_string(&mut text)
            .expect("Failed to read from file");
    }
    remove_file(file_path).expect("Failed to remove temporary file");
    let params = DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier {
            uri: uri.clone(),
            version: params.text_document.version,
        },
        content_changes: vec![
            TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text,
            },
        ],
    };
    ctx.notify(notification::DidChangeTextDocument::METHOD.into(), params);
}

fn text_document_did_close(params: Params, _meta: &EditorMeta, ctx: &mut Context) {
    let params: TextDocumentDidCloseParams = params
        .parse()
        .expect("Params should follow TextDocumentDidCloseParams structure");
    let uri = params.text_document.uri;
    let params = DidCloseTextDocumentParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
    };
    ctx.notify(notification::DidCloseTextDocument::METHOD.into(), params);
}

fn text_document_did_save(params: Params, _meta: &EditorMeta, ctx: &mut Context) {
    let params: TextDocumentDidSaveParams = params
        .parse()
        .expect("Params should follow TextDocumentDidSaveParams structure");
    let uri = params.text_document.uri;
    let params = DidSaveTextDocumentParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
    };
    ctx.notify(notification::DidSaveTextDocument::METHOD.into(), params);
}

fn text_document_completion(params: Params, meta: &EditorMeta, ctx: &mut Context) {
    let req_params: TextDocumentCompletionParams = params
        .clone()
        .parse()
        .expect("Params should follow TextDocumentCompletionParams structure");
    let position = req_params.position;
    let req_params = CompletionParams {
        text_document: TextDocumentIdentifier {
            uri: req_params.text_document.uri.clone(),
        },
        position,
        context: None,
    };
    let id = Id::Num(ctx.request_counter);
    ctx.request_counter += 1;
    ctx.response_waitlist.insert(
        id.clone(),
        (meta.clone(), request::Completion::METHOD.into(), params),
    );
    ctx.call(id, request::Completion::METHOD.into(), req_params);
}

fn text_document_hover(params: Params, meta: &EditorMeta, ctx: &mut Context) {
    let req_params: TextDocumentPositionParams = params
        .clone()
        .parse()
        .expect("Params should follow TextDocumentPositionParams structure");
    // TODO DRY
    let id = Id::Num(ctx.request_counter);
    ctx.request_counter += 1;
    ctx.response_waitlist.insert(
        id.clone(),
        (meta.clone(), request::HoverRequest::METHOD.into(), params),
    );
    ctx.call(id, request::HoverRequest::METHOD.into(), req_params);
}

fn text_document_definition(params: Params, meta: &EditorMeta, ctx: &mut Context) {
    let req_params: TextDocumentPositionParams = params
        .clone()
        .parse()
        .expect("Params should follow TextDocumentPositionParams structure");
    // TODO DRY
    let id = Id::Num(ctx.request_counter);
    ctx.request_counter += 1;
    ctx.response_waitlist.insert(
        id.clone(),
        (meta.clone(), request::GotoDefinition::METHOD.into(), params),
    );
    ctx.call(id, request::GotoDefinition::METHOD.into(), req_params);
}

fn editor_completion(
    meta: &EditorMeta,
    params: &TextDocumentCompletionParams,
    result: CompletionResponse,
    ctx: &mut Context,
) {
    let items = match result {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    let re = Regex::new(r"(?P<c>[:|$])").unwrap();
    let items = items
        .into_iter()
        .map(|x| {
            format!(
                "{}|{}|{}",
                re.replace_all(&x.label, r"\$c"),
                re.replace_all(&x.detail.unwrap_or_else(|| "".to_string()), r"\$c"),
                re.replace_all(&x.label, r"\$c"),
            )
        })
        .collect::<Vec<String>>()
        .join(":");
    let p = params.position;
    let command = format!(
        "set %{{buffer={}}} lsp_completions %§{}.{}@{}:{}§\n",
        meta.buffile,
        p.line + 1,
        p.character + 1 - params.completion.offset,
        params.text_document.version.unwrap(),
        items
    );
    ctx.editor_tx
        .send(EditorResponse {
            meta: meta.clone(),
            command,
        })
        .expect("Failed to send message to editor transport");
}

fn editor_hover(
    meta: &EditorMeta,
    _params: &TextDocumentPositionParams,
    result: Hover,
    ctx: &mut Context,
) {
    let contents = match result.contents {
        HoverContents::Scalar(contents) => contents.plaintext(),
        HoverContents::Array(contents) => contents
            .into_iter()
            .map(|x| x.plaintext())
            .collect::<Vec<String>>()
            .join("\n"),
        HoverContents::Markup(contents) => contents.value,
    };
    if contents.is_empty() {
        return;
    }
    let command = format!("info %§{}§", contents);
    ctx.editor_tx
        .send(EditorResponse {
            meta: meta.clone(),
            command,
        })
        .expect("Failed to send message to editor transport");
}

fn editor_definition(
    meta: &EditorMeta,
    _params: &TextDocumentPositionParams,
    result: GotoDefinitionResponse,
    ctx: &mut Context,
) {
    if let Some(location) = match result {
        GotoDefinitionResponse::Scalar(location) => Some(location),
        GotoDefinitionResponse::Array(mut locations) => Some(locations.remove(0)),
        GotoDefinitionResponse::None => None,
    } {
        let filename = location.uri.path();
        let p = location.range.start;
        let command = format!("edit %§{}§ {} {}", filename, p.line + 1, p.character + 1);
        ctx.editor_tx
            .send(EditorResponse {
                meta: meta.clone(),
                command,
            })
            .expect("Failed to send message to editor transport");
    };
}

fn initialized(
    _meta: &EditorMeta,
    _params: &InitializedParams,
    result: InitializeResult,
    mut ctx: &mut Context,
) {
    ctx.capabilities = Some(result.capabilities);
    let mut requests = Vec::with_capacity(ctx.pending_requests.len());
    for msg in ctx.pending_requests.drain(..) {
        requests.push(msg);
    }

    for msg in requests.drain(..) {
        dispatch_editor_request(msg, &mut ctx);
    }
}

trait PlainText {
    fn plaintext(self) -> String;
}

impl PlainText for MarkedString {
    fn plaintext(self) -> String {
        match self {
            MarkedString::String(contents) => contents,
            MarkedString::LanguageString(contents) => contents.value,
        }
    }
}