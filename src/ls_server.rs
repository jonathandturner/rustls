// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use analysis::AnalysisHost;
use vfs::Vfs;
use serde_json;

use build::*;
use lsp_data::*;
use actions_ls::ActionHandler;

use std::env;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write, ErrorKind};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::path::PathBuf;


#[derive(Debug, new)]
struct ParseError {
    kind: ErrorKind,
    message: &'static str,
    id: Option<usize>,
}

#[derive(Debug)]
enum ServerMessage {
    Request(Request),
    Notification(Notification)
}

#[derive(Debug)]
struct Request {
    id: usize,
    method: Method
}

#[derive(Debug)]
enum Method {
    Shutdown,
    Initialize(InitializeParams),
    Hover(TextDocumentPositionParams),
    GotoDef(TextDocumentPositionParams),
    FindAllRef(ReferenceParams),
    Symbols(DocumentSymbolParams),
    Complete(TextDocumentPositionParams),
    CompleteResolve(CompletionItem),
    Rename(RenameParams),
    Reformat(DocumentFormattingParams),
    ReformatRange(DocumentRangeFormattingParams),
}

#[derive(Debug)]
enum Notification {
    CancelRequest(NumberOrString),
    Change(DidChangeTextDocumentParams),
}

/// Creates an public enum whose variants all contain a single serializable payload
/// with an automatic json to_string implementation
macro_rules! serializable_enum {
    ($enum_name:ident, $($variant_name:ident($variant_type:ty)),*) => (

        pub enum $enum_name {
            $(
                $variant_name($variant_type),
            )*
        }

        impl fmt::Display for $enum_name {
            fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
                let value = match *self {
                    $(
                        $enum_name::$variant_name(ref value) => serde_json::to_string(value),
                    )*
                }.unwrap();

                write!(f, "{}", value)
            }
        }
    )
}

serializable_enum!(ResponseData,
    Init(InitializeResult),
    SymbolInfo(Vec<SymbolInformation>),
    CompletionItems(Vec<CompletionItem>),
    WorkspaceEdit(WorkspaceEdit),
    TextEdit([TextEdit; 1]),
    Locations(Vec<Location>),
    HoverSuccess(Hover)
);

// FIXME(45) generate this function.
fn parse_message(input: &str) -> Result<ServerMessage, ParseError>  {
    let ls_command: serde_json::Value = serde_json::from_str(input).unwrap();

    let params = ls_command.lookup("params");

    if let Some(v) = ls_command.lookup("method") {
        if let Some(name) = v.as_str() {
            match name {
                "shutdown" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    Ok(ServerMessage::Request(Request{id: id, method: Method::Shutdown }))
                }
                "initialize" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let method: InitializeParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::Initialize(method)}))
                }
                "textDocument/hover" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let method: TextDocumentPositionParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::Hover(method)}))
                }
                "textDocument/didChange" => {
                    let method: DidChangeTextDocumentParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Notification(Notification::Change(method)))
                }
                "textDocument/didOpen" => {
                    // TODO handle me
                    Err(ParseError::new(ErrorKind::InvalidData, "didOpen", None))
                }
                "textDocument/definition" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let method: TextDocumentPositionParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::GotoDef(method)}))
                }
                "textDocument/references" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let method: ReferenceParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::FindAllRef(method)}))
                }
                "textDocument/completion" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let method: TextDocumentPositionParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::Complete(method)}))
                }
                "completionItem/resolve" => {
                    // currently, we safely ignore this as a pass-through since we fully handle
                    // textDocument/completion.  In the future, we may want to use this method as a
                    // way to more lazily fill out completion information
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let method: CompletionItem =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::CompleteResolve(method)}))
                }
                "textDocument/documentSymbol" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let method: DocumentSymbolParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::Symbols(method)}))
                }
                "textDocument/rename" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let method: RenameParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::Rename(method)}))
                }
                "textDocument/formatting" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let params: DocumentFormattingParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::Reformat(params)}))
                }
                "textDocument/rangeFormatting" => {
                    let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                    let params: DocumentRangeFormattingParams =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    Ok(ServerMessage::Request(Request{id: id, method: Method::ReformatRange(params)}))
                }
                "$/cancelRequest" => {
                    let params: CancelParams = serde_json::from_value(params.unwrap().to_owned())
                                               .unwrap();
                    Ok(ServerMessage::Notification(Notification::CancelRequest(params.id)))
                }
                "$/setTraceNotification" => {
                    // TODO handle me
                    Err(ParseError::new(ErrorKind::InvalidData, "setTraceNotification", None))
                }
                "workspace/didChangeConfiguration" => {
                    // TODO handle me
                    Err(ParseError::new(ErrorKind::InvalidData, "didChangeConfiguration", None))
                }
                _ => {
                    let id = ls_command.lookup("id").map(|id| id.as_u64().unwrap() as usize);
                    Err(ParseError::new(ErrorKind::InvalidData, "Unknown command", id))
                }
            }
        }
        else {
            let id = ls_command.lookup("id").map(|id| id.as_u64().unwrap() as usize);
            Err(ParseError::new(ErrorKind::InvalidData, "Method is not a string", id))
        }
    }
    else {
        let id = ls_command.lookup("id").map(|id| id.as_u64().unwrap() as usize);
        Err(ParseError::new(ErrorKind::InvalidData, "Method not found", id))
    }
}

pub struct LsService {
    logger: Arc<Logger>,
    shut_down: AtomicBool,
    msg_reader: Box<MessageReader + Sync + Send>,
    output: Box<Output + Sync + Send>,
    handler: ActionHandler,
}

#[derive(Eq, PartialEq, Debug, Clone, Copy)]
pub enum ServerStateChange {
    Continue,
    Break,
}

impl LsService {    
    pub fn new(analysis: Arc<AnalysisHost>,
               vfs: Arc<Vfs>,
               build_queue: Arc<BuildQueue>,
               reader: Box<MessageReader + Send + Sync>,
               output: Box<Output + Send + Sync>,
               logger: Arc<Logger>)
               -> Arc<LsService> {
        Arc::new(LsService {
            logger: logger.clone(),
            shut_down: AtomicBool::new(false),
            msg_reader: reader,
            output: output,
            handler: ActionHandler::new(analysis, vfs, build_queue, logger),
        })
    }

    pub fn run(this: Arc<Self>) {
        while !this.shut_down.load(Ordering::SeqCst) && LsService::handle_message(this.clone()) == ServerStateChange::Continue {}
    }

    fn init(&self, id: usize, init: InitializeParams) {
        let result = InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncKind::Incremental),
                hover_provider: Some(true),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    trigger_characters: vec![".".to_string()],
                }),
                // TODO
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![]),
                }),
                definition_provider: Some(true),
                references_provider: Some(true),
                // TODO
                document_highlight_provider: Some(false),
                document_symbol_provider: Some(true),
                workspace_symbol_provider: Some(true),
                code_action_provider: Some(false),
                // TODO maybe?
                code_lens_provider: None,
                document_formatting_provider: Some(true),
                document_range_formatting_provider: Some(true),
                document_on_type_formatting_provider: None, // TODO: review this, maybe add?
                rename_provider: Some(true),
            }
        };
        self.output.success(id, ResponseData::Init(result));
        let root_path = init.root_path.map(|str| PathBuf::from(str));
        self.handler.init(root_path, &*self.output);
    }

    pub fn handle_message(this: Arc<Self>) -> ServerStateChange {
        let c = match this.msg_reader.read_message() {
            Some(c) => c,
            None => return ServerStateChange::Break,
        };

        let this = this.clone();
        thread::spawn(move || {
            // FIXME(45) refactor to generate this match.
            match parse_message(&c) {
                Ok(ServerMessage::Notification(Notification::CancelRequest(id))) => {
                    this.logger.log(&format!("request to cancel {:?}\n", id));
                },
                Ok(ServerMessage::Notification(Notification::Change(change))) => {
                    this.logger.log(&format!("notification(change): {:?}\n", change));
                    this.handler.on_change(change, &*this.output);
                }
                Ok(ServerMessage::Request(Request{id, method})) => {
                    match method {
                        Method::Initialize(init) => {
                            this.logger.log(&format!("command(init): {:?}\n", init));
                            this.init(id, init);
                        }
                        Method::Shutdown => {
                            this.logger.log(&format!("shutting down...\n"));
                            this.shut_down.store(true, Ordering::SeqCst);
                        }
                        Method::Hover(params) => {
                            this.logger.log(&format!("command(hover): {:?}\n", params));
                            this.handler.hover(id, params, &*this.output);
                        }
                        Method::GotoDef(params) => {
                            this.logger.log(&format!("command(goto): {:?}\n", params));
                            this.handler.goto_def(id, params, &*this.output);
                        }
                        Method::Complete(params) => {
                            this.logger.log(&format!("command(complete): {:?}\n", params));
                            this.handler.complete(id, params, &*this.output);
                        }
                        Method::CompleteResolve(params) => {
                            this.logger.log(&format!("command(complete): {:?}\n", params));
                            this.output.success(id, ResponseData::CompletionItems(vec![params]))
                        }
                        Method::Symbols(params) => {
                            this.logger.log(&format!("command(goto): {:?}\n", params));
                            this.handler.symbols(id, params, &*this.output);
                        }
                        Method::FindAllRef(params) => {
                            this.logger.log(&format!("command(find_all_refs): {:?}\n", params));
                            this.handler.find_all_refs(id, params, &*this.output);
                        }
                        Method::Rename(params) => {
                            this.logger.log(&format!("command(rename): {:?}\n", params));
                            this.handler.rename(id, params, &*this.output);
                        }
                        Method::Reformat(params) => {
                            // FIXME take account of options.
                            this.logger.log(&format!("command(reformat): {:?}\n", params));
                            this.handler.reformat(id, params.text_document, &*this.output);
                        }
                        Method::ReformatRange(params) => {
                            // FIXME reformats the whole file, not just a range.
                            // FIXME take account of options.
                            this.logger.log(&format!("command(reformat): {:?}\n", params));
                            this.handler.reformat(id, params.text_document, &*this.output);
                        }
                    }
                }
                Err(e) => {
                    this.logger.log(&format!("parsing invalid message: {:?}", e));
                    if let Some(id) = e.id {
                        this.output.failure(id, "Unsupported message");
                    }
                },
            }
        });
        ServerStateChange::Continue
    }
}

pub struct Logger {
    log_file: Mutex<File>,
}

impl Logger {
    pub fn new() -> Logger {
        // note: logging is totally optional, but it gives us a way to see behind the scenes
        let log_file = OpenOptions::new().append(true)
                                         .write(true)
                                         .create(true)
                                         .open(env::temp_dir().join("rls_log.txt"))
                                         .expect("Couldn't open log file");
        Logger {
            log_file: Mutex::new(log_file),
        }
    }

    pub fn log(&self, s: &str) {
        let mut log_file = self.log_file.lock().unwrap();
        // FIXME(#40) write thread id to log_file
        log_file.write_all(s.as_bytes()).unwrap();
        // writeln!(::std::io::stderr(), "{}", msg);
    }
}

pub trait MessageReader {
    fn read_message(&self) -> Option<String>;
}

struct StdioMsgReader {
    logger: Arc<Logger>,
}

impl MessageReader for StdioMsgReader {
    fn read_message(&self) -> Option<String> {
        macro_rules! handle_err {
            ($e: expr, $s: expr) => {
                match $e {
                    Ok(x) => x,
                    Err(_) => {
                        self.logger.log($s);
                        return None;
                    }
                }
            }
        }

        // Read in the "Content-length: xx" part
        let mut buffer = String::new();
        handle_err!(io::stdin().read_line(&mut buffer), "Could not read from stdin");

        let res: Vec<&str> = buffer.split(" ").collect();

        // Make sure we see the correct header
        if res.len() != 2 {
            self.logger.log("Header is malformed");
            return None;
        }

        if res[0] == "Content-length:" {
            self.logger.log("Header is missing 'Content-length'");
            return None;
        }

        let size = handle_err!(usize::from_str_radix(&res[1].trim(), 10), "Couldn't read size");
        self.logger.log(&format!("now reading: {} bytes\n", size));

        // Skip the new lines
        let mut tmp = String::new();
        handle_err!(io::stdin().read_line(&mut tmp), "Could not read from stdin");

        let mut content = vec![0; size];
        handle_err!(io::stdin().read_exact(&mut content), "Could not read from stdin");

        let content = handle_err!(String::from_utf8(content), "Non-utf8 input");

        self.logger.log(&format!("in came: {}\n", content));

        Some(content)
    }
}

pub trait Output {
    fn response(&self, output: String);

    fn failure(&self, id: usize, message: &str) {
        // For now this is a catch-all for any error back to the consumer of the RLS
        const METHOD_NOT_FOUND: i64 = -32601;

        #[derive(Serialize)]
        struct ResponseError {
            code: i64,
            message: String
        }

        #[derive(Serialize)]
        struct ResponseFailure {
            jsonrpc: &'static str,
            id: usize,
            error: ResponseError,
        }

        let rf = ResponseFailure {
            jsonrpc: "2.0",
            id: id,
            error: ResponseError {
                code: METHOD_NOT_FOUND,
                message: message.to_owned(),
            },
        };
        let output = serde_json::to_string(&rf).unwrap();
        self.response(output);
    }

    fn success(&self, id: usize, data: ResponseData) {
        // {
        //     jsonrpc: String,
        //     id: usize,
        //     result: String,
        // }
        let output = format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}", id, data);

        self.response(output);
    }

    fn notify(&self, message: &str) {
        let output = serde_json::to_string(
            &NotificationMessage::new(message.to_owned(), ())
        ).unwrap();
        self.response(output);
    }
}

struct StdioOutput {
    logger: Arc<Logger>,
}

impl Output for StdioOutput {
    fn response(&self, output: String) {
        let o = format!("Content-Length: {}\r\n\r\n{}", output.len(), output);

        self.logger.log(&format!("OUTPUT: {:?}", o));

        print!("{}", o);
        io::stdout().flush().unwrap();
    }
}

pub fn run_server(analysis: Arc<AnalysisHost>, vfs: Arc<Vfs>, build_queue: Arc<BuildQueue>) {
    let logger = Arc::new(Logger::new());
    let service = LsService::new(analysis,
                                 vfs,
                                 build_queue,
                                 Box::new(StdioMsgReader { logger: logger.clone() }),
                                 Box::new(StdioOutput { logger: logger.clone() } ),
                                 logger);
    LsService::run(service);
}
