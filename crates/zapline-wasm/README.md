# zapline-wasm

Run untrusted WebAssembly components (plugins) from a zapline app. Every
call gets a fresh instance with a fuel budget and a memory cap, and nothing
the host does not link in: no WASI, so no network, files or clock. A call
that traps, runs out of fuel or grows past its memory is an error, never a
panic, and the plugin's log keeps the reason.

```rust,ignore
use zapline_wasm::{Limits, Sandbox, WasmHost};

zapline_wasm::wasmtime::component::bindgen!({
    path: "wit",
    world: "discounts",
    wasmtime_crate: zapline_wasm::wasmtime,
});

let host = WasmHost::new(Limits::default())?;
let component = host.component(&wasm_bytes)?;
let mut linker = host.linker::<MyState>();
// link your host interface with `|s: &mut Sandbox<MyState>| &mut s.data`
let pre = DiscountsPre::new(linker.instantiate_pre(&component)?)?;
let answer = host.call(MyState::default(), |store| {
    pre.instantiate(&mut *store)?.call_discount(&mut *store, &cart)
});
```

It is a separate crate because wasmtime needs a newer Rust (1.97) than
zapline's MSRV (1.88). See the guide's "Plugins (WebAssembly)" chapter.
