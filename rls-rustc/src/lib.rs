#![feature(rustc_private)]

extern crate env_logger;
extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_save_analysis;
extern crate rustc_session;
extern crate rustc_span;

#[cfg(feature = "ipc")]
use rustc_driver::Compilation;
use rustc_driver::{Callbacks, RunCompiler};
use rustc_interface::interface;
#[cfg(feature = "ipc")]
use rustc_interface::Queries;
use rustc_session::config::ErrorOutputType;
use rustc_session::early_error;

use std::env;
#[allow(unused_imports)]
use std::path::{Path, PathBuf};

#[cfg(feature = "clippy")]
mod clippy;
#[cfg(feature = "ipc")]
mod ipc;

pub fn run() -> Result<(), ()> {
    #[cfg(feature = "ipc")]
    let rt = tokio::runtime::Runtime::new().unwrap();
    #[cfg(feature = "clippy")]
    let clippy_preference = clippy::preference();

    #[cfg(feature = "ipc")]
    let (mut shim_calls, file_loader) = match std::env::var("RLS_IPC_ENDPOINT").ok() {
        Some(endpoint) => {
            let client: ipc::Client = rt
                .block_on(async { ipc::connect(endpoint).await })
                .expect("Couldn't connect to IPC endpoint");
            let (file_loader, callbacks) = client.split();

            (
                ShimCalls {
                    callbacks: Some(callbacks),
                    #[cfg(feature = "clippy")]
                    clippy_preference,
                },
                file_loader.into_boxed(),
            )
        }
        None => (ShimCalls::default(), None),
    };
    #[cfg(not(feature = "ipc"))]
    let (mut shim_calls, file_loader) = (ShimCalls::default(), None);

    let args = env::args_os()
        .enumerate()
        .map(|(i, arg)| {
            arg.into_string().unwrap_or_else(|arg| {
                early_error(
                    ErrorOutputType::default(),
                    &format!("Argument {} is not valid Unicode: {:?}", i, arg),
                )
            })
        })
        .collect::<Vec<_>>();

    #[cfg(feature = "clippy")]
    let args = match clippy_preference {
        Some(preference) => clippy::adjust_args(args, preference),
        None => args,
    };

    rustc_driver::install_ice_hook();
    rustc_driver::catch_fatal_errors(move || {
        let mut compiler = RunCompiler::new(&args, &mut shim_calls);
        compiler.set_file_loader(file_loader);
        compiler.run()
    })
    .map(|_| ())
    .map_err(|_| ())
}

#[derive(Default)]
struct ShimCalls {
    #[cfg(feature = "ipc")]
    callbacks: Option<ipc::IpcCallbacks>,
    #[cfg(feature = "clippy")]
    clippy_preference: Option<clippy::ClippyPreference>,
}

impl Callbacks for ShimCalls {
    fn config(&mut self, config: &mut interface::Config) {
        config.opts.debugging_opts.save_analysis = true;

        #[cfg(feature = "clippy")]
        match self.clippy_preference {
            Some(preference) if preference != clippy::ClippyPreference::Off => {
                clippy::config(config);
            }
            _ => {}
        }
    }

    #[cfg(feature = "ipc")]
    fn after_expansion<'tcx>(
        &mut self,
        compiler: &interface::Compiler,
        queries: &'tcx Queries<'tcx>,
    ) -> Compilation {
        use rustc_session::config::Input;

        use rls_ipc::rpc::{Crate, Edition};
        use std::collections::{HashMap, HashSet};

        let callbacks = match self.callbacks.as_ref() {
            Some(callbacks) => callbacks,
            None => return Compilation::Continue,
        };

        let sess = compiler.session();
        let input = compiler.input();

        let cwd = &sess.working_dir.local_path_if_available();

        let src_path = match input {
            Input::File(ref name) => Some(name.to_path_buf()),
            Input::Str { .. } => None,
        }
        .and_then(|path| src_path(Some(cwd), path));

        let krate = Crate {
            name: queries.crate_name().unwrap().peek().to_owned(),
            src_path,
            disambiguator: sess.local_crate_disambiguator().to_fingerprint().as_value(),
            edition: match sess.edition() {
                rustc_span::edition::Edition::Edition2015 => Edition::Edition2015,
                rustc_span::edition::Edition::Edition2018 => Edition::Edition2018,
                rustc_span::edition::Edition::Edition2021 => Edition::Edition2021,
            },
        };

        let mut input_files: HashMap<PathBuf, HashSet<Crate>> = HashMap::new();
        for file in fetch_input_files(sess) {
            input_files.entry(file).or_default().insert(krate.clone());
        }

        if let Err(e) = futures::executor::block_on(callbacks.input_files(input_files)) {
            log::error!("Can't send input files as part of a compilation callback: {:?}", e);
        }

        Compilation::Continue
    }

    #[cfg(feature = "ipc")]
    fn after_analysis<'tcx>(
        &mut self,
        compiler: &interface::Compiler,
        queries: &'tcx Queries<'tcx>,
    ) -> Compilation {
        let callbacks = match self.callbacks.as_ref() {
            Some(callbacks) => callbacks,
            None => return Compilation::Continue,
        };

        use rustc_save_analysis::CallbackHandler;

        let input = compiler.input();
        let crate_name = queries.crate_name().unwrap().peek().clone();

        queries.global_ctxt().unwrap().peek_mut().enter(|tcx| {
            // There are two ways to move the data from rustc to the RLS, either
            // directly or by serialising and deserialising. We only want to do
            // the latter when there are compatibility issues between crates.

            // This version passes via JSON, it is more easily backwards compatible.
            // save::process_crate(state.tcx.unwrap(),
            //                     state.analysis.unwrap(),
            //                     state.crate_name.unwrap(),
            //                     state.input,
            //                     None,
            //                     save::DumpHandler::new(state.out_dir,
            //                                            state.crate_name.unwrap()));
            // This version passes directly, it is more efficient.
            rustc_save_analysis::process_crate(
                tcx,
                &crate_name,
                &input,
                None,
                CallbackHandler {
                    callback: &mut |a| {
                        let analysis = unsafe { ::std::mem::transmute(a.clone()) };
                        if let Err(e) =
                            futures::executor::block_on(callbacks.complete_analysis(analysis))
                        {
                            log::error!(
                                "Can't send analysis as part of a compilation callback: {:?}",
                                e
                            );
                        }
                    },
                },
            );
        });

        Compilation::Continue
    }
}

#[cfg(feature = "ipc")]
fn fetch_input_files(sess: &rustc_session::Session) -> Vec<PathBuf> {
    let cwd = &sess.working_dir.local_path_if_available();

    sess.source_map()
        .files()
        .iter()
        .filter(|fmap| fmap.is_real_file())
        .filter(|fmap| !fmap.is_imported())
        .map(|fmap| fmap.name.prefer_local().to_string())
        .map(|fmap| src_path(Some(cwd), fmap).unwrap())
        .collect()
}

#[cfg(feature = "ipc")]
fn src_path(cwd: Option<&Path>, path: impl AsRef<Path>) -> Option<PathBuf> {
    let path = path.as_ref();

    Some(match (cwd, path.is_absolute()) {
        (_, true) => path.to_owned(),
        (Some(cwd), _) => cwd.join(path),
        (None, _) => std::env::current_dir().ok()?.join(path),
    })
}
