//! Graphviz compiled to WebAssembly (WASI preview 1), run with wasmtime.
//!
//! The module behaves like `dot`: arguments on the command line, DOT on
//! stdin, SVG on stdout, messages on stderr and a non-zero exit on error.
//! It gets no filesystem, network or environment access.
//!
//! Compiling the module takes a few seconds, so the compiled code is cached
//! under the cache directory, keyed by the module's SHA-256 and wasmtime's
//! compatibility hash (which covers the wasmtime version and settings).

use crate::cache;
use crate::render::SvgOutput;
use sha2::{Digest, Sha256};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wasmtime::{
    Config, Engine, InstancePre, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{I32Exit, WasiCtxBuilder};

static WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/graphviz-wasm/graphviz.wasm"
));

/// Native stack the guest may use. Callers must run renders on a thread
/// with at least `2 * WASM_STACK` of stack (see `server::RENDER_STACK`).
pub const WASM_STACK: usize = 16 << 20;
/// Linear memory a single render may use.
const MAX_MEMORY: usize = 1 << 30;
/// Largest SVG we accept on stdout.
const MAX_STDOUT: usize = 256 << 20;
const MAX_STDERR: usize = 1 << 20;

struct State {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

struct Bundled {
    engine: Engine,
    pre: InstancePre<State>,
    /// Renders run one at a time: the timeout bumps the engine-wide epoch,
    /// which would otherwise interrupt a neighbouring render too.
    lock: Mutex<()>,
}

fn instance() -> Result<&'static Bundled, String> {
    static INSTANCE: OnceLock<Result<Bundled, String>> = OnceLock::new();
    INSTANCE
        .get_or_init(|| {
            load().map_err(|e| {
                let msg = format!("bundled Graphviz failed to load: {e}");
                crate::log(&msg);
                msg
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The SHA-256 of the embedded module, as hex.
pub fn wasm_sha256() -> String {
    Sha256::digest(WASM)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn load() -> wasmtime::Result<Bundled> {
    let mut config = Config::new();
    config.epoch_interruption(true);
    // Graphviz recurses deeply on large graphs (rank assignment, for one);
    // the 512 KiB default overflows where native `dot` with an 8 MiB stack
    // does not. The render threads are spawned with room for this.
    config.max_wasm_stack(WASM_STACK);
    config.async_stack_size(2 * WASM_STACK);
    let engine = Engine::new(&config)?;
    let module = load_module(&engine)?;
    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |s: &mut State| &mut s.wasi)?;
    let pre = linker.instantiate_pre(&module)?;
    Ok(Bundled {
        engine,
        pre,
        lock: Mutex::new(()),
    })
}

fn module_cache_path(engine: &Engine) -> Option<PathBuf> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    let sha = wasm_sha256();
    Some(cache::cache_root()?.join("modules").join(format!(
        "graphviz-{}-{:016x}.cwasm",
        &sha[..16],
        hasher.finish()
    )))
}

fn load_module(engine: &Engine) -> wasmtime::Result<Module> {
    let path = module_cache_path(engine);
    if let Some(path) = path.as_ref().filter(|p| p.is_file()) {
        let started = Instant::now();
        // SAFETY: the file is only ever written by `Module::serialize` below,
        // into the user's own cache directory, under a name that includes the
        // engine's compatibility hash. wasmtime still checks the header and
        // rejects artifacts from another version or configuration.
        match unsafe { Module::deserialize_file(engine, path) } {
            Ok(module) => {
                crate::log(format!(
                    "bundled Graphviz: loaded compiled module {} in {} ms",
                    path.display(),
                    started.elapsed().as_millis()
                ));
                return Ok(module);
            }
            Err(e) => crate::log(format!(
                "bundled Graphviz: ignoring cached module {}: {e}",
                path.display()
            )),
        }
    }

    let started = Instant::now();
    let module = Module::new(engine, WASM)?;
    crate::log(format!(
        "bundled Graphviz: compiled {} KiB module in {} ms",
        WASM.len() / 1024,
        started.elapsed().as_millis()
    ));
    if let Some(path) = path {
        match module.serialize() {
            Ok(bytes) => match cache::write_atomic(&path, &bytes) {
                Ok(()) => {
                    remove_stale_modules(&path);
                    crate::log(format!(
                        "bundled Graphviz: cached compiled module at {}",
                        path.display()
                    ));
                }
                Err(e) => crate::log(format!(
                    "bundled Graphviz: could not cache compiled module: {e}"
                )),
            },
            Err(e) => crate::log(format!("bundled Graphviz: could not serialize module: {e}")),
        }
    }
    Ok(module)
}

/// Removes compiled modules left behind by older versions.
fn remove_stale_modules(current: &std::path::Path) {
    let (Some(dir), Some(name)) = (current.parent(), current.file_name()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if file.starts_with("graphviz-")
            && file.ends_with(".cwasm")
            && *file != *name.to_string_lossy()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Compiles (or loads) the module ahead of the first render.
pub fn warm_up() {
    let _ = instance();
}

pub fn run(source: &str, args: &[String], timeout: Duration) -> SvgOutput {
    let failed = |msg: String| SvgOutput {
        svg: None,
        stderr: String::new(),
        timed_out: false,
        failure: Some(msg),
    };
    let bundled = match instance() {
        Ok(b) => b,
        Err(e) => return failed(e),
    };
    let _guard = bundled
        .lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let stdout = MemoryOutputPipe::new(MAX_STDOUT);
    let stderr = MemoryOutputPipe::new(MAX_STDERR);
    let mut builder = WasiCtxBuilder::new();
    builder
        .stdin(MemoryInputPipe::new(source.as_bytes().to_vec()))
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .arg("dot")
        .args(args);
    let state = State {
        wasi: builder.build_p1(),
        limits: StoreLimitsBuilder::new().memory_size(MAX_MEMORY).build(),
    };
    let mut store = Store::new(&bundled.engine, state);
    store.limiter(|s| &mut s.limits);
    store.set_epoch_deadline(1);

    // Interrupt the guest when the timeout passes; dropping `done` first
    // stops the timer.
    let (done, wait) = mpsc::channel::<()>();
    let engine = bundled.engine.clone();
    let timer = std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(timeout) {
            engine.increment_epoch();
            true
        } else {
            false
        }
    });

    let result = bundled.pre.instantiate(&mut store).and_then(|instance| {
        let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
        start.call(&mut store, ())
    });
    drop(done);
    let timed_out = timer.join().unwrap_or(false);
    drop(store);

    let stdout = stdout.contents().to_vec();
    let stderr = String::from_utf8_lossy(&stderr.contents()).into_owned();
    let exit = match result {
        Ok(()) => 0,
        Err(e) => match e.downcast_ref::<I32Exit>() {
            Some(exit) => exit.0,
            None if timed_out => {
                return SvgOutput {
                    svg: None,
                    stderr,
                    timed_out: true,
                    failure: None,
                }
            }
            None => {
                crate::log(format!("bundled Graphviz trapped: {e:?}"));
                let reason = match e.downcast_ref::<wasmtime::Trap>() {
                    Some(wasmtime::Trap::StackOverflow) => {
                        "ran out of stack space on this graph; the system dot may still render it"
                            .to_string()
                    }
                    Some(trap) => format!("crashed: {trap}"),
                    None => format!("failed: {}", e.root_cause()),
                };
                return SvgOutput {
                    svg: None,
                    stderr,
                    timed_out: false,
                    failure: Some(format!("the built-in Graphviz {reason}")),
                };
            }
        },
    };
    SvgOutput {
        svg: (exit == 0 && !stdout.is_empty()).then_some(stdout),
        stderr,
        timed_out: false,
        failure: None,
    }
}
