//! Global subscriber installation. Its own test binary (process), since the
//! global default can only be set once.

#![cfg(feature = "tracing")]

use zapline::telemetry::{self, Telemetry, TelemetryError};

#[test]
fn init_is_idempotent_and_try_init_reports_conflicts() {
    assert!(matches!(
        Telemetry::new().env_filter("not=a=filter").try_init(),
        Err(TelemetryError::InvalidFilter(_))
    ));
    telemetry::init();
    // Subsequent calls never panic...
    telemetry::init();
    Telemetry::new().json().level("debug").init();
    // ...but `try_init` says why nothing happened.
    let err = Telemetry::new().try_init().unwrap_err();
    assert!(matches!(err, TelemetryError::AlreadyInitialized), "{err}");
    tracing::info!("logged through the global subscriber");
}

#[test]
#[should_panic(expected = "invalid log filter")]
fn init_panics_on_invalid_configuration() {
    Telemetry::new().env_filter("a=b=c").init();
}
