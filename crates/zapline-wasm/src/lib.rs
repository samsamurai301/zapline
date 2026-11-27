//! Run untrusted WebAssembly components (plugins) from a zapline app.
//!
//! A [`WasmHost`] holds one wasmtime engine set up for the component model
//! and fuel metering. [`WasmHost::call`] runs a closure against a fresh
//! [`Store`] that has the host's [`Limits`]: a fuel budget, a memory cap, a
//! table size cap and a cap on instances and tables. Nothing is linked in unless you link it:
//! no WASI, so a plugin has no network, files, clock or randomness, and the
//! same input gives the same answer.
//!
//! Your own interfaces come from `wasmtime::component::bindgen!`, pointed at
//! the wasmtime this crate re-exports (`wasmtime_crate:
//! zapline_wasm::wasmtime`), so the versions always match. Their host traits
//! are implemented on your state `T`; the store's data is a [`Sandbox<T>`],
//! so link them with `|s: &mut Sandbox<T>| &mut s.data`.
//!
//! ```
//! use zapline_wasm::{Limits, WasmHost};
//!
//! let host = WasmHost::new(Limits::default().fuel(1_000_000)).unwrap();
//! assert_eq!(host.limits().fuel, 1_000_000);
//! // Not a component: a clear error, never a panic.
//! assert!(host.component(b"nonsense").is_err());
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

pub use wasmtime;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

/// What one call may use.
///
/// ```
/// let limits = zapline_wasm::Limits::default().fuel(5_000_000).memory_bytes(8 << 20);
/// assert_eq!(limits.memory_bytes, 8 << 20);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Fuel (about one per WebAssembly instruction) a call may burn.
    /// Default 50 million.
    pub fuel: u64,
    /// Bytes each linear memory may grow to. Default 32 MiB.
    pub memory_bytes: usize,
    /// Core instances in one store (a component has several). Default 16.
    pub instances: usize,
    /// Memories in one store. Default 4.
    pub memories: usize,
    /// Tables in one store. Default 16.
    pub tables: usize,
    /// Elements each table may grow to. Default 10,000.
    pub table_elements: usize,
    /// Largest component [`WasmHost::component`] compiles, in bytes.
    /// Default 64 MiB.
    pub component_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 50_000_000,
            memory_bytes: 32 << 20,
            instances: 16,
            memories: 4,
            tables: 16,
            table_elements: 10_000,
            component_bytes: 64 << 20,
        }
    }
}

impl Limits {
    /// Sets [`Limits::fuel`].
    ///
    /// ```
    /// assert_eq!(zapline_wasm::Limits::default().fuel(10).fuel, 10);
    /// ```
    #[must_use]
    pub fn fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    /// Sets [`Limits::memory_bytes`].
    ///
    /// ```
    /// assert_eq!(zapline_wasm::Limits::default().memory_bytes(1 << 20).memory_bytes, 1 << 20);
    /// ```
    #[must_use]
    pub fn memory_bytes(mut self, bytes: usize) -> Self {
        self.memory_bytes = bytes;
        self
    }

    /// Sets [`Limits::table_elements`].
    ///
    /// ```
    /// assert_eq!(zapline_wasm::Limits::default().table_elements(100).table_elements, 100);
    /// ```
    #[must_use]
    pub fn table_elements(mut self, elements: usize) -> Self {
        self.table_elements = elements;
        self
    }

    /// Sets [`Limits::component_bytes`].
    ///
    /// ```
    /// assert_eq!(zapline_wasm::Limits::default().component_bytes(1 << 20).component_bytes, 1 << 20);
    /// ```
    #[must_use]
    pub fn component_bytes(mut self, bytes: usize) -> Self {
        self.component_bytes = bytes;
        self
    }
}

/// A store's data: your state, and the limits wasmtime enforces.
///
/// ```
/// let host = zapline_wasm::WasmHost::new(Default::default()).unwrap();
/// let store = host.store(5u32);
/// assert_eq!(store.data().data, 5);
/// ```
pub struct Sandbox<T> {
    /// What your host functions see.
    pub data: T,
    limits: StoreLimits,
}

/// Why a call failed: a trap, fuel or memory running out, or a bad answer.
///
/// ```
/// let e = zapline_wasm::CallError::new("all fuel consumed");
/// assert_eq!(e.to_string(), "all fuel consumed");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallError(String);

impl CallError {
    /// An error with `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CallError {}

/// Runs components; cheap to clone, one per app.
///
/// ```
/// let host = zapline_wasm::WasmHost::new(Default::default()).unwrap();
/// let other = host.clone();
/// assert_eq!(other.limits(), host.limits());
/// ```
#[derive(Clone)]
pub struct WasmHost {
    engine: Engine,
    limits: Limits,
}

impl WasmHost {
    /// An engine for components, metered by fuel.
    ///
    /// # Errors
    ///
    /// When wasmtime cannot set up its compiler on this platform.
    ///
    /// ```
    /// assert!(zapline_wasm::WasmHost::new(Default::default()).is_ok());
    /// ```
    pub fn new(limits: Limits) -> Result<Self, CallError> {
        let mut config = Config::new();
        config.wasm_component_model(true).consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| CallError::new(e.to_string()))?;
        Ok(Self { engine, limits })
    }

    /// The limits every call gets.
    ///
    /// ```
    /// let host = zapline_wasm::WasmHost::new(Default::default()).unwrap();
    /// assert_eq!(host.limits().memory_bytes, 32 << 20);
    /// ```
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The engine, for anything this crate does not wrap.
    ///
    /// ```
    /// let host = zapline_wasm::WasmHost::new(Default::default()).unwrap();
    /// let _engine = host.engine();
    /// ```
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compiles a component.
    ///
    /// # Errors
    ///
    /// When `wasm` is larger than [`Limits::component_bytes`], or is not a
    /// valid component (a core module is not one).
    ///
    /// ```
    /// let host = zapline_wasm::WasmHost::new(Default::default()).unwrap();
    /// assert!(host.component(&[0, 97, 115, 109, 1, 0, 0, 0]).is_err());
    /// ```
    pub fn component(&self, wasm: &[u8]) -> Result<Component, CallError> {
        if wasm.len() > self.limits.component_bytes {
            return Err(CallError::new(format!(
                "component is {} bytes, more than the {} allowed",
                wasm.len(),
                self.limits.component_bytes
            )));
        }
        Component::new(&self.engine, wasm)
            .map_err(|e| CallError::new(format!("not a component: {e}")))
    }

    /// An empty linker for stores of [`Sandbox<T>`]: add your interfaces.
    ///
    /// ```
    /// let host = zapline_wasm::WasmHost::new(Default::default()).unwrap();
    /// let _linker = host.linker::<()>();
    /// ```
    pub fn linker<T: 'static>(&self) -> Linker<Sandbox<T>> {
        Linker::new(&self.engine)
    }

    /// A fresh store holding `data`, with the host's limits and its fuel.
    ///
    /// ```
    /// let host = zapline_wasm::WasmHost::new(Default::default()).unwrap();
    /// let store = host.store(());
    /// assert_eq!(store.get_fuel().unwrap(), host.limits().fuel);
    /// ```
    pub fn store<T: 'static>(&self, data: T) -> Store<Sandbox<T>> {
        let l = self.limits;
        let mut store = Store::new(
            &self.engine,
            Sandbox {
                data,
                limits: StoreLimitsBuilder::new()
                    .memory_size(l.memory_bytes)
                    .instances(l.instances)
                    .memories(l.memories)
                    .tables(l.tables)
                    .table_elements(l.table_elements)
                    .build(),
            },
        );
        store.limiter(|s| &mut s.limits);
        // Fuel is on for this engine, so this cannot fail.
        let _ = store.set_fuel(l.fuel);
        store
    }

    /// Runs `call` against a fresh store holding `data`. A trap, running
    /// out of fuel, or any other error comes back as a [`CallError`] with
    /// the whole cause chain. Growing a memory or table past its cap is not
    /// an error: `memory.grow` and `table.grow` return -1, and the plugin
    /// decides what to do (most trap, which is then an error).
    ///
    /// # Errors
    ///
    /// Whatever `call` returns, as text.
    ///
    /// ```
    /// let host = zapline_wasm::WasmHost::new(Default::default()).unwrap();
    /// let answer = host.call(2u32, |store| Ok(store.data().data * 21));
    /// assert_eq!(answer, Ok(42));
    /// ```
    pub fn call<T: 'static, R>(
        &self,
        data: T,
        call: impl FnOnce(&mut Store<Sandbox<T>>) -> wasmtime::Result<R>,
    ) -> Result<R, CallError> {
        let mut store = self.store(data);
        call(&mut store).map_err(|e| CallError::new(format!("{e:#}")))
    }
}

/// The last lines a plugin wrote, and why its calls failed; shared by
/// clones, safe across threads.
///
/// ```
/// let log = zapline_wasm::PluginLog::new(2);
/// for line in ["a", "b", "c"] {
///     log.push(line);
/// }
/// assert_eq!(log.lines(), ["b", "c"]);
/// ```
#[derive(Clone)]
pub struct PluginLog {
    lines: Arc<Mutex<VecDeque<String>>>,
    keep: usize,
}

impl PluginLog {
    /// A log keeping the last `keep` lines.
    pub fn new(keep: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::new())),
            keep: keep.max(1),
        }
    }

    /// Adds a line, cut to 1,000 characters, dropping the oldest when full.
    ///
    /// ```
    /// let log = zapline_wasm::PluginLog::new(10);
    /// log.push("x".repeat(5_000));
    /// assert_eq!(log.lines()[0].len(), 1_000);
    /// ```
    pub fn push(&self, line: impl AsRef<str>) {
        let line: String = line.as_ref().chars().take(1_000).collect();
        if let Ok(mut lines) = self.lines.lock() {
            while lines.len() >= self.keep {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    }

    /// The lines, oldest first.
    ///
    /// ```
    /// assert!(zapline_wasm::PluginLog::new(5).lines().is_empty());
    /// ```
    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }
}
