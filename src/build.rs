// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate getopts;
extern crate rustc;
extern crate rustc_driver;
extern crate rustc_errors as errors;
extern crate rustc_resolve;
extern crate rustc_save_analysis;
extern crate syntax;

use cargo::core::{PackageId, Shell, Workspace, Verbosity};
use cargo::ops::{compile_with_exec, Executor, Context, CompileOptions, CompileMode, CompileFilter, Unit};
use cargo::util::{Config as CargoConfig, ProcessBuilder, homedir, ConfigValue};
use cargo::util::{CargoResult};

use data::Analysis;
use vfs::Vfs;
use self::rustc::session::Session;
use self::rustc::session::config::{self, Input, ErrorOutputType};
use self::rustc_driver::{RustcDefaultCalls, run_compiler, run, Compilation, CompilerCalls};
use self::rustc_driver::driver::CompileController;
use self::rustc_save_analysis as save;
use self::rustc_save_analysis::CallbackHandler;
use self::syntax::ast;
use self::syntax::codemap::{FileLoader, RealFileLoader};

use config::Config;

use std::collections::{HashMap, BTreeMap};
use std::env;
use std::ffi::OsString;
use std::fs::{read_dir, remove_file};
use std::io::{self, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::thread;
use std::time::Duration;


/// Manages builds.
///
/// The IDE will request builds quickly (possibly on every keystroke), there is
/// no point running every one. We also avoid running more than one build at once.
/// We cannot cancel builds. It might be worth running builds in parallel or
/// cancelling a started build.
///
/// `BuildPriority::Immediate` builds are started straightaway. Normal builds are
/// started after a timeout. A new build request cancels any pending build requests.
///
/// From the client's point of view, a build request is not guaranteed to cause
/// a build. However, a build is guaranteed to happen and that build will begin
/// after the build request is received (no guarantee on how long after), and
/// that build is guaranteed to have finished before the build reqest returns.
///
/// There is no way for the client to specify that an individual request will
/// result in a build. However, you can tell from the result - if a build
/// was run, the build result will contain any errors or warnings and an indication
/// of success or failure. If the build was not run, the result indicates that
/// it was squashed.
pub struct BuildQueue {
    build_dir: Mutex<Option<PathBuf>>,
    // Arguments and environment with which we call rustc.
    // This can be further expanded for multi-crate target configuration.
    compilation_cx: Arc<Mutex<CompilationContext>>,
    // True if a build is running.
    // Note I have been conservative with Ordering when accessing this atomic,
    // we might be able to do better.
    running: AtomicBool,
    // A vec of channels to pending build threads.
    pending: Mutex<Vec<Sender<Signal>>>,
    vfs: Arc<Vfs>,
    config: Arc<Mutex<Config>>,
}

#[derive(Debug)]
pub enum BuildResult {
    // Build was succesful, argument is warnings.
    Success(Vec<String>, Option<Analysis>),
    // Build finished with errors, argument is errors and warnings.
    Failure(Vec<String>, Option<Analysis>),
    // Build was coelesced with another build.
    Squashed,
    // There was an error attempting to build.
    Err,
}

/// Priority for a build request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildPriority {
    /// Run this build as soon as possible (e.g., on save or explicit build request).
    Immediate,
    /// A regular build request (e.g., on a minor edit).
    Normal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Signal {
    Build,
    Skip,
}

#[derive(Debug)]
struct CompilationContext {
    args: Vec<String>,
    envs: HashMap<String, Option<OsString>>,
}

impl CompilationContext {
    fn new() -> CompilationContext {
        CompilationContext {
            args: vec![],
            envs: HashMap::new(),
        }
    }
}

impl BuildQueue {
    pub fn new(vfs: Arc<Vfs>, config: Arc<Mutex<Config>>) -> BuildQueue {
        BuildQueue {
            build_dir: Mutex::new(None),
            compilation_cx: Arc::new(Mutex::new(CompilationContext::new())),
            running: AtomicBool::new(false),
            pending: Mutex::new(vec![]),
            vfs,
            config,
        }
    }

    pub fn request_build(&self, build_dir: &Path, priority: BuildPriority, force_clean: bool) -> BuildResult {
        trace!("request_build, {:?} {:?}", build_dir, priority);

        // If there is a change in the project directory (or we've been requested to start from scratch),
        // then we can forget any pending build and start straight with this new build.
        {
            let mut prev_build_dir = self.build_dir.lock().unwrap();

            if force_clean || prev_build_dir.as_ref().map_or(true, |dir| dir != build_dir) {
                *prev_build_dir = Some(build_dir.to_owned());
                self.cancel_pending();

                let mut compilation_cx = self.compilation_cx.lock().unwrap();
                (*compilation_cx).args = vec![];
            }
        }

        self.cancel_pending();

        let (tx, rx) = channel();
        self.pending.lock().unwrap().push(tx);
        if priority == BuildPriority::Normal {
            let wait_to_build = { // Release lock before we sleep
                let config = self.config.lock().unwrap();
                config.wait_to_build
            };
            
            thread::sleep(Duration::from_millis(wait_to_build));
        }

        if self.running.load(Ordering::SeqCst) {
            // Blocks
            // println!("blocked on build");
            let signal = rx.recv().unwrap_or(Signal::Build);
            if signal == Signal::Skip {
                return BuildResult::Squashed;
            }
        // Quickly check our channel to see if anyone has told us to skip. This
        // is only likely to have happened if we slept, and some other build ran
        // while we slept.
        } else if rx.try_recv().unwrap_or(Signal::Build) == Signal::Skip {
            return BuildResult::Squashed;
        }

        // If another build has started already, we don't need to build
        // ourselves (it must have arrived after this request; so we don't add
        // to the pending list). But we do need to wait for that build to
        // finish.
        if self.running.swap(true, Ordering::SeqCst) {
            let mut wait = 100;
            while self.running.load(Ordering::SeqCst) && wait < 50000 {
                // println!("loop of death");
                thread::sleep(Duration::from_millis(wait));
                wait *= 2;
            }
            return BuildResult::Squashed;
        }

        let result = self.build();
        self.running.store(false, Ordering::SeqCst);

        // If there is a pending build, run it now.
        let mut pending = self.pending.lock().unwrap();
        let pending = mem::replace(&mut *pending, vec![]);
        if !pending.is_empty() {
            // Kick off one build, then skip the rest.
            let mut pending = pending.iter();
            while let Some(next) = pending.next() {
                if next.send(Signal::Build).is_ok() {
                    break;
                }
            }
            for t in pending {
                let _ = t.send(Signal::Skip);
            }
        }

        result
    }

    // Cancels all pending builds without running any of them.
    fn cancel_pending(&self) {
        let mut pending = self.pending.lock().unwrap();
        let pending = mem::replace(&mut *pending, vec![]);
        for t in pending {
            let _ = t.send(Signal::Skip);
        }
    }

    // Build the project.
    fn build(&self) -> BuildResult {
        trace!("running build");
        // When we change build directory (presumably because the IDE is
        // changing project), we must do a cargo build of the whole project.
        // Otherwise we just use rustc directly.
        //
        // The 'full cargo build' is a `cargo check` customised and run
        // in-process. Cargo will shell out to call rustc (this means the
        // the compiler available at runtime must match the compiler linked to
        // the RLS). All but the last crate are built as normal, we intercept
        // the call to the last crate and do our own rustc build. We cache the
        // command line args and environment so we can avoid running Cargo in
        // the future.
        //
        // Our 'short' rustc build runs rustc directly and in-process (we must
        // do this so we can load changed code from the VFS, rather than from
        // disk). We get the data we need by building with `-Zsave-analysis`.

        let needs_to_run_cargo = self.compilation_cx.lock().unwrap().args.is_empty();

        let build_dir = &self.build_dir.lock().unwrap();
        let build_dir = build_dir.as_ref().unwrap();

        if needs_to_run_cargo {
            if let BuildResult::Err = self.cargo(build_dir.clone()) {
                return BuildResult::Err;
            }
        }

        let compile_cx = self.compilation_cx.lock().unwrap();
        let args = &(*compile_cx).args;
        let envs = &(*compile_cx).envs;
        assert!(!args.is_empty());
        self.rustc(args, envs, build_dir)
    }

    // Runs an in-process instance of Cargo.
    fn cargo(&self, build_dir: PathBuf) -> BuildResult {
        struct RlsExecutor {
            compilation_cx: Arc<Mutex<CompilationContext>>,
            cur_package_id: Mutex<Option<PackageId>>,
            config: Arc<Mutex<Config>>,
        }

        impl RlsExecutor {
            fn new(compilation_cx: Arc<Mutex<CompilationContext>>,
                   config: Arc<Mutex<Config>>) -> RlsExecutor {
                RlsExecutor {
                    compilation_cx: compilation_cx,
                    cur_package_id: Mutex::new(None),
                    config,
                }
            }

            fn is_primary_crate(&self, id: &PackageId) -> bool {
                let cur_package_id = self.cur_package_id.lock().unwrap();
                id == cur_package_id.as_ref().expect("Executor has not been initialised")
            }
        }

        impl Executor for RlsExecutor {
            fn init(&self, cx: &Context) {
                let mut cur_package_id = self.cur_package_id.lock().unwrap();
                *cur_package_id = Some(cx.ws
                                         .current_opt()
                                         .expect("No current package in Cargo")
                                         .package_id()
                                         .clone());
            }

            fn force_rebuild(&self, unit: &Unit) -> bool {
                // We only do a cargo build if we want to force rebuild the last
                // crate (e.g., because some args changed). Therefore we should
                // always force rebuild the primary crate.
                let id = unit.pkg.package_id();
                // FIXME build scripts - this will force rebuild build scripts as
                // well as the primary crate. But this is not too bad - it means
                // we will rarely rebuild more than we have to.
                self.is_primary_crate(id)
            }

            fn exec(&self, cargo_cmd: ProcessBuilder, id: &PackageId) -> CargoResult<()> {
                // Delete any stale data. We try and remove any json files with
                // the same crate name as Cargo would emit. This includes files
                // with the same crate name but different hashes, e.g., those
                // made with a different compiler.
                let cargo_args = cargo_cmd.get_args();
                let crate_name = parse_arg(cargo_args, "--crate-name").expect("no crate-name in rustc command line");
                let out_dir = parse_arg(cargo_args, "--out-dir").expect("no out-dir in rustc command line");
                let analysis_dir = Path::new(&out_dir).join("save-analysis");
                if let Ok(dir_contents) = read_dir(&analysis_dir) {
                    for entry in dir_contents {
                        let entry = entry.expect("unexpected error reading save-analysis directory");
                        let name = entry.file_name();
                        let name = name.to_str().unwrap();
                        if name.starts_with(&crate_name) && name.ends_with(".json") {
                            debug!("removing: `{:?}`", name);
                            remove_file(entry.path()).expect("could not remove file");
                        }
                    }
                }

                // We only want to intercept rustc call targeting current crate to cache
                // args/envs generated by cargo so we can run only rustc later ourselves
                // Currently we don't cache nor modify build script args
                let is_build_script = crate_name == "build_script_build";
                if !self.is_primary_crate(id) || is_build_script {
                    let build_script_notice = if is_build_script {" (build script)"} else {""};
                    trace!("rustc not intercepted - {}{}", id.name(), build_script_notice);

                    return cargo_cmd.exec();
                }

                trace!("rustc intercepted - args: {:?} envs: {:?}", cargo_cmd.get_args(), cargo_cmd.get_envs());

                let rustc_exe = env::var("RUSTC").unwrap_or("rustc".to_owned());
                let mut cmd = Command::new(&rustc_exe);
                let mut args: Vec<_> =
                    cargo_cmd.get_args().iter().map(|a| a.clone().into_string().unwrap()).collect();

                {
                    let config = self.config.lock().unwrap();
                    let crate_type = parse_arg(cargo_args, "--crate-type");
                    // Becase we only try to emulate `cargo test` using `cargo check`, so for now
                    // assume crate_type arg (i.e. in `cargo test` it isn't specified for --test targets)
                    // and build test harness only for final crate type
                    let crate_type = crate_type.expect("no crate-type in rustc command line");
                    let is_final_crate_type = crate_type == "bin" || (crate_type == "lib" && config.build_lib);

                    if config.cfg_test {
                        // FIXME(#351) allow passing --test to lib crate-type when building a dependency
                        if is_final_crate_type {
                            args.push("--test".to_owned());
                        } else {
                            args.push("--cfg".to_owned());
                            args.push("test".to_owned());
                        }
                    }
                    if config.sysroot.is_none() {
                        let sysroot = current_sysroot()
                                        .expect("need to specify SYSROOT env var or use rustup or multirust");
                        args.push("--sysroot".to_owned());
                        args.push(sysroot);
                    }

                    // We can't omit compilation here, because Cargo is going to expect to get
                    // dep-info for this crate, so we shell out to rustc to get that.
                    // This is not really ideal, because we are going to
                    // compute this info anyway when we run rustc ourselves, but we don't do
                    // that before we return to Cargo.
                    // FIXME Don't do this. Start our build here rather than on another thread
                    // so the dep-info is ready by the time we return from this callback.
                    // Since ProcessBuilder doesn't allow to modify args, we need to create
                    // our own command here from scratch here.
                    for a in &args {
                        // Emitting only dep-info is possible only for final crate type, as
                        // as others may emit required metadata for dependent crate types
                        if a.starts_with("--emit") && is_final_crate_type {
                            cmd.arg("--emit=dep-info");
                        } else {
                            cmd.arg(a);
                        }
                    }
                    cmd.envs(cargo_cmd.get_envs().iter().filter_map(|(k, v)| v.as_ref().map(|v| (k, v))));
                    if let Some(cwd) = cargo_cmd.get_cwd() {
                        cmd.current_dir(cwd);
                    }
                }

                cmd.status().expect("Couldn't execute rustc");

                // Finally store the modified cargo-generated args/envs for future rustc calls
                args.insert(0, rustc_exe);
                *self.compilation_cx.lock().unwrap() = CompilationContext {
                    args: args,
                    envs: cargo_cmd.get_envs().clone()
                };

                Ok(())
            }
        }

        trace!("cargo - `{:?}`", build_dir);
        let exec = RlsExecutor::new(self.compilation_cx.clone(), self.config.clone());

        let out = Arc::new(Mutex::new(vec![]));
        let out_clone = out.clone();
        let rls_config = self.config.clone();

        // Cargo may or may not spawn threads to run the various builds, since
        // we may be in separate threads we need to block and wait our thread.
        // However, if Cargo doesn't run a separate thread, then we'll just wait
        // forever. Therefore, we spawn an extra thread here to be safe.
        let handle = thread::spawn(move || {
            let mut flags = "-Zunstable-options -Zsave-analysis --error-format=json \
                             -Zcontinue-parse-after-error".to_owned();

            let mut shell = Shell::from_write(Box::new(BufWriter(out.clone())));
            shell.set_verbosity(Verbosity::Quiet);
            let config = make_cargo_config(&build_dir, shell);
            let mut manifest_path = build_dir.clone();
            manifest_path.push("Cargo.toml");
            trace!("manifest_path: {:?}", manifest_path);
            // TODO: Add support for virtual manifests and multiple packages 
            let ws = Workspace::new(&manifest_path, &config).expect("could not create cargo workspace");
            let current_package = ws.current().unwrap(); 
            let targets = current_package.targets();
            let bins;
            let target_string;

            let opts = {
                let rls_config = rls_config.lock().unwrap();
                if let Some(ref sysroot) = rls_config.sysroot {
                    flags.push_str(&format!(" --sysroot {}", sysroot));
                }
                let rustflags = format!("{} {} {}",
                                         env::var("RUSTFLAGS").unwrap_or(String::new()),
                                         rls_config.rustflags.as_ref().unwrap_or(&String::new()),
                                         flags);
                let rustflags = dedup_flags(&rustflags);
                env::set_var("RUSTFLAGS", &rustflags);

                bins = {
                    if let Some(ref build_bin) = rls_config.build_bin { 
                        let mut bins = targets.iter().filter(|x| x.is_bin()); 
                        let bin = bins.find(|x| x.name() == build_bin);
                        match bin {
                            Some(bin) => vec![bin.name().to_owned()],
                            None => {
                                debug!("cargo - couldn't find binary `{}` (specified in rls toml file)", build_bin);
                                vec![]
                            }
                        }
                    } else {
                        vec![]
                    }
                };

                let mut opts = CompileOptions::default(&config, CompileMode::Check);
                if rls_config.build_lib {
                    opts.filter = CompileFilter::new(true, &[], false, &[], false, &[], false, &[], false); 
                } else if !bins.is_empty() {
                    opts.filter = CompileFilter::new(false, &bins, false, &[], false, &[], false, &[], false);
                }
                if let Some(ref target) = rls_config.target {
                    target_string = target.clone();
                    opts.target = Some(&target_string);
                }
                opts
            };
            compile_with_exec(&ws, &opts, Arc::new(exec)).expect("could not run cargo");
        });

        match handle.join() {
            Ok(_) => BuildResult::Success(vec![], None),
            Err(_) => {
                info!("cargo stdout {}", String::from_utf8(out_clone.lock().unwrap().to_owned()).unwrap());
                BuildResult::Err
            }
        }
    }

    // Runs a single instance of rustc. Runs in-process.
    fn rustc(&self, args: &[String], envs: &HashMap<String, Option<OsString>>, build_dir: &Path) -> BuildResult {
        trace!("rustc - args: `{:?}`, envs: {:?}, build dir: {:?}", args, envs, build_dir);

        let changed = self.vfs.get_cached_files();

        let _restore_env = Environment::push(envs);
        let buf = Arc::new(Mutex::new(vec![]));
        let err_buf = buf.clone();
        let args = args.to_owned();

        let analysis = Arc::new(Mutex::new(None));

        let mut controller = RlsRustcCalls::new(analysis.clone());

        let exit_code = ::std::panic::catch_unwind(|| {
            run(move || {
                // Replace stderr so we catch most errors.
                run_compiler(&args,
                             &mut controller,
                             Some(Box::new(ReplacedFileLoader::new(changed))),
                             Some(Box::new(BufWriter(buf))))
            })
        });

        // FIXME(#25) given that we are running the compiler directly, there is no need
        // to serialise the error messages - we should pass them in memory.
        let stderr_json_msg = convert_message_to_json_strings(Arc::try_unwrap(err_buf)
            .unwrap()
            .into_inner()
            .unwrap());

        let analysis = analysis.lock().unwrap().clone();
        return match exit_code {
            Ok(0) => BuildResult::Success(stderr_json_msg, analysis),
            _ => BuildResult::Failure(stderr_json_msg, analysis),
        };

        // Our compiler controller. We mostly delegate to the default rustc
        // controller, but use our own callback for save-analysis.
        #[derive(Clone)]
        struct RlsRustcCalls {
            default_calls: RustcDefaultCalls,
            analysis: Arc<Mutex<Option<Analysis>>>,
        }

        impl RlsRustcCalls {
            fn new(analysis: Arc<Mutex<Option<Analysis>>>) -> RlsRustcCalls {
                RlsRustcCalls {
                    default_calls: RustcDefaultCalls,
                    analysis: analysis,
                }
            }
        }

        impl<'a> CompilerCalls<'a> for RlsRustcCalls {
            fn early_callback(&mut self,
                              matches: &getopts::Matches,
                              sopts: &config::Options,
                              cfg: &ast::CrateConfig,
                              descriptions: &errors::registry::Registry,
                              output: ErrorOutputType)
                              -> Compilation {
                self.default_calls.early_callback(matches, sopts, cfg, descriptions, output)
            }

            fn no_input(&mut self,
                        matches: &getopts::Matches,
                        sopts: &config::Options,
                        cfg: &ast::CrateConfig,
                        odir: &Option<PathBuf>,
                        ofile: &Option<PathBuf>,
                        descriptions: &errors::registry::Registry)
                        -> Option<(Input, Option<PathBuf>)> {
                self.default_calls.no_input(matches, sopts, cfg, odir, ofile, descriptions)
            }

            fn late_callback(&mut self,
                             matches: &getopts::Matches,
                             sess: &Session,
                             input: &Input,
                             odir: &Option<PathBuf>,
                             ofile: &Option<PathBuf>)
                             -> Compilation {
                self.default_calls.late_callback(matches, sess, input, odir, ofile)
            }

            fn build_controller(&mut self,
                                sess: &Session,
                                matches: &getopts::Matches)
                                -> CompileController<'a> {
                let mut result = self.default_calls.build_controller(sess, matches);
                let analysis = self.analysis.clone();

                result.after_analysis.callback = Box::new(move |state| {
                    // There are two ways to move the data from rustc to the RLS, either
                    // directly or by serialising and deserialising. We only want to do 
                    // the latter when there are compatibility issues between crates.

                    // This version passes via JSON, it is more easily backwards compatible.
                    // save::process_crate(state.tcx.unwrap(),
                    //                     state.expanded_crate.unwrap(),
                    //                     state.analysis.unwrap(),
                    //                     state.crate_name.unwrap(),
                    //                     save::DumpHandler::new(save::Format::Json,
                    //                                            state.out_dir,
                    //                                            state.crate_name.unwrap()));
                    // This version passes directly, it is more efficient.
                    save::process_crate(state.tcx.unwrap(),
                                        state.expanded_crate.unwrap(),
                                        state.analysis.unwrap(),
                                        state.crate_name.unwrap(),
                                        CallbackHandler {
                                            callback: &mut |a| {
                                                let mut analysis = analysis.lock().unwrap();
                                                let a = unsafe {
                                                    ::std::mem::transmute(a.clone())
                                                };
                                                *analysis = Some(a);
                                            }
                                        });
                });
                result.after_analysis.run_callback_on_error = true;
                result.make_glob_map = rustc_resolve::MakeGlobMap::Yes;

                result
            }
        }
    }

    
}

fn make_cargo_config(build_dir: &Path, shell: Shell) -> CargoConfig {
    let config = CargoConfig::new(shell,
                                  // This is Cargo's cwd. We are using the actual cwd, but perhaps
                                  // we should use build_dir or something else?
                                  env::current_dir().unwrap(),
                                  homedir(&build_dir).unwrap());

    // Cargo is expecting the config to come from a config file and keeps
    // track of the path to that file. We'll make one up, it shouldn't be
    // used for much. Cargo does use it for finding a root path. Since
    // we pass an absolute path for the build directory, that doesn't
    // matter too much. However, Cargo still takes the grandparent of this
    // path, so we need to have at least two path elements.
    let config_path = build_dir.join("config").join("rls-config.toml");

    let mut config_value_map = config.load_values().unwrap();
    {
        let build_value = config_value_map.entry("build".to_owned()).or_insert(ConfigValue::Table(HashMap::new(), config_path.clone()));

        let target_dir = build_dir.join("target").join("rls").to_str().unwrap().to_owned();
        let td_value = ConfigValue::String(target_dir, config_path);
        if let &mut ConfigValue::Table(ref mut build_table, _) = build_value {
            build_table.insert("target-dir".to_owned(), td_value);
        } else {
            unreachable!();
        }
    }

    config.set_values(config_value_map).unwrap();
    config
}

fn parse_arg(args: &[OsString], arg: &str) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a == arg {
            return Some(args[i + 1].clone().into_string().unwrap());
        }
    }
    None
}

fn current_sysroot() -> Option<String> {
    let home = env::var("RUSTUP_HOME").or(env::var("MULTIRUST_HOME"));
    let toolchain = env::var("RUSTUP_TOOLCHAIN").or(env::var("MULTIRUST_TOOLCHAIN"));
    if let (Ok(home), Ok(toolchain)) = (home, toolchain) {
        Some(format!("{}/toolchains/{}", home, toolchain))
    } else {
        let rustc_exe = env::var("RUSTC").unwrap_or("rustc".to_owned());
        env::var("SYSROOT")
            .map(|s| s.to_owned())
                .ok()
                .or_else(|| Command::new(rustc_exe)
                    .arg("--print")
                    .arg("sysroot")
                    .output()
                    .ok()
                    .and_then(|out| String::from_utf8(out.stdout).ok())
                    .map(|s| s.trim().to_owned()))
    }
}

// A threadsafe buffer for writing.
struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

// An RAII helper to set and reset the current working directory and env vars.
struct Environment {
    old_vars: HashMap<String, Option<OsString>>,
}

impl Environment {
    fn push(envs: &HashMap<String, Option<OsString>>) -> Environment {
        let mut result = Environment {
            old_vars: HashMap::new(),
        };

        for (k, v) in envs {
            result.old_vars.insert(k.to_owned(), env::var_os(k));
            match *v {
                Some(ref v) => env::set_var(k, v),
                None => env::remove_var(k),
            }
        }
        result
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (k, v) in &self.old_vars {
            match *v {
                Some(ref v) => env::set_var(k, v),
                None => env::remove_var(k),
            }
        }
    }
}

fn convert_message_to_json_strings(input: Vec<u8>) -> Vec<String> {
    let mut output = vec![];

    // FIXME: this is *so gross*  Trying to work around cargo not supporting json messages
    let it = input.into_iter();

    let mut read_iter = it.skip_while(|&x| x != b'{');

    let mut _msg = String::new();
    loop {
        match read_iter.next() {
            Some(b'\n') => {
                output.push(_msg);
                _msg = String::new();
                while let Some(res) = read_iter.next() {
                    if res == b'{' {
                        _msg.push('{');
                        break;
                    }
                }
            }
            Some(x) => {
                _msg.push(x as char);
            }
            None => {
                break;
            }
        }
    }

    output
}

/// Tries to read a file from a list of replacements, and if the file is not
/// there, then reads it from disk, by delegating to `RealFileLoader`.
pub struct ReplacedFileLoader {
    replacements: HashMap<PathBuf, String>,
    real_file_loader: RealFileLoader,
}

impl ReplacedFileLoader {
    pub fn new(replacements: HashMap<PathBuf, String>) -> ReplacedFileLoader {
        ReplacedFileLoader {
            replacements: replacements,
            real_file_loader: RealFileLoader,
        }
    }
}

impl FileLoader for ReplacedFileLoader {
    fn file_exists(&self, path: &Path) -> bool {
        self.real_file_loader.file_exists(path)
    }

    fn abs_path(&self, path: &Path) -> Option<PathBuf> {
        self.real_file_loader.abs_path(path)
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        if let Some(abs_path) = self.abs_path(path) {
            if self.replacements.contains_key(&abs_path) {
                return Ok(self.replacements[&abs_path].clone());
            }
        }
        self.real_file_loader.read_file(path)
    }
}

/// flag_str is a string of command line args for Rust. This function removes any
/// duplicate flags.
fn dedup_flags(flag_str: &str) -> String {
    // The basic strategy here is that we split flag_str into a set of keys and
    // values and dedup any duplicate keys, using the last value in flag_str.
    // This is a bit complicated because of the variety of ways args can be specified.

    // Retain flags order to prevent complete project rebuild due to RUSTFLAGS fingerprint change
    let mut flags = BTreeMap::new();
    let mut bits = flag_str.split_whitespace().peekable();

    while let Some(bit) = bits.next() {
        let mut bit = bit.to_owned();
        // Handle `-Z foo` the same way as `-Zfoo`.
        if bit.len() == 2 && bits.peek().is_some() && !bits.peek().unwrap().starts_with('-') {
            let bit_clone = bit.clone();
            let mut bit_chars = bit_clone.chars();
            if bit_chars.next().unwrap() == '-' && bit_chars.next().unwrap() != '-' {
                bit.push_str(bits.next().unwrap());
            }
        }

        if bit.starts_with('-') {
            if bit.contains('=') {
                let bits: Vec<_> = bit.split('=').collect();
                assert!(bits.len() == 2);
                flags.insert(bits[0].to_owned() + "=", bits[1].to_owned());
            } else {
                if bits.peek().is_some() && !bits.peek().unwrap().starts_with('-') {
                    flags.insert(bit, bits.next().unwrap().to_owned());
                } else {
                    flags.insert(bit, String::new());
                }
            }
        } else {
            // A standalone arg with no flag, no deduplication to do. We merge these
            // together, which is probably not ideal, but is simple.
            flags.entry(String::new()).or_insert(String::new()).push_str(&format!(" {}", bit));
        }
    }

    // Put the map back together as a string.
    let mut result = String::new();
    for (k, v) in &flags {
        if k.is_empty() {
            result.push_str(v);
        } else {
            result.push(' ');
            result.push_str(k);
            if !v.is_empty() {
                if !k.ends_with('=') {
                    result.push(' ');
                }
                result.push_str(v);
            }
        }
    }
    result
}

#[cfg(test)]
mod test {
    use super::dedup_flags;

    #[test]
    fn test_dedup_flags() {
        // These should all be preserved.
        assert!(dedup_flags("") == "");
        assert!(dedup_flags("-Zfoo") == " -Zfoo");
        assert!(dedup_flags("-Z foo") == " -Zfoo");
        assert!(dedup_flags("-Zfoo bar") == " -Zfoo bar");
        let result = dedup_flags("-Z foo foo bar");
        assert!(result.matches("foo").count() == 2);
        assert!(result.matches("bar").count() == 1);

        // These should dedup.
        assert!(dedup_flags("-Zfoo -Zfoo") == " -Zfoo");
        assert!(dedup_flags("-Zfoo -Zfoo -Zfoo") == " -Zfoo");
        let result = dedup_flags("-Zfoo -Zfoo -Zbar");
        assert!(result.matches("foo").count() == 1);
        assert!(result.matches("bar").count() == 1);
        let result = dedup_flags("-Zfoo -Zbar -Zfoo -Zbar -Zbar");
        assert!(result.matches("foo").count() == 1);
        assert!(result.matches("bar").count() == 1);
        assert!(dedup_flags("-Zfoo -Z foo") == " -Zfoo");

        assert!(dedup_flags("--error-format=json --error-format=json") == " --error-format=json");
        assert!(dedup_flags("--error-format=foo --error-format=json") == " --error-format=json");
    }
}
