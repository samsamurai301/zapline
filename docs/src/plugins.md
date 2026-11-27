# Plugins (WebAssembly)

Some apps run code they did not write: a shop's discount rules, a
customer's webhook transform, a tenant's validation. The `zapline-wasm`
crate runs that code as WebAssembly components, sandboxed and metered, so
a plugin can compute but cannot reach anything you do not lend it.

```toml
[dependencies]
zapline-wasm = "0.1"
```

It is a separate crate because wasmtime needs Rust 1.97, newer than
zapline's minimum (1.88).

## What a plugin gets

- **A fresh instance per call.** Nothing carries over between calls.
- **Fuel:** about one unit per instruction; 50 million a call by default.
  An endless loop ends with an error, not a stuck thread.
- **Memory and tables:** each linear memory stops at 32 MiB and each table
  at 10,000 elements by default; `memory.grow` or `table.grow` past the cap
  returns -1 (not an error: most plugins then trap, which is one).
- **Size:** `component` refuses anything over 64 MiB before compiling it.
- **Only what you link.** No WASI: no network, files, clock or randomness.
  The same input always gives the same answer.

A trap, running out of fuel or memory, or a component that asks for an
import you did not link, is an `Err`, never a panic. Keep the reason in a
`PluginLog` so whoever installed the plugin can see why it failed.

## Your interface

Describe what the plugin exports and what the host lends in WIT, and let
`bindgen!` write the types. Point it at the wasmtime `zapline-wasm`
re-exports so the versions always match:

```rust,ignore
zapline_wasm::wasmtime::component::bindgen!({
    path: "wit/discounts.wit",
    world: "discounts",
    wasmtime_crate: zapline_wasm::wasmtime,
});
```

The store's data is a `Sandbox<T>`: your state is `T`, and the host
traits `bindgen!` generates are implemented on it.

```rust,ignore
use zapline_wasm::wasmtime::component::HasSelf;
use zapline_wasm::{Limits, PluginLog, WasmHost};

struct State {
    settings: std::collections::HashMap<String, String>,
    log: PluginLog,
}

impl acme::plugin::host::Host for State {
    fn setting(&mut self, key: String) -> Option<String> {
        self.settings.get(&key).cloned()
    }
}

let host = WasmHost::new(Limits::default())?;
let component = host.component(&std::fs::read("plugin.wasm")?)?;
let mut linker = host.linker::<State>();
acme::plugin::host::add_to_linker::<_, HasSelf<State>>(&mut linker, |s| &mut s.data)?;
let pre = DiscountsPre::new(linker.instantiate_pre(&component)?)?;

// Per call: a fresh store with the limits and a full tank.
let log = PluginLog::new(200);
let answer = host.call(State { settings, log: log.clone() }, |store| {
    pre.instantiate(&mut *store)?.call_discount(&mut *store, &cart)
});
if let Err(e) = &answer {
    log.push(format!("error: {e}"));
}
```

Compile the component and build the `*Pre` once, at startup; a call then
only instantiates. Change the limits with `Limits::default().fuel(..)` and
`.memory_bytes(..)`, `.table_elements(..)` and `.component_bytes(..)`.

## Building a plugin

Any language with a component toolchain works against your WIT file: Rust
with `wit-bindgen` or `cargo component`, JavaScript with `jco`, Python with
`componentize-py`, Go with TinyGo.
