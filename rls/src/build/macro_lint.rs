
#[allow(unused_extern_crates)]
extern crate rustc_driver;
extern crate rustc_lint;
extern crate rustc_span;
extern crate rustc_interface;
extern crate rustc_session;
extern crate syntax;

// use rustc_driver::{Callbacks, Compilation};
use rustc_driver::Callbacks;
// use rustc_interface::{Config, interface::Compiler, Queries};
use rustc_interface::Config;
use rustc_lint::{
    EarlyContext,
    EarlyLintPass,
};
use rustc_span::hygiene::{SyntaxContext};
use rustc_span::Span;
use rustc_session::{declare_lint, impl_lint_pass};
use syntax::ast;

use std::sync::{Arc, Mutex};
use std::path::PathBuf;

use rls_data::{Analysis, Def, DefKind, SpanData, Id, Signature, Attribute};
use rls_span as span;

declare_lint! {
    pub MACRO_DOCS,
    Allow,
    "gathers documentation for macros",
    report_in_external_macro
}

#[derive(Debug)]
pub struct Comments {
    span: (u32, u32, SyntaxContext),
    text: String,
}

impl Comments {
    pub fn new(span: Span, text: String) -> Self {
        let data = span.data();
        Self {
            span: (data.lo.0, data.hi.0, data.ctxt),
            text,
        }
    }
}

#[derive(Debug, Default)]
pub struct MacroDoc {
    pub defs: Arc<Mutex<Vec<Def>>>,
}

impl MacroDoc {
    pub(crate) fn new(defs: Arc<Mutex<Vec<Def>>>) -> Self {
        Self { defs, }
    }
}

impl_lint_pass!(MacroDoc => [MACRO_DOCS]);

impl EarlyLintPass for MacroDoc {
    fn check_item(&mut self, ecx: &EarlyContext, it: &ast::Item) {
        if let ast::ItemKind::MacroDef(_) = &it.kind {
            println!("macro `{:#?}`", it);
            let mut width = 0;
            let docs = it.attrs
                .iter()
                .filter(|attr| attr.is_doc_comment())
                .flat_map(|attr| attr.doc_str())
                .map(|sym| {
                    let doc = sym.as_str().chars()
                        .filter(|c| c != &'/')
                        .collect::<String>();
                    if doc.len() > width {
                        width = doc.len();
                    }
                    doc
                })
                .collect::<Vec<_>>()
                .join("\n");
            
            println!("{}", std::iter::repeat('-').take(width).collect::<String>());
            println!("{}", docs);

            // let id = Id { krate: 0, index: 0, };
            // let name = it.ident.to_string();
            // let file_name = ecx.sess.local_crate_source_file.unwrap_or_default();
            // let span = SpanData {
            //     file_name,
            //     byte_start: it.span.lo().0,
            //     byte_end: it.span.hi().0,
            //     line_start: span::Row::new_one_indexed(0),
            //     line_end: span::Row::new_one_indexed(0),
            //     // Character offset.
            //     column_start: span::Column::new_one_indexed(0),
            //     column_end: span::Column::new_one_indexed(0),
            // };
            // self.defs.lock().unwrap().push(Def {
            //     kind: DefKind::Macro,
            //     id,
            //     span,
            //     name,
            //     qualname: format!("{}", file_name.to_str().unwrap()),
            //     value: name,
            //     parent: None,
            //     children: Vec::default(),
            //     decl_id: None,
            //     docs,
            //     sig: Some(Signature {
            //         text: format!("macro_rules! {}", name),
            //         defs: Vec::default(),
            //         refs: Vec::default(),
            //     }),
            //     attributes: vec![
            //         Attribute {}
            //     ],
            // })
        }
    }
}

// struct RegisterMacDocs;

// impl Callbacks for RegisterMacDocs {
//     fn config(&mut self, config: &mut Config) {
//         // this prevents the compiler from dropping the expanded AST
//         // although it still works without it?
//         config.opts.debugging_opts.save_analysis = true;
//         // no output files saved
//         config.opts.debugging_opts.no_analysis = true;

//         // config.opts.describe_lints = true;

        
//         let previous = config.register_lints.take();
//         config.register_lints = Some(Box::new(move |sess, lint_store| {
//             // technically we're ~guaranteed that this is none but might as well call anything that
//             // is there already. Certainly it can't hurt.
//             if let Some(previous) = &previous {
//                 (previous)(sess, lint_store);
//             }

//             lint_store.register_lints(&[&MACRO_DOCS]);
//             lint_store.register_early_pass(|| Box::new(MacroDoc));
//         }));
//     }
// }
