//! Conversion of raw rustc-emitted JSON messages into LSP diagnostics.
//!
//! Data definitions for diagnostics can be found in the Rust compiler for:
//! 1. Internal diagnostics at `src/librustc_errors/diagnostic.rs`.
//! 2. Emitted JSON format at `src/librustc_errors/json.rs`.

use std::collections::HashMap;
use std::iter;
use std::path::{Path, PathBuf};

use crate::lsp_data::ls_util;
use log::debug;
use lsp_types::{
    DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Range,
};
use rls_span::compiler::DiagnosticSpan;
use serde_derive::Deserialize;
use url::Url;

pub use lsp_types::Diagnostic;

#[derive(Debug)]
pub struct Suggestion {
    pub range: Range,
    pub new_text: String,
    pub label: String,
}

#[derive(Debug)]
pub struct ParsedDiagnostics {
    pub diagnostics: HashMap<PathBuf, Vec<(Diagnostic, Vec<Suggestion>)>>,
}

/// Deserialized JSON diagnostic that was emitted by rustc.
#[derive(Debug, Deserialize)]
struct CompilerMessage {
    message: String,
    code: Option<CompilerMessageCode>,
    level: String,
    spans: Vec<DiagnosticSpan>,
    children: Vec<AssociatedMessage>,
}

/// Represents an emitted subdiagnostic for a certain message. Rustc also emits
/// always empty `code`, `children` and `rendered` fields, which we intentionally
/// ignore here.
#[derive(Debug, Deserialize)]
struct AssociatedMessage {
    message: String,
    level: String,
    spans: Vec<DiagnosticSpan>,
}

#[derive(Debug, Deserialize)]
struct CompilerMessageCode {
    code: String,
}

pub fn parse_diagnostics(
    message: &str,
    cwd: &Path,
    related_information_support: bool,
) -> Option<ParsedDiagnostics> {
    let message = match serde_json::from_str::<CompilerMessage>(message) {
        Ok(m) => m,
        Err(e) => {
            debug!("build error {:?}", e);
            debug!("from {}", message);
            return None;
        }
    };

    // Only messages with spans are useful - those without it are often general
    // information, like "aborting due to X previous errors".
    if message.spans.is_empty() {
        return None;
    }

    // A single compiler message can consist of multiple primary spans, each
    // corresponding to equally important main cause of the same reported
    // error/warning type. Because of this, we split those into multiple LSP
    // diagnostics, since they can contain a single primary range. Those will
    // also share any additional notes, suggestions, and secondary spans emitted
    // by rustc, in a form of LSP diagnostic related information.
    let (primaries, secondaries): (Vec<DiagnosticSpan>, Vec<DiagnosticSpan>) =
        message.spans.iter().cloned().partition(|span| span.is_primary);

    let mut diagnostics = HashMap::new();

    // If the client doesn't support related information, emit separate diagnostics for
    // secondary spans.
    let diagnostic_spans = if related_information_support { &primaries } else { &message.spans };

    for (path, diagnostic) in diagnostic_spans.iter().map(|span| {
        let children = || message.children.iter().flat_map(|msg| &msg.spans);
        let all_spans = || iter::once(span).chain(&secondaries).chain(children());

        let suggestions = make_suggestions(span, all_spans());
        let related_information = if related_information_support {
            Some(make_related_information(all_spans(), cwd))
        } else {
            None
        };

        let diagnostic_message = {
            let mut diagnostic_message = message.message.clone();

            if let Some(ref label) = span.label {
                diagnostic_message.push_str(&format!("\n\n{}", label));
            }

            if let Some(notes) = format_notes(&message.children, span) {
                diagnostic_message.push_str(&format!("\n\n{}", notes));
            }
            diagnostic_message
        };

        // A diagnostic source is quite likely to be clippy if it contains
        // the further information link to the rust-clippy project.
        let source = if diagnostic_message.contains("rust-clippy") { "clippy" } else { "rustc" };

        let rls_span = {
            let mut span = span;
            // If span points to a macro, search through the expansions
            // for a more useful source location.
            while span.file_name.ends_with(" macros>") && span.expansion.is_some() {
                span = &span.expansion.as_ref().unwrap().span;
            }
            span.rls_span().zero_indexed()
        };

        let file_path = cwd.join(&rls_span.file);

        let diagnostic = Diagnostic {
            range: ls_util::rls_to_range(rls_span.range),
            severity: Some(severity(&message.level, span.is_primary)),
            code: Some(NumberOrString::String(match message.code {
                Some(ref c) => c.code.clone(),
                None => String::new(),
            })),
            source: Some(source.to_owned()),
            message: diagnostic_message,
            related_information,
        };

        (file_path, (diagnostic, suggestions))
    }) {
        diagnostics.entry(path).or_insert_with(Vec::new).push(diagnostic);
    }

    Some(ParsedDiagnostics { diagnostics })
}

fn format_notes(children: &[AssociatedMessage], primary: &DiagnosticSpan) -> Option<String> {
    let mut notes = String::new();

    for &AssociatedMessage { ref message, ref level, ref spans, .. } in children {
        macro_rules! add_message_to_notes {
            ($msg:expr) => {{
                let mut lines = message.lines();
                notes.push_str(&format!("\n{}: {}", level, lines.next().unwrap()));
                for line in lines {
                    notes.push_str(&format!(
                        "\n{:indent$}{line}",
                        "",
                        indent = level.len() + 2,
                        line = line,
                    ));
                }
            }};
        }

        if spans.is_empty() {
            add_message_to_notes!(message);
        } else if spans.len() == 1 && spans[0].is_within(primary) {
            add_message_to_notes!(message);
            if let Some(ref suggested) = spans[0].suggested_replacement {
                if !suggested.is_empty() {
                    // Only show the suggestion when it is non-empty.
                    // This matches rustc's behavior.
                    notes.push_str(&format!(": `{}`", suggested));
                }
            }
        }
    }

    if notes.is_empty() {
        None
    } else {
        Some(notes.trim().to_string())
    }
}

fn severity(level: &str, is_primary_span: bool) -> DiagnosticSeverity {
    match (level, is_primary_span) {
        (_, false) => DiagnosticSeverity::Information,
        ("error", _) => DiagnosticSeverity::Error,
        (..) => DiagnosticSeverity::Warning,
    }
}

fn make_related_information<'a>(
    spans: impl Iterator<Item = &'a DiagnosticSpan>,
    cwd: &Path,
) -> Vec<DiagnosticRelatedInformation> {
    let mut related_information: Vec<DiagnosticRelatedInformation> = spans
        .filter_map(|span| {
            let rls_span = span.rls_span().zero_indexed();

            span.label.as_ref().map(|label| DiagnosticRelatedInformation {
                location: Location {
                    uri: Url::from_file_path(cwd.join(&rls_span.file)).unwrap(),
                    range: ls_util::rls_to_range(rls_span.range),
                },
                message: label.trim().to_owned(),
            })
        })
        .collect();

    related_information.sort_by_key(|info| info.location.range.start);

    related_information
}

fn make_suggestions<'a>(
    primary: &DiagnosticSpan,
    spans: impl Iterator<Item = &'a DiagnosticSpan>,
) -> Vec<Suggestion> {
    let primary_range = ls_util::rls_to_range(primary.rls_span().zero_indexed().range);

    let mut suggestions: Vec<Suggestion> = spans
        .filter_map(|span| {
            span.suggested_replacement
                .as_ref()
                .map(|suggested| span_suggestion(span, suggested))
                .or_else(|| span.label.as_ref().and_then(|label| label_suggestion(span, label)))
        })
        .collect();

    // Suggestions are displayed at primary span, so if the change is somewhere
    // else, be sure to specify that.
    // TODO: In theory this can even point to different files -- does that happen in practice?
    for suggestion in &mut suggestions {
        if !suggestion.range.is_within(&primary_range) {
            let line = suggestion.range.start.line + 1; // as 1-based
            suggestion.label.insert_str(0, &format!("Line {}: ", line));
        }
    }

    suggestions
}

fn span_suggestion(span: &DiagnosticSpan, suggested: &str) -> Suggestion {
    let rls_span = span.rls_span().zero_indexed();
    let range = ls_util::rls_to_range(rls_span.range);
    let action = if range.start == range.end { "Add" } else { "Change to" };
    let label = format!("{} `{}`", action, suggested);
    Suggestion { new_text: suggested.to_string(), range, label }
}

fn label_suggestion(span: &DiagnosticSpan, label: &str) -> Option<Suggestion> {
    let suggest_label = "consider changing this to `";
    if label.starts_with(suggest_label) && label.ends_with('`') {
        let suggested_replacement = &label[suggest_label.len()..label.len() - 1];
        return Some(span_suggestion(span, suggested_replacement));
    }
    None
}

trait IsWithin {
    /// Returns whether `other` is considered within `self`
    /// NOTE: a thing should be 'within' itself.
    fn is_within(&self, other: &Self) -> bool;
}

impl<T: PartialOrd<T>> IsWithin for std::ops::RangeInclusive<T> {
    fn is_within(&self, other: &Self) -> bool {
        self.start() >= other.start()
            && self.start() <= other.end()
            && self.end() <= other.end()
            && self.end() >= other.start()
    }
}

impl IsWithin for DiagnosticSpan {
    fn is_within(&self, other: &Self) -> bool {
        let DiagnosticSpan { line_start, line_end, column_start, column_end, .. } = *self;
        (line_start..=line_end).is_within(&(other.line_start..=other.line_end))
            && (column_start..=column_end).is_within(&(other.column_start..=other.column_end))
    }
}

impl IsWithin for Range {
    fn is_within(&self, other: &Self) -> bool {
        (self.start.line..=self.end.line).is_within(&(other.start.line..=other.end.line))
            && (self.start.character..=self.end.character)
                .is_within(&(other.start.character..=other.end.character))
    }
}

/// Tests for formatted messages from the compiler's JSON output.
/// Runs cargo with `--message-format=json` to generate the JSON for new tests and add JSON
/// message files to the `$FIXTURES_DIR/compiler_message/` directory.
#[cfg(test)]
mod diagnostic_message_test {
    use super::*;
    use lsp_types::Position;

    pub(super) fn fixtures_dir() -> &'static Path {
        Path::new(env!("FIXTURES_DIR"))
    }

    pub(super) fn read_fixture(path: impl AsRef<Path>) -> String {
        std::fs::read_to_string(fixtures_dir().join(path.as_ref())).unwrap()
    }

    pub(super) fn parse_compiler_message(
        compiler_message: &str,
        with_related_information: bool,
    ) -> ParsedDiagnostics {
        let _ = ::env_logger::try_init();
        let cwd = ::std::env::current_dir().unwrap();
        parse_diagnostics(compiler_message, &cwd, with_related_information)
            .expect("failed to parse compiler message")
    }

    pub(super) trait FileDiagnosticTestExt {
        fn single_file_results(&self) -> &Vec<(Diagnostic, Vec<Suggestion>)>;
        /// Returns `(primary message, secondary messages)`.
        fn to_messages(&self) -> Vec<(String, Vec<String>)>;
        fn to_primary_messages(&self) -> Vec<String>;
        fn to_secondary_messages(&self) -> Vec<String>;
    }

    impl FileDiagnosticTestExt for ParsedDiagnostics {
        fn single_file_results(&self) -> &Vec<(Diagnostic, Vec<Suggestion>)> {
            self.diagnostics.values().nth(0).unwrap()
        }

        fn to_messages(&self) -> Vec<(String, Vec<String>)> {
            self.single_file_results()
                .iter()
                .map(|(diagnostic, _)| {
                    (
                        diagnostic.message.clone(),
                        diagnostic
                            .related_information
                            .as_ref()
                            .unwrap_or(&Vec::new())
                            .iter()
                            .map(|d| d.message.clone())
                            .collect(),
                    )
                })
                .collect()
        }

        fn to_primary_messages(&self) -> Vec<String> {
            self.to_messages().iter().map(|(p, _)| p.clone()).collect()
        }

        fn to_secondary_messages(&self) -> Vec<String> {
            self.to_messages().iter().flat_map(|(_, s)| s.clone()).collect()
        }
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
        let diag =
            parse_compiler_message(&read_fixture("compiler_message/use-after-move.json"), true);

        let diagnostic = &diag.diagnostics.values().nth(0).unwrap()[0];

        assert_eq!(diagnostic.0.source, Some("rustc".into()));

        let messages = diag.to_messages();
        assert_eq!(
            messages[0].0,
            "use of moved value: `s`\n\n\
            value used here after move\n\n\
            note: move occurs because `s` has type `std::string::String`, which does not implement the `Copy` trait"
        );

        assert_eq!(messages[0].1, vec!["value moved here", "value used here after move"]);
    }

    /// ```
    /// fn type_annotations_needed() {
    ///     let v = Vec::new();
    /// }
    /// ```
    #[test]
    fn message_type_annotations_needed() {
        let messages = parse_compiler_message(
            &read_fixture("compiler_message/type-annotations-needed.json"),
            true,
        )
        .to_messages();
        assert_eq!(
            messages[0].0,
            "type annotations needed\n\n\
             cannot infer type for `T`",
        );

        assert_eq!(messages[0].1, vec!["consider giving `v` a type", "cannot infer type for `T`"]);

        // Check if we don't emit related information if it's not supported and
        // if secondary spans are emitted as separate diagnostics.
        let messages = parse_compiler_message(
            &read_fixture("compiler_message/type-annotations-needed.json"),
            false,
        );

        assert_eq!(
            messages.to_primary_messages(),
            vec![
                "type annotations needed\n\n\
                 cannot infer type for `T`",
                "type annotations needed\n\n\
                 consider giving `v` a type",
            ]
        );

        let secondaries = messages.to_secondary_messages();
        assert!(secondaries.is_empty(), "{:?}", secondaries);
    }

    /// ```
    /// fn mismatched_types() -> usize {
    ///     123_i32
    /// }
    /// ```
    #[test]
    fn message_mismatched_types() {
        let messages =
            parse_compiler_message(&read_fixture("compiler_message/mismatched-types.json"), true)
                .to_messages();
        assert_eq!(
            messages[0].0,
            "mismatched types\n\n\
             expected usize, found i32",
        );

        assert_eq!(
            messages[0].1,
            vec!["expected `usize` because of return type", "expected usize, found i32",]
        );
    }

    /// ```
    /// fn not_mut() {
    ///     let string = String::new();
    ///     let _s1 = &mut string;
    /// }
    /// ```
    #[test]
    fn message_not_mutable() {
        let messages = parse_compiler_message(&read_fixture("compiler_message/not-mut.json"), true)
            .to_messages();
        assert_eq!(
            messages[0].0,
            "cannot borrow immutable local variable `string` as mutable\n\n\
             cannot borrow mutably",
        );

        // NOTE: 'consider' message becomes a suggestion.
        assert_eq!(
            messages[0].1,
            vec!["consider changing this to `mut string`", "cannot borrow mutably",]
        );
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
        let messages =
            parse_compiler_message(&read_fixture("compiler_message/consider-borrowing.json"), true)
                .to_messages();
        assert_eq!(
            messages[0].0,
            r#"mismatched types

expected &str, found struct `std::string::String`

note: expected type `&str`
         found type `std::string::String`
help: consider borrowing here: `&string`"#,
        );

        assert_eq!(messages[0].1, vec!["expected &str, found struct `std::string::String`"]);
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
        let messages =
            parse_compiler_message(&read_fixture("compiler_message/move-out-of-borrow.json"), true)
                .to_messages();
        assert_eq!(
            messages[0].0,
            "cannot move out of borrowed content\n\ncannot move out of borrowed content"
        );

        assert_eq!(
            messages[0].1,
            vec![
                "cannot move out of borrowed content",
                "hint: to prevent move, use `ref string` or `ref mut string`",
            ]
        );
    }

    /// ```
    /// use std::{f64, u64, u8 as Foo};
    /// ```
    #[test]
    fn message_unused_use() {
        let messages =
            parse_compiler_message(&read_fixture("compiler_message/unused-use.json"), true)
                .to_messages();

        // Single compiler message with 3 primary spans should emit 3 separate
        // diagnostics.
        for msg in &messages {
            assert_eq!(
                msg.0,
                "unused imports: `f64`, `u64`, `u8 as Foo`\n\n\
                 note: #[warn(unused_imports)] on by default"
            );

            assert!(msg.1.is_empty(), "{:?}", msg.1);
        }
    }

    #[test]
    fn message_cannot_find_type() {
        let messages =
            parse_compiler_message(&read_fixture("compiler_message/cannot-find-type.json"), true)
                .to_messages();
        assert_eq!(
            messages[0].0,
            "cannot find type `HashSet` in this scope\n\n\
             not found in this scope"
        );

        assert_eq!(messages[0].1, vec!["not found in this scope"]);
    }

    /// ```
    /// let _s = 1 / 1;
    /// ```
    #[test]
    fn message_clippy_identity_op() {
        let diag =
            parse_compiler_message(&read_fixture("compiler_message/clippy-identity-op.json"), true);

        let diagnostic = &diag.diagnostics.values().nth(0).unwrap()[0];

        assert_eq!(diagnostic.0.source, Some("clippy".into()));

        let messages = diag.to_messages();
        println!("\n---message---\n{}\n---", messages[0].0);

        let link = {
            let link_index = messages[0]
                .0
                .find("https://rust-lang-nursery.github.io/rust-clippy/")
                .expect("no clippy link found in message");
            &messages[0].0[link_index..]
        };

        assert_eq!(
            messages[0].0,
            "the operation is ineffective. Consider reducing it to `1`\n\n\
             note: #[warn(identity_op)] implied by #[warn(clippy)]\n\
             help: for further information visit "
                .to_owned()
                + link
        );

        assert!(messages[0].1.is_empty(), "{:?}", messages[0].1);
    }

    #[test]
    fn macro_error_no_trait() {
        let diag = parse_compiler_message(
            &read_fixture("compiler_message/macro-error-no-trait.json"),
            true,
        );
        assert_eq!(diag.diagnostics.len(), 1, "{:#?}", diag.diagnostics);

        let file = &diag.diagnostics.keys().nth(0).unwrap();
        assert!(file.to_str().unwrap().ends_with("src/main.rs"), "Unexpected file {:?}", file);

        let diagnostic = &diag.diagnostics.values().nth(0).unwrap()[0];
        assert_eq!(diagnostic.0.source, Some("rustc".into()));
        assert_eq!(
            diagnostic.0.range,
            Range { start: Position::new(2, 4), end: Position::new(2, 27) }
        );

        let messages = diag.to_messages();
        assert_eq!(
            messages[0].0,
            "no method named `write_fmt` found for type `std::string::String` \
             in the current scope\n\n\
             help: items from traits can only be used if the trait is in scope"
        );

        assert!(messages[0].1.is_empty(), "{:?}", messages[0].1);
    }

    /// ```
    /// #[macro_use]
    /// extern crate log;
    /// fn main() {
    ///     info!("forgot comma {}" 123);
    /// }
    /// ```
    #[test]
    fn macro_expected_token_nested_expansion() {
        let diag = parse_compiler_message(
            &read_fixture("compiler_message/macro-expected-token.json"),
            true,
        );
        assert_eq!(diag.diagnostics.len(), 1, "{:#?}", diag.diagnostics);

        let file = &diag.diagnostics.keys().nth(0).unwrap();
        assert!(file.to_str().unwrap().ends_with("src/main.rs"), "Unexpected file {:?}", file);

        let diagnostic = &diag.diagnostics.values().nth(0).unwrap()[0];
        assert_eq!(diagnostic.0.source, Some("rustc".into()));
        assert_eq!(
            diagnostic.0.range,
            Range { start: Position::new(4, 4), end: Position::new(4, 33) }
        );

        let messages = diag.to_messages();
        assert_eq!(messages[0].0, "expected token: `,`");

        assert!(messages[0].1.is_empty(), "{:?}", messages[0].1);
    }
}

/// Tests for creating suggestions from the compilers JSON output.
#[cfg(test)]
mod diagnostic_suggestion_test {
    use self::diagnostic_message_test::*;
    use super::*;
    use lsp_types::Position;

    #[test]
    fn suggest_use_when_cannot_find_type() {
        let diag =
            parse_compiler_message(&read_fixture("compiler_message/cannot-find-type.json"), true);

        let diagnostics = diag.diagnostics.values().nth(0).unwrap();

        eprintln!("{:#?}", diagnostics);

        let use_hash_set = diagnostics
            .iter()
            .flat_map(|(_, suggestions)| suggestions)
            .find(|s| s.new_text == "use std::collections::HashSet;\n")
            .expect("`use std::collections::HashSet` not found");

        assert_eq!(use_hash_set.label, "Line 15: Add `use std::collections::HashSet;\n`");

        assert_eq!(
            use_hash_set.range,
            Range { start: Position::new(14, 0), end: Position::new(14, 0) }
        );
    }

    #[test]
    fn suggest_mut_when_not_mut() {
        let diag = parse_compiler_message(&read_fixture("compiler_message/not-mut.json"), true);

        let diagnostics = diag.diagnostics.values().nth(0).unwrap();

        eprintln!("{:#?}", diagnostics);

        let change_to_mut = diagnostics
            .iter()
            .flat_map(|(_, suggestions)| suggestions)
            .find(|s| s.new_text == "mut string")
            .expect("`mut string` not found");

        assert_eq!(change_to_mut.label, "Line 133: Change to `mut string`");

        assert_eq!(
            change_to_mut.range,
            Range { start: Position::new(132, 12), end: Position::new(132, 18) }
        );
    }

    /// ```
    /// pub const WINDOW_PROGRESS: &'static str = "window/progress";
    /// ```
    #[test]
    fn suggest_clippy_const_static() {
        let diag = parse_compiler_message(
            &read_fixture("compiler_message/clippy-const-static-lifetime.json"),
            true,
        );

        let diagnostics = diag.diagnostics.values().nth(0).unwrap();

        eprintln!("{:#?}", diagnostics);

        let change_to_mut = diagnostics
            .iter()
            .flat_map(|(_, suggestions)| suggestions)
            .find(|s| s.new_text == "&str")
            .expect("`&str` not found");

        assert_eq!(change_to_mut.label, "Line 355: Change to `&str`");

        assert_eq!(
            change_to_mut.range,
            Range { start: Position::new(354, 34), end: Position::new(354, 46) }
        );
    }

    #[test]
    fn suggest_macro_error_no_trait() {
        let diag = parse_compiler_message(
            &read_fixture("compiler_message/macro-error-no-trait.json"),
            true,
        );
        let diagnostics = diag.diagnostics.values().nth(0).unwrap();

        eprintln!("{:#?}", diagnostics);

        let change_to_mut = diagnostics
            .iter()
            .flat_map(|(_, suggestions)| suggestions)
            .find(|s| s.new_text == "use std::fmt::Write;\n\n")
            .expect("`use std::fmt::Write;` not found");

        assert_eq!(change_to_mut.label, "Line 1: Add `use std::fmt::Write;\n\n`");

        assert_eq!(
            change_to_mut.range,
            Range { start: Position::new(0, 0), end: Position::new(0, 0) }
        );
    }
}
