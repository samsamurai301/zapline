//! A small component built at test time runs inside the sandbox: its host
//! import works, and fuel, memory and traps end a call cleanly.

use zapline_wasm::wasmtime::component::HasSelf;
use zapline_wasm::{Limits, PluginLog, Sandbox, WasmHost};

zapline_wasm::wasmtime::component::bindgen!({
    inline: "
        package test:sandbox;
        interface host {
            log: func(n: u32);
        }
        world plugin {
            import host;
            export spin: func(n: u64) -> u64;
            export grow: func(pages: u32) -> s32;
            export grow-table: func(n: u32) -> s32;
            export boom: func();
        }
    ",
    world: "plugin",
    wasmtime_crate: zapline_wasm::wasmtime,
});

const WIT: &str = "
    package test:sandbox;
    interface host {
        log: func(n: u32);
    }
    world plugin {
        import host;
        export spin: func(n: u64) -> u64;
        export grow: func(pages: u32) -> s32;
        export grow-table: func(n: u32) -> s32;
        export boom: func();
    }
";

/// `spin(n)` counts down from `n`, logging when it starts; `grow` asks
/// for more memory, `grow-table` for more table slots; `boom` traps.
const MODULE: &str = r#"
    (module
      (import "test:sandbox/host" "log" (func $log (param i32)))
      (memory 1)
      (table 1 funcref)
      (func (export "spin") (param $n i64) (result i64)
        (call $log (i32.wrap_i64 (local.get $n)))
        (block $done
          (loop $again
            (br_if $done (i64.eqz (local.get $n)))
            (local.set $n (i64.sub (local.get $n) (i64.const 1)))
            (br $again)))
        (i64.const 7))
      (func (export "grow") (param $pages i32) (result i32)
        (memory.grow (local.get $pages)))
      (func (export "grow-table") (param $n i32) (result i32)
        (table.grow (ref.null func) (local.get $n)))
      (func (export "boom") unreachable))
"#;

fn component() -> Vec<u8> {
    let mut module = wat::parse_str(MODULE).unwrap();
    let mut resolve = wit_parser::Resolve::default();
    let pkg = resolve.push_str("sandbox.wit", WIT).unwrap();
    let world = resolve.select_world(&[pkg], Some("plugin")).unwrap();
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

/// What the plugin's host import sees.
struct State {
    log: PluginLog,
}

impl test::sandbox::host::Host for State {
    fn log(&mut self, n: u32) {
        self.log.push(format!("spin {n}"));
    }
}

fn setup(limits: Limits) -> (WasmHost, PluginPre<Sandbox<State>>) {
    let host = WasmHost::new(limits).unwrap();
    let component = host.component(&component()).unwrap();
    let mut linker = host.linker::<State>();
    test::sandbox::host::add_to_linker::<_, HasSelf<State>>(&mut linker, |s| &mut s.data).unwrap();
    let pre = PluginPre::new(linker.instantiate_pre(&component).unwrap()).unwrap();
    (host, pre)
}

#[test]
fn a_component_runs_with_its_host_import() {
    let (host, pre) = setup(Limits::default());
    let log = PluginLog::new(10);
    let state = State { log: log.clone() };
    let answer = host.call(state, |s| {
        pre.instantiate(&mut *s)?.call_spin(&mut *s, 1_000)
    });
    assert_eq!(answer, Ok(7));
    assert_eq!(log.lines(), ["spin 1000"]);
}

#[test]
fn running_out_of_fuel_ends_the_call() {
    let (host, pre) = setup(Limits::default().fuel(100_000));
    let state = State {
        log: PluginLog::new(10),
    };
    let err = host
        .call(state, |s| {
            pre.instantiate(&mut *s)?.call_spin(&mut *s, u64::MAX)
        })
        .unwrap_err();
    assert!(err.to_string().contains("fuel"), "{err}");

    // The next call starts with a full tank.
    let state = State {
        log: PluginLog::new(10),
    };
    let again = host.call(state, |s| pre.instantiate(&mut *s)?.call_spin(&mut *s, 10));
    assert_eq!(again, Ok(7));
}

#[test]
fn memory_stops_at_the_cap() {
    let (host, pre) = setup(Limits::default().memory_bytes(4 << 16));
    let grow = |pages| {
        let state = State {
            log: PluginLog::new(10),
        };
        host.call(state, |s| {
            pre.instantiate(&mut *s)?.call_grow(&mut *s, pages)
        })
    };
    // From one page: two more fit in four, ten do not (memory.grow says -1).
    assert_eq!(grow(2), Ok(1));
    assert_eq!(grow(10), Ok(-1));
}

#[test]
fn tables_stop_at_the_cap() {
    let (host, pre) = setup(Limits::default().table_elements(100));
    let grow = |n| {
        let state = State {
            log: PluginLog::new(10),
        };
        host.call(state, |s| {
            pre.instantiate(&mut *s)?.call_grow_table(&mut *s, n)
        })
    };
    // From one slot: ten more fit in a hundred, a million do not.
    assert_eq!(grow(10), Ok(1));
    assert_eq!(grow(1_000_000), Ok(-1));
}

#[test]
fn oversized_components_are_refused_before_compiling() {
    let host = WasmHost::new(Limits::default().component_bytes(16)).unwrap();
    let err = host.component(&component()).unwrap_err();
    assert!(
        err.to_string().contains("more than the 16 allowed"),
        "{err}"
    );
}

#[test]
fn a_trap_is_an_error_not_a_panic() {
    let (host, pre) = setup(Limits::default());
    let state = State {
        log: PluginLog::new(10),
    };
    let err = host
        .call(state, |s| pre.instantiate(&mut *s)?.call_boom(&mut *s))
        .unwrap_err();
    assert!(err.to_string().contains("unreachable"), "{err}");
}

#[test]
fn imports_the_host_does_not_lend_are_refused() {
    let host = WasmHost::new(Limits::default()).unwrap();
    let component = host.component(&component()).unwrap();
    // Nothing linked: instantiating needs `test:sandbox/host`.
    let err = host
        .linker::<()>()
        .instantiate_pre(&component)
        .err()
        .unwrap();
    assert!(err.to_string().contains("test:sandbox/host"), "{err}");
}
