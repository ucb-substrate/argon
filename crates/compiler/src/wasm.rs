//! Runs `extern` functions implemented in WebAssembly modules.
//!
//! A module implements ABI version 0. It exports `memory`,
//! `argon_abi_version() -> i32` returning 0, `argon_alloc(len: i32) -> i32`,
//! and one `argon_fn_<name>(ptr: i32, len: i32) -> i64` per function, which
//! reads its CBOR-encoded argument array and returns `(ptr << 32) | len` of a
//! CBOR-encoded `{"Ok": value}` or `{"Err": message}`. It may also export
//! `argon_panic_message() -> i64`, read the same way after a trap. Modules get
//! no imports, so a function can only compute its result from its arguments.

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// The ABI version this host implements.
pub const ABI_VERSION: i32 = 0;

/// The prefix of the export implementing an `extern` function.
pub const EXPORT_PREFIX: &str = "argon_fn_";

/// How long a call runs before its [`CallControl`] reports it as slow.
pub const SLOW_CALL: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(1)
};

/// Why a call returned no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The call was stopped through its [`CallControl`].
    Cancelled,
    /// The call failed for this reason.
    Failed(String),
}

impl From<String> for CallError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// Receives the name of a function whose call has run for [`SLOW_CALL`].
type SlowCallReport = Arc<dyn Fn(&str) + Send + Sync>;

/// Stops, and reports slow, calls made on a thread while it is installed with
/// [`CallControl::run`]. Calls made without one run until they finish.
#[derive(Clone, Default)]
pub struct CallControl {
    cancelled: Arc<AtomicBool>,
    on_slow_call: Option<SlowCallReport>,
}

impl std::fmt::Debug for CallControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallControl")
            .field("cancelled", &self.is_cancelled())
            .field("reports_slow_calls", &self.on_slow_call.is_some())
            .finish()
    }
}

impl CallControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Calls `report` with a function's name once a call of it has run for
    /// [`SLOW_CALL`].
    pub fn on_slow_call(mut self, report: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_slow_call = Some(Arc::new(report));
        self
    }

    /// Stops the running call, and every later one, within a few milliseconds.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Runs `f` with the calls it makes on this thread under this control.
    pub fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        struct Restore(Option<CallControl>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CONTROL.with(|control| *control.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(CONTROL.with(|control| control.replace(Some(self.clone()))));
        f()
    }

    /// The control installed on this thread, or one that never stops a call.
    #[cfg(not(target_family = "wasm"))]
    fn current() -> Self {
        CONTROL.with(|control| control.borrow().clone().unwrap_or_default())
    }
}

thread_local! {
    static CONTROL: RefCell<Option<CallControl>> = const { RefCell::new(None) };
}

#[cfg(not(target_family = "wasm"))]
pub use host::call;

/// Calls the function `name` of the module at `path` with CBOR-encoded
/// arguments, returning its CBOR-encoded result.
#[cfg(target_family = "wasm")]
pub fn call(_path: &std::path::Path, _name: &str, _input: &[u8]) -> Result<Vec<u8>, CallError> {
    Err(CallError::Failed(
        "WASM functions cannot be called in this build".to_owned(),
    ))
}

#[cfg(not(target_family = "wasm"))]
mod host {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Mutex, OnceLock},
        time::{Duration, Instant, SystemTime},
    };

    use wasmtime::{
        Config, Engine, Instance, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, Trap,
        UpdateDeadline,
    };

    use super::{ABI_VERSION, CallControl, CallError, EXPORT_PREFIX, SLOW_CALL};

    /// How often a running call checks its [`CallControl`].
    const TICK: Duration = Duration::from_millis(10);
    /// Bytes of linear memory one call may use.
    const MAX_MEMORY: usize = 1 << 30;
    /// Results kept before the memo is cleared.
    const MAX_MEMOIZED: usize = 1024;

    /// The engine, with a clock thread that makes running calls check their
    /// control without the cost of counting instructions.
    fn engine() -> &'static Engine {
        static ENGINE: OnceLock<Engine> = OnceLock::new();
        ENGINE.get_or_init(|| {
            let mut config = Config::new();
            config.epoch_interruption(true);
            let engine = Engine::new(&config).expect("the WASM engine configuration is valid");
            let clock = engine.clone();
            std::thread::Builder::new()
                .name("argon-wasm-clock".to_owned())
                .spawn(move || {
                    loop {
                        std::thread::sleep(TICK);
                        clock.increment_epoch();
                    }
                })
                .expect("the WASM clock thread starts");
            engine
        })
    }

    /// What identifies one revision of a module file.
    type Stamp = (u64, Option<SystemTime>);

    /// A call's module revision, function name, and encoded arguments.
    type CallKey = (PathBuf, Stamp, String, Vec<u8>);

    /// Results of earlier calls. A call is a pure function of its key, since
    /// each gets a fresh instance with no imports.
    static RESULTS: OnceLock<Mutex<HashMap<CallKey, Vec<u8>>>> = OnceLock::new();

    /// Compiles the module at `path`, reusing the compilation while the file
    /// is unchanged.
    fn module(path: &Path) -> Result<(Stamp, Module), String> {
        static MODULES: OnceLock<Mutex<HashMap<PathBuf, (Stamp, Module)>>> = OnceLock::new();
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("cannot read `{}`: {error}", path.display()))?;
        let stamp = (metadata.len(), metadata.modified().ok());
        let modules = MODULES.get_or_init(Default::default);
        if let Some((cached, module)) = modules.lock().unwrap().get(path)
            && *cached == stamp
        {
            return Ok((stamp, module.clone()));
        }
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read `{}`: {error}", path.display()))?;
        let module = Module::new(engine(), &bytes)
            .map_err(|error| format!("invalid WASM module `{}`: {error}", path.display()))?;
        if let Some(import) = module.imports().next() {
            return Err(format!(
                "WASM module `{}` imports `{}::{}`, but extern functions get no imports",
                path.display(),
                import.module(),
                import.name()
            ));
        }
        modules
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (stamp, module.clone()));
        Ok((stamp, module))
    }

    struct State {
        limits: StoreLimits,
    }

    /// Calls the function `name` of the module at `path` with CBOR-encoded
    /// arguments, returning its CBOR-encoded result.
    ///
    /// Each call gets a fresh instance, so no state carries between calls, and
    /// a successful result is reused for the same module and arguments. The
    /// call runs until it finishes unless this thread's [`CallControl`] stops it.
    pub fn call(path: &Path, name: &str, input: &[u8]) -> Result<Vec<u8>, CallError> {
        let (stamp, module) = module(path)?;
        let key = (path.to_path_buf(), stamp, name.to_owned(), input.to_vec());
        let results = RESULTS.get_or_init(Default::default);
        if let Some(output) = results.lock().unwrap().get(&key) {
            return Ok(output.clone());
        }
        let control = CallControl::current();
        if control.is_cancelled() {
            return Err(CallError::Cancelled);
        }
        let output = run(&module, path, name, input, control)?;
        let mut results = results.lock().unwrap();
        if results.len() >= MAX_MEMOIZED {
            results.clear();
        }
        results.insert(key, output.clone());
        Ok(output)
    }

    fn run(
        module: &Module,
        path: &Path,
        name: &str,
        input: &[u8],
        control: CallControl,
    ) -> Result<Vec<u8>, CallError> {
        let mut store = Store::new(
            engine(),
            State {
                limits: StoreLimitsBuilder::new()
                    .memory_size(MAX_MEMORY)
                    .instances(1)
                    .build(),
            },
        );
        store.limiter(|state| &mut state.limits);
        // Each clock tick, check whether the call should stop or be reported.
        store.set_epoch_deadline(1);
        let started = Instant::now();
        let mut reported = false;
        let watched = control.clone();
        let function_name = name.to_owned();
        store.epoch_deadline_callback(move |_| {
            if watched.is_cancelled() {
                return Ok(UpdateDeadline::Interrupt);
            }
            if !reported && started.elapsed() >= SLOW_CALL {
                reported = true;
                if let Some(report) = &watched.on_slow_call {
                    report(&function_name);
                }
            }
            Ok(UpdateDeadline::Continue(1))
        });
        let instance = Instance::new(&mut store, module, &[])
            .map_err(|error| format!("cannot instantiate `{}`: {error}", path.display()))?;
        let abi = instance
            .get_typed_func::<(), i32>(&mut store, "argon_abi_version")
            .map_err(|_| {
                format!(
                    "`{}` does not export `argon_abi_version`; build it with `argon-guest`",
                    path.display()
                )
            })?;
        let version = abi
            .call(&mut store, ())
            .map_err(|error| error.to_string())?;
        if version != ABI_VERSION {
            return Err(format!(
                "`{}` implements ABI version {version}, but this compiler implements version {ABI_VERSION}",
                path.display()
            )
            .into());
        }
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| format!("`{}` does not export its memory", path.display()))?;
        let function = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, &format!("{EXPORT_PREFIX}{name}"))
            .map_err(|_| format!("`{}` does not export function `{name}`", path.display()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "argon_alloc")
            .map_err(|_| format!("`{}` does not export `argon_alloc`", path.display()))?;

        let len = i32::try_from(input.len()).map_err(|_| "arguments are too large".to_owned())?;
        let result = alloc.call(&mut store, len).and_then(|ptr| {
            memory.write(&mut store, ptr as u32 as usize, input)?;
            function.call(&mut store, (ptr, len))
        });
        let packed = match result {
            Ok(packed) => packed,
            Err(error) => {
                if let Some(Trap::Interrupt) = error.downcast_ref::<Trap>()
                    && control.is_cancelled()
                {
                    return Err(CallError::Cancelled);
                }
                let message = panic_message(&mut store, &instance, memory)
                    .map(|message| format!("panic: {message}"))
                    .unwrap_or_else(|| format!("trap: {error}"));
                return Err(message.into());
            }
        };
        read_packed(&store, memory, packed)
            .ok_or_else(|| CallError::Failed("result is outside the module's memory".to_owned()))
    }

    /// The message a panicking guest recorded, if it exports one.
    fn panic_message(
        store: &mut Store<State>,
        instance: &Instance,
        memory: Memory,
    ) -> Option<String> {
        let read = instance
            .get_typed_func::<(), i64>(&mut *store, "argon_panic_message")
            .ok()?;
        let packed = read.call(&mut *store, ()).ok()?;
        let bytes = read_packed(store, memory, packed)?;
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Copies out the bytes at `(ptr << 32) | len`.
    fn read_packed(store: &Store<State>, memory: Memory, packed: i64) -> Option<Vec<u8>> {
        let packed = packed as u64;
        let ptr = (packed >> 32) as usize;
        let len = (packed & 0xffff_ffff) as usize;
        memory
            .data(store)
            .get(ptr..ptr.checked_add(len)?)
            .map(<[u8]>::to_vec)
    }
}
