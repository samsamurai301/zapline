//! The guide's code, compiled: every ```` ```rust ```` block in `docs/src/*.md`
//! is a doctest of this crate, so an API change that breaks the guide fails
//! CI. They are compiled, not run: `build.rs` marks every block `no_run`
//! (many start a server), leaving `docs/src` as readers see it, and lists
//! every chapter with Rust in it as a `#[cfg(doctest)]` item, so new
//! chapters are checked too.
//!
//! ```sh
//! cargo test -p zapline-guide-check --doc
//! ```
//!
//! A fragment gets what it needs from hidden `# ` lines (mdBook hides them
//! too); a block that can't compile on its own (it needs a generated
//! project around it) is `rust,ignore`.

include!(concat!(env!("OUT_DIR"), "/chapters.rs"));
