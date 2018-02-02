// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Post-build processing of data.

#![allow(missing_docs)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use build::BuildResult;
use lsp_data::{ls_util, PublishDiagnosticsParams};
use CRATE_BLACKLIST;

use analysis::AnalysisHost;
use data::Analysis;
use ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range, Position};
use serde_json;
use span::compiler::DiagnosticSpan;
use url::Url;

pub type BuildResults = HashMap<PathBuf, Vec<(Diagnostic, Vec<Suggestion>)>>;

pub struct PostBuildHandler {
    pub analysis: Arc<AnalysisHost>,
    pub previous_build_results: Arc<Mutex<BuildResults>>,
    pub project_path: PathBuf,
    pub show_warnings: bool,
    pub use_black_list: bool,
    pub notifier: Box<Notifier>,
    pub blocked_threads: Vec<thread::Thread>,
}

/// Trait for communication back to the rest of the RLS (and on to the client).
// This trait only really exists to work around the object safety rules (Output
// is not object-safe).
pub trait Notifier: Send {
    fn notify_begin(&self);
    fn notify_end(&self);
    fn notify_publish(&self, PublishDiagnosticsParams);
}

impl PostBuildHandler {
    pub fn handle(mut self, result: BuildResult) {
        self.notifier.notify_begin();

        match result {
            BuildResult::Success(cwd, messages, new_analysis) => {
                thread::spawn(move || {
                    trace!("build - Success");

                    // Emit appropriate diagnostics using the ones from build.
                    self.handle_messages(&cwd, &messages);

                    // Reload the analysis data.
                    debug!("reload analysis: {:?}", self.project_path);
                    if new_analysis.is_empty() {
                        self.reload_analysis_from_disk(&cwd);
                    } else {
                        self.reload_analysis_from_memory(&cwd, new_analysis);
                    }

                    // Wake up any threads blocked on this analysis.
                    for t in self.blocked_threads.drain(..) {
                        t.unpark();
                    }

                    self.notifier.notify_end();
                });
            }
            BuildResult::Squashed => {
                trace!("build - Squashed");
                self.notifier.notify_end();
            }
            BuildResult::Err => {
                trace!("build - Error");
                self.notifier.notify_end();
            }
        }
    }

    fn handle_messages(&self, cwd: &Path, messages: &[String]) {
        // These notifications will include empty sets of errors for files
        // which had errors, but now don't. This instructs the IDE to clear
        // errors for those files.
        let mut results = self.previous_build_results.lock().unwrap();
        // We must not clear the hashmap, just the values in each list.
        // This allows us to save allocated before memory.
        for v in &mut results.values_mut() {
            v.clear();
        }

        for msg in messages {
            if let Some(FileDiagnostic {
                file_path,
                diagnostic,
                secondaries,
                suggestions,
            }) = parse_diagnostics(msg) {
                let entry = results
                    .entry(cwd.join(file_path))
                    .or_insert_with(Vec::new);

                entry.push((diagnostic, suggestions));
                for secondary in secondaries {
                    entry.push((secondary, vec![]));
                }
            }
        }

        self.emit_notifications(&results);
    }

    fn reload_analysis_from_disk(&self, cwd: &Path) {
        if self.use_black_list {
            self.analysis
                .reload_with_blacklist(&self.project_path, cwd, &CRATE_BLACKLIST)
                .unwrap();
        } else {
            self.analysis.reload(&self.project_path, cwd).unwrap();
        }
    }

    fn reload_analysis_from_memory(&self, cwd: &Path, analysis: Vec<Analysis>) {
        if self.use_black_list {
            self.analysis
                .reload_from_analysis(analysis, &self.project_path, cwd, &CRATE_BLACKLIST)
                .unwrap();
        } else {
            self.analysis
                .reload_from_analysis(analysis, &self.project_path, cwd, &[])
                .unwrap();
        }
    }

    fn emit_notifications(&self, build_results: &BuildResults) {
        for (path, diags) in build_results {
            let mut diagnostics: Vec<_> = diags
                .iter()
                .filter_map(|&(ref d, _)| {
                    if self.show_warnings || d.severity != Some(DiagnosticSeverity::Warning) {
                        Some(d.clone())
                    }
                    else {
                        None
                    }
                })
                .collect();

            // deduplicate diagnostics (keep most severe)
            diagnostics.sort_unstable_by(|a, b| sortable_diagnostic(a).cmp(&sortable_diagnostic(b)));
            diagnostics.dedup_by(|a, b| dupable_diagnostic_bits(a) == dupable_diagnostic_bits(b));

            let params = PublishDiagnosticsParams {
                uri: Url::from_file_path(path).unwrap(),
                diagnostics,
            };

            self.notifier.notify_publish(params);
        }
    }
}

/// Sort order for a Diagnostic
/// sort by position, message, severity (most severe first), source (rustc first)
fn sortable_diagnostic(d: &Diagnostic) -> (Position, Position, &str, usize, &str) {
    use ls_types::DiagnosticSeverity::*;
    (
        d.range.start,
        d.range.end,
        &d.message,
        match d.severity {
            Some(Error) => 0,
            Some(Warning) => 1,
            Some(Information) => 2,
            Some(Hint) => 3,
            _ => 4,
        },
        match d.source.as_ref().map(String::as_str) {
            Some("rustc") => "!!first",
            Some(s) => s,
            _ => "~~last",
        },
    )
}

/// Deduplicate diagnostics have the same position & message
fn dupable_diagnostic_bits(d: &Diagnostic) -> (Position, Position, &str) {
    let (p1, p2, msg, ..) = sortable_diagnostic(d);
    (p1, p2, msg)
}

#[derive(Debug)]
pub struct Suggestion {
    pub range: Range,
    pub new_text: String,
    pub label: String,
}

#[derive(Debug)]
struct FileDiagnostic {
    file_path: PathBuf,
    diagnostic: Diagnostic,
    secondaries: Vec<Diagnostic>,
    suggestions: Vec<Suggestion>,
}

#[derive(Debug, Deserialize)]
struct CompilerMessage {
    message: String,
    code: Option<CompilerMessageCode>,
    level: String,
    spans: Vec<DiagnosticSpan>,
    children: Vec<CompilerMessage>,
}

#[derive(Debug, Deserialize)]
struct CompilerMessageCode {
    code: String,
}

fn parse_diagnostics(message: &str) -> Option<FileDiagnostic> {
    let message = match serde_json::from_str::<CompilerMessage>(message) {
        Ok(m) => m,
        Err(e) => {
            debug!("build error {:?}", e);
            debug!("from {}", message);
            return None;
        }
    };

    if message.spans.is_empty() {
        return None;
    }

    let diagnostic_msg = format!("{}\n", message.message);
    let primary_span = message.spans.iter().find(|s| s.is_primary).unwrap();
    let rls_span = primary_span.rls_span().zero_indexed();
    let suggestions = make_suggestions(&message.children, &rls_span.file);

    let diagnostic = {
        let mut primary_message = diagnostic_msg.clone();
        if let Some(ref primary_label) = primary_span.label {
            if primary_label.trim() != primary_message.trim() {
                primary_message.push_str(&format!("\n{}", primary_label));
            }
        }

        if let Some(notes) = format_notes(&message.children, primary_span) {
            primary_message.push_str(&format!("\n{}", notes));
        }

        Diagnostic {
            range: ls_util::rls_to_range(rls_span.range),
            severity: Some(severity(&message.level)),
            code: Some(NumberOrString::String(match message.code {
                Some(ref c) => c.code.clone(),
                None => String::new(),
            })),
            source: Some("rustc".into()),
            message: primary_message.trim().to_owned(),
        }
    };

    // For a compiler error that has secondary spans (e.g. borrow error showing
    // both borrow and error spans) we emit additional diagnostics. These don't
    // include notes and are of an `Information` severity.
    let secondaries = message
    .spans
    .iter()
    .filter(|x| !x.is_primary)
    .map(|secondary_span| {
        let mut secondary_message = if secondary_span.is_within(primary_span) {
            String::new()
        }
        else {
            diagnostic_msg.clone()
        };

        if let Some(ref secondary_label) = secondary_span.label {
            secondary_message.push_str(&format!("\n{}", secondary_label));
        }
        let rls_span = secondary_span.rls_span().zero_indexed();

        Diagnostic {
            range: ls_util::rls_to_range(rls_span.range),
            severity: Some(DiagnosticSeverity::Information),
            code: Some(NumberOrString::String(match message.code {
                Some(ref c) => c.code.clone(),
                None => String::new(),
            })),
            source: Some("rustc".into()),
            message: secondary_message.trim().to_owned(),
        }
    }).collect();

    Some(FileDiagnostic {
        file_path: rls_span.file,
        diagnostic,
        secondaries,
        suggestions,
    })
}

fn format_notes(children: &[CompilerMessage], primary: &DiagnosticSpan) -> Option<String> {
    if !children.is_empty() {
        let mut notes = String::new();
        for &CompilerMessage { ref message, ref level, ref spans, .. } in children {
            notes.push_str(&format!("\n{}: ", level));

            macro_rules! add_message_to_notes {
                ($msg:expr) => {{
                    let mut lines = message.lines();
                    notes.push_str(lines.next().unwrap());
                    for line in lines {
                        notes.push_str(&format!(
                            "\n{:indent$}{line}",
                            "",
                            indent = level.len() + 2,
                            line = line,
                        ));
                    }
                }}
            }

            if spans.is_empty() {
                add_message_to_notes!(message);
            }
            else if spans.len() == 1 && spans[0].is_within(primary) {
                add_message_to_notes!(message);
                if let Some(ref suggested) = spans[0].suggested_replacement {
                    notes.push_str(&format!(": `{}`", suggested));
                }
            }
        }

        if notes.is_empty() { None } else { Some(notes) }
    }
    else {
        None
    }
}

fn severity(level: &str) -> DiagnosticSeverity {
    if level == "error" {
        DiagnosticSeverity::Error
    } else {
        DiagnosticSeverity::Warning
    }
}

fn make_suggestions(children: &[CompilerMessage], file: &Path) -> Vec<Suggestion> {
    let mut suggestions = vec![];
    for c in children {
        for sp in &c.spans {
            let span = sp.rls_span().zero_indexed();
            if span.file == file {
                if let Some(ref s) = sp.suggested_replacement {
                    let suggestion = Suggestion {
                        new_text: s.clone(),
                        range: ls_util::rls_to_range(span.range),
                        label: format!("{}: `{}`", c.message, s),
                    };
                    suggestions.push(suggestion);
                }
            }
        }
    }
    suggestions
}

trait IsWithin {
    /// Returns whether `other` is considered within `self`
    /// note: a thing should be 'within' itself
    fn is_within(&self, other: &Self) -> bool;
}
impl<T: PartialOrd<T>> IsWithin for ::std::ops::Range<T> {
    fn is_within(&self, other: &Self) -> bool {
        self.start >= other.start &&
            self.start <= other.end &&
            self.end <= other.end &&
            self.end >= other.start
    }
}
impl IsWithin for DiagnosticSpan {
    fn is_within(&self, other: &Self) -> bool {
        let DiagnosticSpan { line_start, line_end, column_start, column_end, .. } = *self;
        (line_start..line_end+1).is_within(&(other.line_start..other.line_end+1)) &&
            (column_start..column_end+1).is_within(&(other.column_start..other.column_end+1))
    }
}

/// Tests for formatted messages from the compilers json output
/// run cargo with `--message-format=json` to generate the json for new tests and add .json
/// message files to '../../test_data/compiler_message/'
#[cfg(test)]
mod diagnostic_message_test {
    use super::*;

    /// Returns (primary message, secondary messages)
    fn parsed_message(compiler_message: &str) -> (String, Vec<String>) {
        let _ = ::env_logger::try_init();
        let parsed = parse_diagnostics(compiler_message)
            .expect("failed to parse compiler message");
        (parsed.diagnostic.message, parsed.secondaries.into_iter().map(|s| s.message).collect())
    }

    /// ```
    /// fn use_after_move() {
    ///     let s = String::new();
    ///     ::std::mem::drop(s);
    ///     ::std::mem::drop(s);
    /// }
    /// ```
    #[test]
    fn message_use_after_move() {
        let (msg, others) = parsed_message(
            include_str!("../../test_data/compiler_message/use-after-move.json")
        );
        assert_eq!(
            msg,
            "use of moved value: `s`\n\n\
            value used here after move\n\n\
            note: move occurs because `s` has type `std::string::String`, which does not implement the `Copy` trait"
        );

        assert_eq!(others, vec![
            "use of moved value: `s`\n\n\
            value moved here"
        ]);
    }

    /// ```
    /// fn type_annotations_needed() {
    ///     let v = Vec::new();
    /// }
    /// ```
    #[test]
    fn message_type_annotations_needed() {
        let (msg, others) = parsed_message(
            include_str!("../../test_data/compiler_message/type-annotations-needed.json")
        );
        assert_eq!(
            msg,
            "type annotations needed\n\n\
            cannot infer type for `T`",
        );

        assert_eq!(others, vec![
            "type annotations needed\n\n\
            consider giving `v` a type"
        ]);
    }

    /// ```
    /// fn mismatched_types() -> usize {
    ///     123_i32
    /// }
    /// ```
    #[test]
    fn message_mismatched_types() {
        let (msg, others) = parsed_message(
            include_str!("../../test_data/compiler_message/mismatched-types.json")
        );
        assert_eq!(
            msg,
            "mismatched types\n\n\
            expected usize, found i32",
        );

        assert_eq!(others, vec![
            "mismatched types\n\n\
            expected `usize` because of return type"
        ]);
    }

    /// ```
    /// fn not_mut() {
    ///     let string = String::new();
    ///     let _s1 = &mut string;
    /// }
    /// ```
    #[test]
    fn message_not_mutable() {
        let (msg, others) = parsed_message(
            include_str!("../../test_data/compiler_message/not-mut.json")
        );
        assert_eq!(
            msg,
            "cannot borrow immutable local variable `string` as mutable\n\n\
            cannot borrow mutably",
        );

        assert_eq!(others, vec![
            "cannot borrow immutable local variable `string` as mutable\n\n\
            consider changing this to `mut string`"
        ]);
    }

    /// ```
    /// fn consider_borrow() {
    ///     fn takes_ref(s: &str) {}
    ///     let string = String::new();
    ///     takes_ref(string);
    /// }
    /// ```
    #[test]
    fn message_consider_borrowing() {
        let (msg, others) = parsed_message(
            include_str!("../../test_data/compiler_message/consider-borrowing.json")
        );
        assert_eq!(
            msg,
            r#"mismatched types

expected &str, found struct `std::string::String`

note: expected type `&str`
         found type `std::string::String`
help: consider borrowing here: `&string`"#,
        );

        assert!(others.is_empty(), "{:?}", others);
    }

    /// ```
    /// fn move_out_of_borrow() {
    ///     match &Some(String::new()) {
    ///         &Some(string) => takes_borrow(&string),
    ///         &None => {},
    ///     }
    /// }
    /// ```
    #[test]
    fn message_move_out_of_borrow() {
        let (msg, others) = parsed_message(
            include_str!("../../test_data/compiler_message/move-out-of-borrow.json")
        );
        assert_eq!(msg, "cannot move out of borrowed content");

        assert_eq!(others, vec!["hint: to prevent move, use `ref string` or `ref mut string`"]);
    }
}
