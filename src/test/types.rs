// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::io::{BufRead, BufReader};

use analysis::Span;
use ide::{Input, SaveInput, Position};
use serde_json;

#[derive(Clone, Copy, Debug)]
pub struct Src<'a, 'b> {
    pub file_name: &'a Path,
    // 1 indexed
    pub line: usize,
    pub name: &'b str,
}

pub fn src<'a, 'b>(file_name: &'a Path, line: usize, name: &'b str) -> Src<'a, 'b> {
    Src {
        file_name: file_name,
        line: line,
        name: name,
    }
}

pub struct Cache {
    base_path: PathBuf,
    files: HashMap<PathBuf, Vec<String>>,
}

impl Cache {
    pub fn new(base_path: &Path) -> Cache {
        Cache {
            base_path: base_path.to_owned(),
            files: HashMap::new(),
        }
    }

    pub fn mk_span(&mut self, src: Src) -> Span {
        let line = self.get_line(src);
        let col = line.find(src.name).expect(&format!("Line does not contain name {}", src.name));
        Span {
            file_name: self.abs_path(src.file_name),
            line_start: src.line - 1,
            line_end: src.line - 1,
            column_start: char_of_byte_index(&line, col),
            column_end: char_of_byte_index(&line, col + src.name.len()),
        }
    }

    pub fn mk_position(&mut self, src: Src) -> Position {
        let line = self.get_line(src);
        let col = line.find(src.name).expect(&format!("Line does not contain name {}", src.name));
        Position {
            filepath: self.abs_path(src.file_name),
            line: src.line - 1,
            col: char_of_byte_index(&line, col),
        }
    }

    pub fn mk_ls_position(&mut self, src: Src) -> String {
        let line = self.get_line(src);
        let col = line.find(src.name).expect(&format!("Line does not contain name {}", src.name));
        format!("{{\"line\":\"{}\",\"character\":\"{}\"}}", src.line - 1, char_of_byte_index(&line, col))
    }

    pub fn abs_path(&self, file_name: &Path) -> PathBuf {
        let result = self.base_path.join(file_name).canonicalize().expect("Couldn't canonicalise path");
        let result = if cfg!(windows) {
            // FIXME: If the \\?\ prefix is not stripped from the canonical path, the HTTP server tests fail. Why?
            let result_string = result.to_str().expect("Path contains non-utf8 characters.");
            PathBuf::from(&result_string[r"\\?\".len()..])
        } else {
            result
        };
        result
    }

    pub fn mk_input(&mut self, src: Src) -> Vec<u8> {
        let span = self.mk_span(src);
        let pos = self.mk_position(src);
        let input = Input { pos: pos, span: span };

        let s = serde_json::to_string(&input).unwrap();
        let s = format!("{{{}}}", s.replace("\"", "\\\""));
        s.as_bytes().to_vec()
    }

    pub fn mk_save_input(&self, file_name: &Path) -> Vec<u8> {
        let input = SaveInput {
            project_path: self.abs_path(Path::new(".")),
            saved_file: file_name.to_owned(),
        };
        let s = serde_json::to_string(&input).unwrap();
        let s = format!("{{{}}}", s.replace("\"", "\\\""));
        s.as_bytes().to_vec()
    }

    fn get_line(&mut self, src: Src) -> String {
        let base_path = &self.base_path;
        let lines = self.files.entry(src.file_name.to_owned()).or_insert_with(|| {
            let file_name = &base_path.join(src.file_name);
            let file = File::open(file_name).expect(&format!("Couldn't find file: {:?}", file_name));
            let lines = BufReader::new(file).lines();
            lines.collect::<Result<Vec<_>, _>>().unwrap()
        });

        if src.line - 1 >= lines.len() {
            panic!("Line {} not in file, found {} lines", src.line, lines.len());
        }

        lines[src.line - 1].to_owned()
    }
}

fn char_of_byte_index(s: &str, byte: usize) -> usize {
    for (c, (b, _)) in s.char_indices().enumerate() {
        if b == byte {
            return c;
        }
    }

    panic!("Couldn't find byte {} in {:?}", byte, s);
}
