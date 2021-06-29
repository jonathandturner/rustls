use std::fs;
use std::path::Path;
use std::time::Duration;

use futures::future;
use lsp_types::{notification::*, request::*, *};
use serde::de::Deserialize;
use serde_json::json;

use crate::support::project_builder::{project, ProjectBuilder};
use crate::support::{basic_bin_manifest, fixtures_dir};

#[allow(dead_code)]
mod support;

fn initialize_params(root_path: &Path) -> InitializeParams {
    InitializeParams {
        process_id: None,
        root_uri: None,
        root_path: Some(root_path.display().to_string()),
        initialization_options: None,
        capabilities: ClientCapabilities {
            workspace: None,
            window: Some(WindowClientCapabilities { progress: Some(true) }),
            text_document: None,
            experimental: None,
        },
        trace: None,
        workspace_folders: None,
    }
}

fn initialize_params_with_opts(root_path: &Path, opts: serde_json::Value) -> InitializeParams {
    InitializeParams { initialization_options: Some(opts), ..initialize_params(root_path) }
}

#[test]
fn client_test_infer_bin() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("infer_bin")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag = rls.wait_for_diagnostics();

    assert!(diag.uri.as_str().ends_with("src/main.rs"));
    assert!(diag.diagnostics[0].message.contains("struct is never constructed: `UnusedBin`"));
}

#[test]
fn client_test_infer_lib() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("infer_lib")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag = rls.wait_for_diagnostics();

    assert!(diag.uri.as_str().ends_with("src/lib.rs"));
    assert!(diag.diagnostics[0].message.contains("struct is never constructed: `UnusedLib`"));
}

#[test]
fn client_test_infer_custom_bin() {
    let p =
        ProjectBuilder::try_from_fixture(fixtures_dir().join("infer_custom_bin")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag = rls.wait_for_diagnostics();

    assert!(diag.uri.as_str().ends_with("src/custom_bin.rs"));
    assert!(diag.diagnostics[0].message.contains("struct is never constructed: `UnusedCustomBin`"));
}

/// Test includes window/progress regression testing
#[test]
fn client_test_simple_workspace() {
    let p = project("simple_workspace")
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = [
                "member_lib",
                "member_bin",
                ]
            "#,
        )
        .file(
            "Cargo.lock",
            r#"
                [root]
                name = "member_lib"
                version = "0.1.0"

                [[package]]
                name = "member_bin"
                version = "0.1.0"
                dependencies = [
                "member_lib 0.1.0",
                ]
            "#,
        )
        .file(
            "member_bin/Cargo.toml",
            r#"
                [package]
                name = "member_bin"
                version = "0.1.0"
                authors = ["Igor Matuszewski <Xanewok@gmail.com>"]

                [dependencies]
                member_lib = { path = "../member_lib" }
            "#,
        )
        .file(
            "member_bin/src/main.rs",
            r#"
                extern crate member_lib;

                fn main() {
                    let a = member_lib::MemberLibStruct;
                }
            "#,
        )
        .file(
            "member_lib/Cargo.toml",
            r#"
                [package]
                name = "member_lib"
                version = "0.1.0"
                authors = ["Igor Matuszewski <Xanewok@gmail.com>"]

                [dependencies]
            "#,
        )
        .file(
            "member_lib/src/lib.rs",
            r#"
                pub struct MemberLibStruct;

                struct Unused;

                #[cfg(test)]
                mod tests {
                    #[test]
                    fn it_works() {
                    }
                }
            "#,
        )
        .build();

    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    // Check if we built member_lib and member_bin + their cfg(test) variants
    let count = rls
        .messages()
        .iter()
        .filter(|msg| msg["method"] == "window/progress")
        .filter(|msg| msg["params"]["title"] == "Building")
        .filter(|msg| {
            msg["params"]["message"].as_str().map(|x| x.starts_with("member_")).unwrap_or(false)
        })
        .count();
    assert_eq!(count, 4);
}

#[test]
fn client_changing_workspace_lib_retains_diagnostics() {
    let p = project("simple_workspace")
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = [
                "library",
                "binary",
                ]
            "#,
        )
        .file(
            "library/Cargo.toml",
            r#"
                [package]
                name = "library"
                version = "0.1.0"
                authors = ["Example <rls@example.com>"]
            "#,
        )
        .file(
            "library/src/lib.rs",
            r#"
                pub fn fetch_u32() -> u32 {
                    let unused = ();
                    42
                }
                #[cfg(test)]
                mod test {
                    #[test]
                    fn my_test() {
                        let test_val: u32 = super::fetch_u32();
                    }
                }
            "#,
        )
        .file(
            "binary/Cargo.toml",
            r#"
                [package]
                name = "binary"
                version = "0.1.0"
                authors = ["Igor Matuszewski <Xanewok@gmail.com>"]

                [dependencies]
                library = { path = "../library" }
            "#,
        )
        .file(
            "binary/src/main.rs",
            r#"
                extern crate library;

                fn main() {
                    let val: u32 = library::fetch_u32();
                }
            "#,
        )
        .build();

    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let lib = rls.future_diagnostics("library/src/lib.rs");
    let bin = rls.future_diagnostics("binary/src/main.rs");
    let (lib, bin) = rls.block_on(future::join(lib, bin)).unwrap();
    let (lib, bin) = (lib.unwrap(), bin.unwrap());

    assert!(lib.diagnostics.iter().any(|m| m.message.contains("unused variable: `test_val`")));
    assert!(lib.diagnostics.iter().any(|m| m.message.contains("unused variable: `unused`")));
    assert!(bin.diagnostics[0].message.contains("unused variable: `val`"));

    rls.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
        content_changes: vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position { line: 1, character: 38 },
                end: Position { line: 1, character: 41 },
            }),
            range_length: Some(3),
            text: "u64".to_string(),
        }],
        text_document: VersionedTextDocumentIdentifier {
            uri: Url::from_file_path(p.root().join("library/src/lib.rs")).unwrap(),
            version: Some(0),
        },
    });

    let lib = rls.future_diagnostics("library/src/lib.rs");
    let bin = rls.future_diagnostics("binary/src/main.rs");
    let (lib, bin) = rls.block_on(future::join(lib, bin)).unwrap();
    let (lib, bin) = (lib.unwrap(), bin.unwrap());

    // lib unit tests have compile errors
    assert!(lib.diagnostics.iter().any(|m| m.message.contains("unused variable: `unused`")));
    assert!(lib.diagnostics.iter().any(|m| m.message.contains("expected `u32`, found `u64`")));
    // bin depending on lib picks up type mismatch
    assert!(bin.diagnostics[0].message.contains("mismatched types\n\nexpected `u32`, found `u64`"));

    rls.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
        content_changes: vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position { line: 1, character: 38 },
                end: Position { line: 1, character: 41 },
            }),
            range_length: Some(3),
            text: "u32".to_string(),
        }],
        text_document: VersionedTextDocumentIdentifier {
            uri: Url::from_file_path(p.root().join("library/src/lib.rs")).unwrap(),
            version: Some(1),
        },
    });

    let lib = rls.future_diagnostics("library/src/lib.rs");
    let bin = rls.future_diagnostics("binary/src/main.rs");
    let (lib, bin) = rls.block_on(future::join(lib, bin)).unwrap();
    let (lib, bin) = (lib.unwrap(), bin.unwrap());

    assert!(lib.diagnostics.iter().any(|m| m.message.contains("unused variable: `test_val`")));
    assert!(lib.diagnostics.iter().any(|m| m.message.contains("unused variable: `unused`")));
    assert!(bin.diagnostics[0].message.contains("unused variable: `val`"));
}

#[test]
fn client_implicit_workspace_pick_up_lib_changes() {
    let p = project("simple_workspace")
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "binary"
                version = "0.1.0"
                authors = ["Example <rls@example.com>"]

                [dependencies]
                inner = { path = "inner" }
            "#,
        )
        .file(
            "src/main.rs",
            r#"
                extern crate inner;

                fn main() {
                    let val = inner::foo();
                }
            "#,
        )
        .file(
            "inner/Cargo.toml",
            r#"
                [package]
                name = "inner"
                version = "0.1.0"
                authors = ["Example <rls@example.com>"]
            "#,
        )
        .file(
            "inner/src/lib.rs",
            r#"
                pub fn foo() -> u32 { 42 }
            "#,
        )
        .build();

    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let bin = rls.future_diagnostics("src/main.rs");
    let bin = rls.block_on(bin).unwrap();
    let bin = bin.unwrap();
    assert!(bin.diagnostics[0].message.contains("unused variable: `val`"));

    rls.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
        content_changes: vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position { line: 1, character: 23 },
                end: Position { line: 1, character: 26 },
            }),
            range_length: Some(3),
            text: "bar".to_string(),
        }],
        text_document: VersionedTextDocumentIdentifier {
            uri: Url::from_file_path(p.root().join("inner/src/lib.rs")).unwrap(),
            version: Some(0),
        },
    });

    // bin depending on lib picks up type mismatch
    let bin = rls.future_diagnostics("src/main.rs");
    let bin = rls.block_on(bin).unwrap();
    let bin = bin.unwrap();
    assert!(bin.diagnostics[0].message.contains("cannot find function `foo`"));

    rls.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
        content_changes: vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position { line: 1, character: 23 },
                end: Position { line: 1, character: 26 },
            }),
            range_length: Some(3),
            text: "foo".to_string(),
        }],
        text_document: VersionedTextDocumentIdentifier {
            uri: Url::from_file_path(p.root().join("inner/src/lib.rs")).unwrap(),
            version: Some(1),
        },
    });

    let bin = rls.future_diagnostics("src/main.rs");
    let bin = rls.block_on(bin).unwrap();
    let bin = bin.unwrap();
    assert!(bin.diagnostics[0].message.contains("unused variable: `val`"));
}

#[test]
fn client_test_complete_self_crate_name() {
    let p = project("ws_with_test_dir")
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = ["library"]
            "#,
        )
        .file(
            "library/Cargo.toml",
            r#"
                [package]
                name = "library"
                version = "0.1.0"
                authors = ["Example <rls@example.com>"]
            "#,
        )
        .file(
            "library/src/lib.rs",
            r#"
                pub fn function() -> usize { 5 }
            "#,
        )
        .file(
            "library/tests/test.rs",
            r#"
                   extern crate library;
                   use library::~
            "#,
        )
        .build();

    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag = rls.wait_for_diagnostics();
    assert!(diag.diagnostics[0].message.contains("expected identifier"));

    let response = rls.request::<Completion>(
        100,
        CompletionParams {
            context: Some(CompletionContext {
                trigger_character: Some(":".to_string()),
                trigger_kind: CompletionTriggerKind::TriggerCharacter,
            }),
            text_document_position: TextDocumentPositionParams {
                position: Position::new(2, 32),
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("library/tests/test.rs")).unwrap(),
                },
            },
        },
    );

    let items = match response {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(CompletionList { items, .. })) => items,
        _ => Vec::new(),
    };

    let item = items.into_iter().nth(0).expect("Racer autocompletion failed");
    assert_eq!(item.detail.unwrap(), "pub fn function() -> usize");
}

// Spurious in Rust CI, e.g.
// https://github.com/rust-lang/rust/pull/60730
// https://github.com/rust-lang/rust/pull/61771
// https://github.com/rust-lang/rust/pull/61932
#[ignore]
#[test]
fn client_completion_suggests_arguments_in_statements() {
    let p = project("ws_with_test_dir")
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = ["library"]
            "#,
        )
        .file(
            "library/Cargo.toml",
            r#"
                [package]
                name = "library"
                version = "0.1.0"
                authors = ["Example <rls@example.com>"]
            "#,
        )
        .file(
            "library/src/lib.rs",
            r#"
                pub fn function() -> usize { 5 }
            "#,
        )
        .file(
            "library/tests/test.rs",
            r#"
                   extern crate library;
                   fn magic() {
                       let a = library::f~
                   }
            "#,
        )
        .build();

    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(
        0,
        lsp_types::InitializeParams {
            process_id: None,
            root_uri: None,
            root_path: Some(root_path.display().to_string()),
            initialization_options: None,
            capabilities: lsp_types::ClientCapabilities {
                workspace: None,
                window: Some(WindowClientCapabilities { progress: Some(true) }),
                text_document: Some(TextDocumentClientCapabilities {
                    completion: Some(CompletionCapability {
                        completion_item: Some(CompletionItemCapability {
                            snippet_support: Some(true),
                            ..CompletionItemCapability::default()
                        }),
                        ..CompletionCapability::default()
                    }),
                    ..TextDocumentClientCapabilities::default()
                }),
                experimental: None,
            },
            trace: None,
            workspace_folders: None,
        },
    );

    let diag = rls.wait_for_diagnostics();
    assert!(diag.diagnostics[0].message.contains("expected one of"));

    let response = rls.request::<Completion>(
        100,
        CompletionParams {
            context: Some(CompletionContext {
                trigger_character: Some("f".to_string()),
                trigger_kind: CompletionTriggerKind::TriggerCharacter,
            }),
            text_document_position: TextDocumentPositionParams {
                position: Position::new(3, 41),
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("library/tests/test.rs")).unwrap(),
                },
            },
        },
    );

    let items = match response {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(CompletionList { items, .. })) => items,
        _ => Vec::new(),
    };

    let item = items.into_iter().nth(0).expect("Racer autocompletion failed");
    assert_eq!(item.insert_text.unwrap(), "function()");
}

#[test]
fn client_use_statement_completion_doesnt_suggest_arguments() {
    let p = project("ws_with_test_dir")
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = ["library"]
            "#,
        )
        .file(
            "library/Cargo.toml",
            r#"
                [package]
                name = "library"
                version = "0.1.0"
                authors = ["Example <rls@example.com>"]
            "#,
        )
        .file(
            "library/src/lib.rs",
            r#"
                pub fn function() -> usize { 5 }
            "#,
        )
        .file(
            "library/tests/test.rs",
            r#"
                   extern crate library;
                   use library::~;
            "#,
        )
        .build();

    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag = rls.wait_for_diagnostics();
    assert!(diag.diagnostics[0].message.contains("expected identifier"));

    let response = rls.request::<Completion>(
        100,
        CompletionParams {
            context: Some(CompletionContext {
                trigger_character: Some(":".to_string()),
                trigger_kind: CompletionTriggerKind::TriggerCharacter,
            }),
            text_document_position: TextDocumentPositionParams {
                position: Position::new(2, 32),
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("library/tests/test.rs")).unwrap(),
                },
            },
        },
    );

    let items = match response {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(CompletionList { items, .. })) => items,
        _ => Vec::new(),
    };

    let item = items.into_iter().nth(0).expect("Racer autocompletion failed");
    assert_eq!(item.insert_text.unwrap(), "function");
}

/// Test simulates typing in a dependency wrongly in a couple of ways before finally getting it
/// right. Rls should provide Cargo.toml diagnostics.
///
/// ```
/// [dependencies]
/// auto-cfg = "0.5555"
/// ```
///
/// * Firstly "auto-cfg" doesn't exist, it should be "autocfg"
/// * Secondly version 0.5555 of "autocfg" doesn't exist.
#[test]
fn client_dependency_typo_and_fix() {
    let manifest_with_dependency = |dep: &str| {
        format!(
            r#"
            [package]
            name = "dependency_typo"
            version = "0.1.0"
            authors = ["alexheretic@gmail.com"]

            [dependencies]
            {}
        "#,
            dep
        )
    };

    let p = project("dependency_typo")
        .file("Cargo.toml", &manifest_with_dependency(r#"auto-cfg = "0.5555""#))
        .file(
            "src/main.rs",
            r#"
                fn main() {
                    println!("Hello world!");
                }
            "#,
        )
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag = rls.wait_for_diagnostics();
    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Error));
    assert!(diag.diagnostics[0].message.contains("no matching package named `auto-cfg`"));

    let change_manifest = |contents: &str| {
        std::fs::write(root_path.join("Cargo.toml"), contents).unwrap();
    };

    // fix naming typo, we now expect a version error diagnostic
    change_manifest(&manifest_with_dependency(r#"autocfg = "0.5555""#));
    rls.notify::<DidChangeWatchedFiles>(DidChangeWatchedFilesParams {
        changes: vec![FileEvent {
            uri: Url::from_file_path(p.root().join("Cargo.toml")).unwrap(),
            typ: FileChangeType::Changed,
        }],
    });

    let diag = rls.wait_for_diagnostics();
    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Error));
    assert!(diag.diagnostics[0].message.contains("^0.5555"));

    // Fix version issue so no error diagnostics occur.
    // This is kinda slow as cargo will compile the dependency, though I
    // chose autocfg to minimise this as it is a very small dependency.
    change_manifest(&manifest_with_dependency(r#"autocfg = "1""#));
    rls.notify::<DidChangeWatchedFiles>(DidChangeWatchedFilesParams {
        changes: vec![FileEvent {
            uri: Url::from_file_path(p.root().join("Cargo.toml")).unwrap(),
            typ: FileChangeType::Changed,
        }],
    });

    let diag = rls.wait_for_diagnostics();
    assert_eq!(
        diag.diagnostics.iter().find(|d| d.severity == Some(DiagnosticSeverity::Error)),
        None
    );
}

/// Tests correct positioning of a toml parse error, use of `==` instead of `=`.
#[test]
fn client_invalid_toml_manifest() {
    let p = project("invalid_toml")
        .file(
            "Cargo.toml",
            r#"[package]
            name = "probably_valid"
            version == "0.1.0"
            authors = ["alexheretic@gmail.com"]
            "#,
        )
        .file(
            "src/main.rs",
            r#"
                fn main() {
                    println!("Hello world!");
                }
            "#,
        )
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag: PublishDiagnosticsParams = rls.wait_for_diagnostics();

    assert!(diag.uri.as_str().ends_with("invalid_toml/Cargo.toml"));
    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Error));
    assert!(diag.diagnostics[0].message.contains("failed to parse manifest"));

    assert_eq!(
        diag.diagnostics[0].range,
        Range {
            start: Position { line: 2, character: 21 },
            end: Position { line: 2, character: 22 },
        }
    );
}

/// Tests correct file highlighting of workspace member manifest with invalid path dependency.
#[test]
fn client_invalid_member_toml_manifest() {
    let project = project("invalid_member_toml")
        .file(
            "Cargo.toml",
            r#"[package]
            name = "root_is_fine"
            version = "0.1.0"
            authors = ["alexheretic@gmail.com"]

            [dependencies]
            member_a = { path = "member_a" }

            [workspace]
            "#,
        )
        .file("src/main.rs", "fn main() {}")
        .file(
            "member_a/Cargo.toml",
            r#"[package]
            name = "member_a"
            version = "0.0.3"
            authors = ["alexheretic@gmail.com"]

            [dependencies]
            dodgy_member = { path = "dodgy_member" }
            "#,
        )
        .file("member_a/src/lib.rs", "fn ma() {}")
        .file(
            "member_a/dodgy_member/Cargo.toml",
            r#"[package]
            name = "dodgy_member"
            version = "0.5.0"
            authors = ["alexheretic@gmail.com"]

            [dependencies]
            nosuch = { path = "not-exist" }
            "#,
        )
        .file("member_a/dodgy_member/src/lib.rs", "fn dm() {}")
        .build();
    let root_path = project.root();
    let mut rls = project.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag: PublishDiagnosticsParams = rls.wait_for_diagnostics();

    assert!(diag.uri.as_str().ends_with("invalid_member_toml/member_a/dodgy_member/Cargo.toml"));

    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Error));
    assert!(diag.diagnostics[0].message.contains("failed to load manifest"));
}

#[test]
fn client_invalid_member_dependency_resolution() {
    let project = project("invalid_member_resolution")
        .file(
            "Cargo.toml",
            r#"[package]
            name = "root_is_fine"
            version = "0.1.0"
            authors = ["alexheretic@gmail.com"]

            [dependencies]
            member_a = { path = "member_a" }

            [workspace]
            "#,
        )
        .file("src/main.rs", "fn main() {}")
        .file(
            "member_a/Cargo.toml",
            r#"[package]
            name = "member_a"
            version = "0.0.5"
            authors = ["alexheretic@gmail.com"]

            [dependencies]
            dodgy_member = { path = "dodgy_member" }
            "#,
        )
        .file("member_a/src/lib.rs", "fn ma() {}")
        .file(
            "member_a/dodgy_member/Cargo.toml",
            r#"[package]
            name = "dodgy_member"
            version = "0.6.0"
            authors = ["alexheretic@gmail.com"]

            [dependencies]
            nosuchdep123 = "1.2.4"
            "#,
        )
        .file("member_a/dodgy_member/src/lib.rs", "fn dm() {}")
        .build();
    let root_path = project.root();
    let mut rls = project.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag: PublishDiagnosticsParams = rls.wait_for_diagnostics();

    assert!(diag
        .uri
        .as_str()
        .ends_with("invalid_member_resolution/member_a/dodgy_member/Cargo.toml"));

    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Error));
    assert!(diag.diagnostics[0].message.contains("no matching package named `nosuchdep123`"));
}

#[test]
fn client_handle_utf16_unit_text_edits() {
    let p = project("client_handle_utf16_unit_text_edits")
        .file(
            "Cargo.toml",
            r#"[package]
            name = "client_handle_utf16_unit_text_edits"
            version = "0.1.0"
            authors = ["example@example.com"]
            "#,
        )
        .file("src/main.rs", "fn main() {}")
        .file("src/some.rs", "😢")
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    rls.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier {
            uri: Url::from_file_path(p.root().join("src/some.rs")).unwrap(),
            version: Some(0),
        },
        // "😢" -> ""
        content_changes: vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 2 },
            }),
            range_length: Some(2),
            text: "".to_string(),
        }],
    });
}

/// Ensures that wide characters do not prevent RLS from calculating correct
/// 'whole file' LSP range.
#[test]
fn client_format_utf16_range() {
    let p = project("client_format_utf16_range")
        .file(
            "Cargo.toml",
            r#"[package]
            name = "client_format_utf16_range"
            version = "0.1.0"
            authors = ["example@example.com"]
            "#,
        )
        .file("src/main.rs", "/* 😢😢😢😢😢😢😢 */ fn main() { }")
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    let result = rls.request::<Formatting>(
        66,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier {
                uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
            },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                properties: Default::default(),
            },
        },
    );

    let new_text: Vec<_> =
        result.unwrap().iter().map(|edit| edit.new_text.as_str().replace('\r', "")).collect();
    // Actual formatting isn't important - what is, is that the buffer isn't
    // malformed and code stays semantically equivalent.
    assert_eq!(new_text, vec!["/* 😢😢😢😢😢😢😢 */\nfn main() {}\n"]);
}

#[test]
fn client_lens_run() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("lens_run")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(
        0,
        lsp_types::InitializeParams {
            process_id: None,
            root_uri: None,
            root_path: Some(root_path.display().to_string()),
            initialization_options: Some(json!({ "cmdRun": true})),
            capabilities: Default::default(),
            trace: None,
            workspace_folders: None,
        },
    );

    rls.wait_for_indexing();
    assert!(rls.messages().iter().count() >= 7);

    let lens = rls.request::<CodeLensRequest>(
        1,
        CodeLensParams {
            text_document: TextDocumentIdentifier {
                uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
            },
        },
    );

    let expected = CodeLens {
        command: Some(Command {
            command: "rls.run".to_string(),
            title: "Run test".to_string(),
            arguments: Some(vec![json!({
                "args": [ "test", "--", "--nocapture", "test_foo" ],
                "binary": "cargo",
                "env": { "RUST_BACKTRACE": "short" }
            })]),
        }),
        data: None,
        range: Range {
            start: Position { line: 4, character: 3 },
            end: Position { line: 4, character: 11 },
        },
    };

    assert_eq!(lens, Some(vec![expected]));
}

#[test]
#[ignore] // Spurious in Rust CI, https://github.com/rust-lang/rust/issues/62225
fn client_find_definitions() {
    const SRC: &str = r#"
        struct Foo {
        }

        impl Foo {
            fn new() {
            }
        }

        fn main() {
            Foo::new();
        }
    "#;

    let p = project("simple_workspace")
        .file("Cargo.toml", &basic_bin_manifest("bar"))
        .file("src/main.rs", SRC)
        .build();

    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    // FIXME: Without `all_targets=false`, this test will randomly fail.
    let opts = json!({"settings": {"rust": {"racer_completion": false, "all_targets": false } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let mut results = vec![];
    for (line_index, line) in SRC.lines().enumerate() {
        for i in 0..line.len() {
            let id = (line_index * 100 + i) as u64;
            let result = rls.request::<GotoDefinition>(
                id,
                TextDocumentPositionParams {
                    position: Position { line: line_index as u64, character: i as u64 },
                    text_document: TextDocumentIdentifier {
                        uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                    },
                },
            );

            let ranges: Vec<_> = result
                .into_iter()
                .flat_map(|x| match x {
                    GotoDefinitionResponse::Scalar(loc) => vec![loc].into_iter(),
                    GotoDefinitionResponse::Array(locs) => locs.into_iter(),
                    _ => unreachable!(),
                })
                .map(|x| x.range)
                .collect();

            if !ranges.is_empty() {
                results.push((line_index, i, ranges));
            }
        }
    }

    // Foo
    let foo_definition = Range {
        start: Position { line: 1, character: 15 },
        end: Position { line: 1, character: 18 },
    };

    // Foo::new
    let foo_new_definition = Range {
        start: Position { line: 5, character: 15 },
        end: Position { line: 5, character: 18 },
    };

    // main
    let main_definition = Range {
        start: Position { line: 9, character: 11 },
        end: Position { line: 9, character: 15 },
    };

    let expected = [
        // struct Foo
        (1, 15, vec![foo_definition]),
        (1, 16, vec![foo_definition]),
        (1, 17, vec![foo_definition]),
        (1, 18, vec![foo_definition]),
        // impl Foo
        (4, 13, vec![foo_definition]),
        (4, 14, vec![foo_definition]),
        (4, 15, vec![foo_definition]),
        (4, 16, vec![foo_definition]),
        // fn new
        (5, 15, vec![foo_new_definition]),
        (5, 16, vec![foo_new_definition]),
        (5, 17, vec![foo_new_definition]),
        (5, 18, vec![foo_new_definition]),
        // fn main
        (9, 11, vec![main_definition]),
        (9, 12, vec![main_definition]),
        (9, 13, vec![main_definition]),
        (9, 14, vec![main_definition]),
        (9, 15, vec![main_definition]),
        // Foo::new()
        (10, 12, vec![foo_definition]),
        (10, 13, vec![foo_definition]),
        (10, 14, vec![foo_definition]),
        (10, 15, vec![foo_definition]),
        (10, 17, vec![foo_new_definition]),
        (10, 18, vec![foo_new_definition]),
        (10, 19, vec![foo_new_definition]),
        (10, 20, vec![foo_new_definition]),
    ];

    if results.len() != expected.len() {
        panic!(
            "Got different amount of completions than expected: {} vs. {}: {:#?}",
            results.len(),
            expected.len(),
            results
        )
    }

    for (i, (actual, expected)) in results.iter().zip(expected.iter()).enumerate() {
        if actual != expected {
            panic!(
                "Found different definition at index {}. Got {:#?}, expected {:#?}",
                i, actual, expected
            )
        }
    }
}

#[test]
#[ignore] // Spurious in Rust CI, https://github.com/rust-lang/rust/pull/62805
fn client_deglob() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("deglob")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    // Test a single swglob
    let commands = rls
        .request::<CodeActionRequest>(
            100,
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                },
                range: Range { start: Position::new(2, 0), end: Position::new(2, 0) },
                context: CodeActionContext { diagnostics: vec![], only: None },
            },
        )
        .expect("No code actions returned for line 2");

    // Right now we only support deglobbing via commands. Please update this
    // test if we move to making text edits via CodeAction (which we should for
    // deglobbing);
    let Command { title, command, arguments, .. } = match commands.into_iter().nth(0).unwrap() {
        CodeActionOrCommand::Command(commands) => commands,
        CodeActionOrCommand::CodeAction(_) => unimplemented!(),
    };

    let arguments = arguments.expect("Missing command arguments");

    assert_eq!(title, "Deglob import".to_string());
    assert!(command.starts_with("rls.deglobImports-"));

    assert!(arguments[0]["new_text"].as_str() == Some("{Stdin, Stdout}"));
    assert_eq!(
        serde_json::from_value::<Location>(arguments[0]["location"].clone()).unwrap(),
        Location {
            range: Range { start: Position::new(2, 13), end: Position::new(2, 14) },
            uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
        }
    );

    rls.request::<ExecuteCommand>(200, ExecuteCommandParams { command, arguments });
    // Right now the execute command returns an empty response and sends
    // appropriate apply edit request via a side-channel
    let result = rls
        .messages()
        .iter()
        .rfind(|msg| msg["method"] == ApplyWorkspaceEdit::METHOD)
        .unwrap()
        .clone();
    let params = <ApplyWorkspaceEdit as Request>::Params::deserialize(&result["params"])
        .expect("Couldn't deserialize params");

    let (url, edits) = params.edit.changes.unwrap().drain().nth(0).unwrap();
    assert_eq!(url, Url::from_file_path(p.root().join("src/main.rs")).unwrap());
    assert_eq!(
        edits,
        vec![TextEdit {
            range: Range { start: Position::new(2, 13), end: Position::new(2, 14) },
            new_text: "{Stdin, Stdout}".to_string(),
        }]
    );

    // Test a deglob for double wildcard
    let commands = rls
        .request::<CodeActionRequest>(
            1100,
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                },
                range: Range { start: Position::new(5, 0), end: Position::new(5, 0) },
                context: CodeActionContext { diagnostics: vec![], only: None },
            },
        )
        .expect("No code actions returned for line 12");

    // Right now we only support deglobbing via commands. Please update this
    // test if we move to making text edits via CodeAction (which we should for
    // deglobbing);
    let Command { title, command, arguments, .. } = match commands.into_iter().nth(0).unwrap() {
        CodeActionOrCommand::Command(commands) => commands,
        CodeActionOrCommand::CodeAction(_) => unimplemented!(),
    };

    let arguments = arguments.expect("Missing command arguments");

    assert_eq!(title, "Deglob imports".to_string());
    assert!(command.starts_with("rls.deglobImports-"));
    let expected = [(14, 15, "size_of"), (31, 32, "max")];
    for i in 0..2 {
        assert!(arguments[i]["new_text"].as_str() == Some(expected[i].2));
        assert_eq!(
            serde_json::from_value::<Location>(arguments[i]["location"].clone()).unwrap(),
            Location {
                range: Range {
                    start: Position::new(5, expected[i].0),
                    end: Position::new(5, expected[i].1),
                },
                uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
            }
        );
    }

    rls.request::<ExecuteCommand>(1200, ExecuteCommandParams { command, arguments });
    // Right now the execute command returns an empty response and sends
    // appropriate apply edit request via a side-channel
    let result = rls
        .messages()
        .iter()
        .rfind(|msg| msg["method"] == ApplyWorkspaceEdit::METHOD)
        .unwrap()
        .clone();
    let params = <ApplyWorkspaceEdit as Request>::Params::deserialize(&result["params"])
        .expect("Couldn't deserialize params");

    let (url, edits) = params.edit.changes.unwrap().drain().nth(0).unwrap();
    assert_eq!(url, Url::from_file_path(p.root().join("src/main.rs")).unwrap());
    assert_eq!(
        edits,
        expected
            .iter()
            .map(|e| TextEdit {
                range: Range { start: Position::new(5, e.0), end: Position::new(5, e.1) },
                new_text: e.2.to_string()
            })
            .collect::<Vec<_>>()
    );
}

fn is_notification_for_unknown_config(msg: &serde_json::Value) -> bool {
    msg["method"] == ShowMessage::METHOD
        && msg["params"]["message"].as_str().unwrap().contains("Unknown")
}

fn is_notification_for_deprecated_config(msg: &serde_json::Value) -> bool {
    msg["method"] == ShowMessage::METHOD
        && msg["params"]["message"].as_str().unwrap().contains("is deprecated")
}

fn is_notification_for_duplicated_config(msg: &serde_json::Value) -> bool {
    msg["method"] == ShowMessage::METHOD
        && msg["params"]["message"].as_str().unwrap().contains("Duplicate")
}

#[test]
fn client_init_duplicated_and_unknown_settings() {
    let p = project("simple_workspace")
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file(
            "src/main.rs",
            r#"
                struct UnusedBin;
                fn main() {
                    println!("Hello world!");
                }
            "#,
        )
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({
        "settings": {
            "rust": {
                "features": ["some_feature"],
                "all_targets": false,
                "unknown1": 1,
                "unknown2": false,
                "dup_val": 1,
                "dup_val": false,
                "dup_licated": "dup_lacated",
                "DupLicated": "DupLicated",
                "dup-licated": "dup-licated",
                // Deprecated
                "use_crate_blacklist": true
            }
        }
    });

    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    assert!(rls.messages().iter().any(is_notification_for_unknown_config));
    assert!(
        rls.messages().iter().any(is_notification_for_deprecated_config),
        "`use_crate_blacklist` should be marked as deprecated"
    );
    assert!(rls.messages().iter().any(is_notification_for_duplicated_config));
}

#[test]
fn client_did_change_configuration_duplicated_and_unknown_settings() {
    let p = project("simple_workspace")
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file(
            "src/main.rs",
            r#"
                struct UnusedBin;
                fn main() {
                    println!("Hello world!");
                }
            "#,
        )
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    assert!(!rls.messages().iter().any(is_notification_for_unknown_config));
    assert!(!rls.messages().iter().any(is_notification_for_duplicated_config));
    let settings = json!({
        "rust": {
            "features": ["some_feature"],
            "all_targets": false,
            "unknown1": 1,
            "unknown2": false,
            "dup_val": 1,
            "dup_val": false,
            "dup_licated": "dup_lacated",
            "DupLicated": "DupLicated",
            "dup-licated": "dup-licated"
        }
    });
    rls.notify::<DidChangeConfiguration>(DidChangeConfigurationParams { settings });

    rls.wait_for_message(is_notification_for_unknown_config);
    if !rls.messages().iter().any(is_notification_for_duplicated_config) {
        rls.wait_for_message(is_notification_for_duplicated_config);
    }
}

#[test]
fn client_shutdown() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));
}

#[test]
fn client_goto_def() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    let result = rls.request::<GotoDefinition>(
        11,
        TextDocumentPositionParams {
            position: Position { line: 12, character: 27 },
            text_document: TextDocumentIdentifier {
                uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
            },
        },
    );

    let ranges: Vec<_> = result
        .into_iter()
        .flat_map(|x| match x {
            GotoDefinitionResponse::Scalar(loc) => vec![loc].into_iter(),
            GotoDefinitionResponse::Array(locs) => locs.into_iter(),
            _ => unreachable!(),
        })
        .map(|x| x.range)
        .collect();

    assert!(ranges.iter().any(|r| r.start == Position { line: 11, character: 8 }));
}

#[test]
fn client_hover() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    // FIXME: Without `all_targets=false`, this test will randomly fail.
    let opts = json!({"settings": {"rust": { "all_targets": false } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let result = rls
        .request::<HoverRequest>(
            11,
            TextDocumentPositionParams {
                position: Position { line: 12, character: 27 },
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                },
            },
        )
        .unwrap();

    let contents = ["&str", "let world = \"world\";"];
    let mut contents: Vec<_> = contents.iter().map(ToString::to_string).collect();
    let contents =
        contents.drain(..).map(|value| LanguageString { language: "rust".to_string(), value });
    let contents = contents.map(MarkedString::LanguageString).collect();

    assert_eq!(result.contents, HoverContents::Array(contents));
}

/// Test hover continues to work after the source has moved line
#[ignore] // FIXME(#1265): Spurious failure - sometimes we lose the semantic information from Rust - why?
#[test]
fn client_hover_after_src_line_change() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": {"racer_completion": false } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let world_src_pos = Position { line: 12, character: 27 };
    let world_src_pos_after = Position { line: 13, character: 27 };

    let result = rls
        .request::<HoverRequest>(
            11,
            TextDocumentPositionParams {
                position: world_src_pos,
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                },
            },
        )
        .unwrap();

    let contents = ["&str", "let world = \"world\";"];
    let contents: Vec<_> = contents
        .iter()
        .map(|value| LanguageString { language: "rust".to_string(), value: (*value).to_string() })
        .map(MarkedString::LanguageString)
        .collect();

    assert_eq!(result.contents, HoverContents::Array(contents.clone()));

    rls.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
        content_changes: vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position { line: 10, character: 15 },
                end: Position { line: 10, character: 15 },
            }),
            range_length: Some(0),
            text: "\n    ".to_string(),
        }],
        text_document: VersionedTextDocumentIdentifier {
            uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
            version: Some(2),
        },
    });

    rls.wait_for_indexing();

    let result = rls
        .request::<HoverRequest>(
            11,
            TextDocumentPositionParams {
                position: world_src_pos_after,
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                },
            },
        )
        .unwrap();

    assert_eq!(result.contents, HoverContents::Array(contents));
}

#[test]
fn client_workspace_symbol() {
    let p =
        ProjectBuilder::try_from_fixture(fixtures_dir().join("workspace_symbol")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": { "cfg_test": true } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let symbols = rls
        .request::<WorkspaceSymbol>(42, WorkspaceSymbolParams { query: "nemo".to_owned() })
        .unwrap();

    let mut nemos = vec![
        ("src/main.rs", "nemo", SymbolKind::Function, 1, 11, 1, 15, Some("x")),
        ("src/foo.rs", "nemo", SymbolKind::Module, 0, 4, 0, 8, Some("foo")),
    ];

    for (file, name, kind, start_l, start_c, end_l, end_c, container_name) in nemos.drain(..) {
        let sym = SymbolInformation {
            name: name.to_string(),
            kind,
            container_name: container_name.map(ToString::to_string),
            location: Location {
                uri: Url::from_file_path(p.root().join(file)).unwrap(),
                range: Range {
                    start: Position { line: start_l, character: start_c },
                    end: Position { line: end_l, character: end_c },
                },
            },
            deprecated: None,
        };
        dbg!(&sym);
        assert!(symbols.iter().any(|s| *s == sym));
    }
}

#[test]
fn client_workspace_symbol_duplicates() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("workspace_symbol_duplicates"))
        .unwrap()
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": { "cfg_test": true } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let symbols = rls
        .request::<WorkspaceSymbol>(42, WorkspaceSymbolParams { query: "Frobnicator".to_owned() })
        .unwrap();

    let symbol = SymbolInformation {
        name: "Frobnicator".to_string(),
        kind: SymbolKind::Struct,
        container_name: Some("a".to_string()),
        location: Location {
            uri: Url::from_file_path(p.root().join("src/shared.rs")).unwrap(),
            range: Range {
                start: Position { line: 1, character: 7 },
                end: Position { line: 1, character: 18 },
            },
        },
        deprecated: None,
    };

    assert_eq!(symbols, vec![symbol]);
}

#[ignore] // FIXME(#1265): This is spurious (we don't pick up reference under #[cfg(test)])-ed code - why?
#[test]
fn client_find_all_refs_test() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": {"all_targets": true } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let result = rls
        .request::<References>(
            42,
            ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                    },
                    position: Position { line: 0, character: 7 },
                },
                context: ReferenceContext { include_declaration: true },
            },
        )
        .unwrap();

    let ranges = [((0, 7), (0, 10)), ((6, 14), (6, 17)), ((14, 15), (14, 18))];
    for ((sl, sc), (el, ec)) in &ranges {
        let range = Range {
            start: Position { line: *sl, character: *sc },
            end: Position { line: *el, character: *ec },
        };

        dbg!(range);
        assert!(result.iter().any(|x| x.range == range));
    }
}

#[test]
fn client_find_all_refs_no_cfg_test() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("find_all_refs_no_cfg_test"))
        .unwrap()
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": { "all_targets": false } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let result = rls
        .request::<References>(
            42,
            ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                    },
                    position: Position { line: 0, character: 7 },
                },
                context: ReferenceContext { include_declaration: true },
            },
        )
        .unwrap();

    let ranges = [((0, 7), (0, 10)), ((13, 15), (13, 18))];
    for ((sl, sc), (el, ec)) in &ranges {
        let range = Range {
            start: Position { line: *sl, character: *sc },
            end: Position { line: *el, character: *ec },
        };

        dbg!(range);
        assert!(result.iter().any(|x| x.range == range));
    }
}

#[test]
fn client_borrow_error() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("borrow_error")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag = rls.wait_for_diagnostics();

    let msg = "cannot borrow `x` as mutable more than once at a time";
    assert!(diag.diagnostics.iter().any(|diag| diag.message.contains(msg)));
}

#[test]
fn client_highlight() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    // FIXME: Without `all_targets=false`, this test will randomly fail.
    let opts = json!({"settings": {"rust": { "all_targets": false } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let result = rls
        .request::<DocumentHighlightRequest>(
            42,
            TextDocumentPositionParams {
                position: Position { line: 12, character: 27 },
                text_document: TextDocumentIdentifier {
                    uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                },
            },
        )
        .unwrap();

    let ranges = [((11, 8), (11, 13)), ((12, 27), (12, 32))];
    for ((sl, sc), (el, ec)) in &ranges {
        let range = Range {
            start: Position { line: *sl, character: *sc },
            end: Position { line: *el, character: *ec },
        };

        dbg!(range);
        assert!(result.iter().any(|x| x.range == range));
    }
}

#[test]
fn client_rename() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    // FIXME: Without `all_targets=false`, this test will randomly fail.
    let opts = json!({"settings": {"rust": { "all_targets": false } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    let result = rls
        .request::<Rename>(
            42,
            RenameParams {
                text_document_position: TextDocumentPositionParams {
                    position: Position { line: 12, character: 27 },
                    text_document: TextDocumentIdentifier {
                        uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
                    },
                },
                new_name: "foo".to_owned(),
            },
        )
        .unwrap();

    dbg!(&result);

    let uri = Url::from_file_path(p.root().join("src/main.rs")).unwrap();
    let ranges = [((11, 8), (11, 13)), ((12, 27), (12, 32))];
    let ranges = ranges
        .iter()
        .map(|((sl, sc), (el, ec))| Range {
            start: Position { line: *sl, character: *sc },
            end: Position { line: *el, character: *ec },
        })
        .map(|range| TextEdit { range, new_text: "foo".to_string() });

    let changes = std::iter::once((uri, ranges.collect())).collect();

    assert_eq!(result.changes, Some(changes));
}

#[test]
fn client_reformat() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("reformat")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    let result = rls.request::<Formatting>(
        42,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier {
                uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
            },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                properties: Default::default(),
            },
        },
    );

    assert_eq!(result.unwrap()[0], TextEdit {
        range: Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: 2, character: 0 },
        },
        new_text: "pub mod foo;\npub fn main() {\n    let world = \"world\";\n    println!(\"Hello, {}!\", world);\n}\n".to_string(),
    });
}

#[test]
fn client_reformat_with_range() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("reformat_with_range"))
        .unwrap()
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    let result = rls.request::<RangeFormatting>(
        42,
        DocumentRangeFormattingParams {
            text_document: TextDocumentIdentifier {
                uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
            },
            range: Range {
                start: Position { line: 1, character: 0 },
                end: Position { line: 2, character: 0 },
            },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                properties: Default::default(),
            },
        },
    );

    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    let formatted = r#"pub fn main() {
    let world1 = "world";
    println!("Hello, {}!", world1);
"#
    .replace("\r", "")
    .replace("\n", newline);

    let edits = result.unwrap();
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0].new_text, formatted);
    assert_eq!(edits[1].new_text, newline);
}

#[test]
fn client_multiple_binaries() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("multiple_bins")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": { "build_bin": "bin2" } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    {
        let msgs = rls.messages();
        let diags = msgs
            .iter()
            .filter(|x| x["method"] == PublishDiagnostics::METHOD)
            .flat_map(|msg| msg["params"]["diagnostics"].as_array().unwrap())
            .map(|diag| diag["message"].as_str().unwrap())
            .collect::<Vec<&str>>();

        for i in 1..3 {
            let msg = &format!("unused variable: `bin_name{}`", i);
            assert!(diags.iter().any(|message| message.starts_with(msg)));
        }
    }
}

#[ignore] // Requires `rust-src` component, which isn't available in Rust CI.
#[test]
fn client_completion() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    let text_document =
        TextDocumentIdentifier { uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap() };

    let completions = |x: CompletionResponse| match x {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(CompletionList { items, .. }) => items,
    };

    macro_rules! item_eq {
        ($item:expr, $expected:expr) => {{
            let (label, kind, detail) = $expected;
            ($item.label == *label && $item.kind == *kind && $item.detail == *detail)
        }};
    }

    let expected = [
        // FIXME(https://github.com/rust-lang/rls/issues/1205) - empty "     " string
        ("world", &Some(CompletionItemKind::Variable), &Some("let world = \"     \";".to_string())),
        ("x", &Some(CompletionItemKind::Field), &Some("x: u64".to_string())),
    ];

    let result = rls.request::<Completion>(
        11,
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: text_document.clone(),
                position: Position { line: 12, character: 30 },
            },
            context: None,
        },
    );
    let items = completions(result.unwrap());
    assert!(items.iter().any(|item| item_eq!(item, expected[0])));
    let result = rls.request::<Completion>(
        11,
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document,
                position: Position { line: 15, character: 30 },
            },
            context: None,
        },
    );
    let items = completions(result.unwrap());
    assert!(items.iter().any(|item| item_eq!(item, expected[1])));
}

#[test]
fn client_bin_lib_project() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("bin_lib")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": { "cfg_test": true, "build_bin": "bin_lib" } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    let diag: PublishDiagnosticsParams = rls.wait_for_diagnostics();

    assert!(diag.uri.as_str().ends_with("bin_lib/tests/tests.rs"));
    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Warning));
    assert!(diag.diagnostics[0].message.contains("unused variable: `unused_var`"));
}

#[test]
fn client_infer_lib() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("infer_lib")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    let diag = rls.wait_for_diagnostics();

    assert!(diag.uri.as_str().ends_with("src/lib.rs"));
    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Warning));
    assert!(diag.diagnostics[0].message.contains("struct is never constructed: `UnusedLib`"));
}

#[test]
fn client_omit_init_build() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    const ID: u64 = 1337;
    let response = rls.future_msg(|msg| msg["id"] == json!(ID));

    let opts = json!({ "omitInitBuild": true });
    rls.request::<Initialize>(ID, initialize_params_with_opts(root_path, opts));

    // We need to assert that no other messages are received after a short
    // period of time (e.g. no build progress messages).
    std::thread::sleep(std::time::Duration::from_secs(1));
    rls.block_on(response).unwrap().unwrap();

    assert_eq!(rls.messages().iter().count(), 1);
}

#[test]
fn client_find_impls() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("find_impls")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();

    let uri = Url::from_file_path(p.root().join("src/main.rs")).unwrap();

    let locations = |result: Option<GotoDefinitionResponse>| match result.unwrap() {
        GotoDefinitionResponse::Scalar(loc) => vec![loc],
        GotoDefinitionResponse::Array(locations) => locations,
        GotoDefinitionResponse::Link(mut links) => {
            links.drain(..).map(|l| Location { uri: l.target_uri, range: l.target_range }).collect()
        }
    };

    let result = rls.request::<GotoImplementation>(
        1,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri.clone()),
            position: Position { line: 3, character: 7 }, // "Bar"
        },
    );
    let expected = [(9, 15, 9, 18), (10, 12, 10, 15)];
    let expected = expected.iter().map(|(a, b, c, d)| Location {
        uri: uri.clone(),
        range: Range {
            start: Position { line: *a, character: *b },
            end: Position { line: *c, character: *d },
        },
    });
    let locs = locations(result);
    for exp in expected {
        assert!(locs.iter().any(|x| *x == exp));
    }

    let result = rls.request::<GotoImplementation>(
        1,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri.clone()),
            position: Position { line: 6, character: 6 }, // "Super"
        },
    );
    let expected = [(9, 15, 9, 18), (13, 15, 13, 18)];
    let expected = expected.iter().map(|(a, b, c, d)| Location {
        uri: uri.clone(),
        range: Range {
            start: Position { line: *a, character: *b },
            end: Position { line: *c, character: *d },
        },
    });
    let locs = locations(result);
    for exp in expected {
        assert!(locs.iter().any(|x| *x == exp));
    }
}

#[test]
fn client_features() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("features")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": {"features": ["bar", "baz"] } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    let diag = rls.wait_for_diagnostics();

    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Error));
    let msg = "cannot find struct, variant or union type `Foo` in this scope";
    assert!(diag.diagnostics[0].message.contains(msg));
}

#[test]
fn client_all_features() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("features")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": {"all_features": true } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    rls.wait_for_indexing();

    assert_eq!(
        rls.messages().iter().filter(|x| x["method"] == PublishDiagnostics::METHOD).count(),
        0
    );
}

#[test]
fn client_no_default_features() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("features")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust":
        { "no_default_features": true, "features": ["foo", "bar"] } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    let diag = rls.wait_for_diagnostics();

    let diagnostics: Vec<_> =
        diag.diagnostics.iter().filter(|d| d.severity == Some(DiagnosticSeverity::Error)).collect();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::Error));
    let msg = "cannot find struct, variant or union type `Baz` in this scope";
    assert!(diagnostics[0].message.contains(msg));
}

#[test]
fn client_all_targets() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("bin_lib")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({"settings": {"rust": { "cfg_test": true, "all_targets": true } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    let diag: PublishDiagnosticsParams = rls.wait_for_diagnostics();

    assert!(diag.uri.as_str().ends_with("bin_lib/tests/tests.rs"));
    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Warning));
    assert!(diag.diagnostics[0].message.contains("unused variable: `unused_var`"));
}

/// Handle receiving a notification before the `initialize` request by ignoring and
/// continuing to run
#[test]
fn client_ignore_uninitialized_notification() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    rls.notify::<DidChangeConfiguration>(DidChangeConfigurationParams { settings: json!({}) });
    rls.request::<Initialize>(0, initialize_params(root_path));

    rls.wait_for_indexing();
}

/// Handle receiving requests before the `initialize` request by returning an error response
/// and continuing to run
#[test]
fn client_fail_uninitialized_request() {
    let p = ProjectBuilder::try_from_fixture(fixtures_dir().join("common")).unwrap().build();
    let mut rls = p.spawn_rls_async();

    const ID: u64 = 1337;

    rls.request::<GotoDefinition>(
        ID,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::from_file_path(p.root().join("src/main.rs")).unwrap(),
            },
            position: Position { line: 0, character: 0 },
        },
    );

    rls.block_on(async { tokio::time::sleep(Duration::from_secs(1)).await }).unwrap();

    let err = jsonrpc_core::Failure::deserialize(rls.messages().last().unwrap()).unwrap();
    assert_eq!(err.id, jsonrpc_core::Id::Num(ID));
    assert_eq!(err.error.code, jsonrpc_core::ErrorCode::ServerError(-32002));
    assert_eq!(err.error.message, "not yet received `initialize` request");
}

// Test that RLS can accept configuration with config keys in 4 different cases:
// - mixedCase
// - CamelCase
// - snake_case
// - kebab-case
fn client_init_impl(convert_case: fn(&str) -> String) {
    let p = project("config_cases")
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file(
            "src/main.rs",
            r#"
                #![allow(dead_code)]
                struct NonCfg;
                #[cfg(test)]
                struct CfgTest { inner: PathBuf }
                fn main() {}
            "#,
        )
        .build();

    let root_path = p.root();
    let mut rls = p.spawn_rls_async();

    let opts = json!({ "settings": { "rust": { convert_case("all_targets"): true } } });
    rls.request::<Initialize>(0, initialize_params_with_opts(root_path, opts));

    let diag = rls.wait_for_diagnostics();

    assert_eq!(diag.diagnostics.len(), 1);
    assert_eq!(diag.diagnostics[0].severity, Some(DiagnosticSeverity::Error));
    let msg = "cannot find type `PathBuf` in this scope";
    assert!(diag.diagnostics[0].message.contains(msg));
}

#[test]
fn client_init_with_configuration_mixed_case() {
    client_init_impl(heck::MixedCase::to_mixed_case);
}

#[test]
fn client_init_with_configuration_camel_case() {
    client_init_impl(heck::CamelCase::to_camel_case);
}

#[test]
fn client_init_with_configuration_snake_case() {
    client_init_impl(heck::SnakeCase::to_snake_case);
}

#[test]
fn client_init_with_configuration_kebab_case() {
    client_init_impl(heck::KebabCase::to_kebab_case);
}

#[test]
fn client_parse_error_on_malformed_input() {
    use crate::support::rls_exe;
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(rls_exe())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    cmd.stdin.take().unwrap().write_all(b"Malformed input").unwrap();
    let mut output = vec![];
    cmd.stdout.take().unwrap().read_to_end(&mut output).unwrap();
    let output = String::from_utf8(output).unwrap();

    assert_eq!(output, "Content-Length: 75\r\n\r\n{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32700,\"message\":\"Parse error\"},\"id\":null}");

    // Right now parse errors shutdown the RLS, which we might want to revisit
    // to provide better fault tolerance.
    cmd.wait().unwrap();
}

#[test]
fn client_cargo_target_directory_is_excluded_from_backups() {
    // This is to make sure that if it's rls that crates target/ directory the directory is
    // excluded from backups just as if it was created by cargo itself. See a comment in
    // run_cargo_ws() or rust-lang/cargo@cf3bfc9/rust-lang/cargo#8378 for more information.
    let p = project("backup_exclusion_workspace")
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file(
            "src/main.rs",
            r#"
                fn main() {
                    println!("Hello world!");
                }
            "#,
        )
        .build();
    let root_path = p.root();
    let mut rls = p.spawn_rls_async();
    rls.request::<Initialize>(0, initialize_params(root_path));
    let _ = rls.wait_for_indexing();
    let cachedir_tag = p.root().join("target").join("CACHEDIR.TAG");
    assert!(cachedir_tag.is_file());
    assert!(fs::read_to_string(&cachedir_tag)
        .unwrap()
        .starts_with("Signature: 8a477f597d28d172789f06886806bc55"));
}
