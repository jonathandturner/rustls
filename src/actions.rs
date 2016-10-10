extern crate racer;
extern crate rustfmt;

use analysis::{AnalysisHost, Span};
use self::racer::core::complete_from_file;
use self::racer::core::find_definition;
use self::racer::core;
use self::rustfmt::{Input as FmtInput, format_input};
use self::rustfmt::config::{self, WriteMode};

use std::default::Default;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ide::{Input, Output, FmtOutput, VscodeKind};
use vfs::Vfs;

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct Position {
    pub filepath: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Serialize, Eq, PartialEq, Deserialize)]
pub enum Provider {
    Compiler,
    Racer,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Completion {
    pub name: String,
    pub context: String,
}

#[derive(Debug, Serialize)]
pub struct Title {
    pub ty: String,
    pub docs: String,
    pub doc_url: String,
}

#[derive(Debug, Serialize)]
pub struct Symbol {
    pub name: String,
    pub kind: VscodeKind,
    pub span: Span,
}

// Timeout = 0.5s (totally arbitrary).
const RUSTW_TIMEOUT: u64 = 500;

pub fn complete(pos: Position, _analysis: Arc<AnalysisHost>, vfs: Arc<Vfs>) -> Vec<Completion> {
    let vfs: &Vfs = &vfs;
    panic::catch_unwind(|| {
        let pos = adjust_vscode_pos_for_racer(pos);
        let file_path = &Path::new(&pos.filepath);

        let cache = core::FileCache::new();
        let session = core::Session::from_path(&cache, file_path, file_path);
        for (path, txt) in vfs.get_changed_files() {
            session.cache_file_contents(&path, txt);
        }

        let src = session.load_file(file_path);

        let pos = session.load_file(file_path).coords_to_point(pos.line, pos.col).unwrap();
        let results = complete_from_file(&src.code, file_path, pos, &session);

        results.map(|comp| Completion {
            name: comp.matchstr.clone(),
            context: comp.contextstr.clone(),
        }).collect()
    }).unwrap_or(vec![])
}

pub fn find_refs(source: Input, analysis: Arc<AnalysisHost>) -> Vec<Span> {
    let t = thread::current();
    let span = source.span;
    info!("title for: {:?}", span);
    let rustw_handle = thread::spawn(move || {
        let result = analysis.find_all_refs(&span);
        t.unpark();

        info!("rustw find_all_refs: {:?}", result);
        result
    });

    thread::park_timeout(Duration::from_millis(RUSTW_TIMEOUT));

    rustw_handle.join().ok().and_then(|t| t.ok()).unwrap_or(vec![])
}

pub fn fmt(file_name: &str, vfs: Arc<Vfs>) -> FmtOutput {
    let path = PathBuf::from(file_name);
    let input = match vfs.get_file_changes(&path) {
        Some(s) => FmtInput::Text(s),
        None => FmtInput::File(path),
    };

    let mut config = config::Config::default();
    config.skip_children = true;
    config.write_mode = WriteMode::Plain;

    let mut buf = Vec::<u8>::new();
    match format_input(input, &config, Some(&mut buf)) {
        Ok(_) => FmtOutput::Change(String::from_utf8(buf).unwrap()),
        Err(_) => FmtOutput::Err,
    }
}

pub fn goto_def(source: Input, analysis: Arc<AnalysisHost>, vfs: Arc<Vfs>) -> Output {
    // Save-analysis thread.
    let t = thread::current();
    let span = source.span;
    let compiler_handle = thread::spawn(move || {
        let result = if let Ok(s) = analysis.goto_def(&span) {
            info!("compiler success!");
            Some(Position {
                filepath: s.file_name,
                line: s.line_start,
                col: s.column_start,
            })
        } else {
            info!("compiler failed");
            None
        };

        t.unpark();

        result
    });

    // Racer thread.
    let pos = adjust_vscode_pos_for_racer(source.pos);
    let racer_handle = thread::spawn(move || {
        let file_path = &Path::new(&pos.filepath);

        let cache = core::FileCache::new();
        let session = core::Session::from_path(&cache, file_path, file_path);
        for (path, txt) in vfs.get_changed_files() {
            session.cache_file_contents(&path, txt);
        }

        let src = session.load_file(file_path);

        find_definition(&src.code,
                        file_path,
                        src.coords_to_point(pos.line, pos.col).unwrap(),
                        &session)
            .and_then(|mtch| {
                let source_path = &mtch.filepath;
                if mtch.point != 0 {
                    let (line, col) = session.load_file(source_path)
                                             .point_to_coords(mtch.point)
                                             .unwrap();
                    let fpath = source_path.to_str().unwrap().to_owned();
                    Some(Position {
                        filepath: fpath,
                        line: line,
                        col: col,
                    })
                } else {
                    None
                }
            })
    });

    thread::park_timeout(Duration::from_millis(RUSTW_TIMEOUT));

    let compiler_result = compiler_handle.join().unwrap_or(None);
    match compiler_result {
        Some(r) => Output::Ok(r, Provider::Compiler),
        None => {
            info!("Using racer");
            match racer_handle.join() {
                Ok(Some(r)) => {
                    Output::Ok(adjust_racer_pos_for_vscode(r), Provider::Racer)
                }
                _ => Output::Err,
            }
        }
    }
}

pub fn title(source: Input, analysis: Arc<AnalysisHost>) -> Option<Title> {
    let t = thread::current();
    let span = source.span;
    info!("title for: {:?}", span);
    let rustw_handle = thread::spawn(move || {
        let ty = analysis.show_type(&span).unwrap_or(String::new());
        let docs = analysis.docs(&span).unwrap_or(String::new());
        let doc_url = analysis.doc_url(&span).unwrap_or(String::new());
        t.unpark();

        info!("rustw show_type: {:?}", ty);
        info!("rustw docs: {:?}", docs);
        info!("rustw doc url: {:?}", doc_url);
        Title {
            ty: ty,
            docs: docs,
            doc_url: doc_url,
        }
    });

    thread::park_timeout(Duration::from_millis(RUSTW_TIMEOUT));

    rustw_handle.join().ok()
}

pub fn symbols(file_name: String, analysis: Arc<AnalysisHost>) -> Vec<Symbol> {
    let t = thread::current();
    let rustw_handle = thread::spawn(move || {
        let symbols = analysis.symbols(&file_name).unwrap_or(vec![]);
        t.unpark();

        symbols.into_iter().map(|s| {
            Symbol {
                name: s.name,
                kind: VscodeKind::from(s.kind),
                span: s.span,
            }
        }).collect()
    });

    thread::park_timeout(Duration::from_millis(RUSTW_TIMEOUT));

    rustw_handle.join().unwrap_or(vec![])
}


fn adjust_vscode_pos_for_racer(mut source: Position) -> Position {
    source.line += 1;
    source
}

fn adjust_racer_pos_for_vscode(mut source: Position) -> Position {
    if source.line > 0 {
        source.line -= 1;
    }
    source
}
