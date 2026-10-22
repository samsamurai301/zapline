//! The embedded project templates.
//!
//! Template sources live in `templates/` as `*.tmpl` files (the suffix keeps
//! cargo from treating the embedded `Cargo.toml` as a nested package and keeps
//! rustfmt/clippy away from placeholder-laden Rust). Placeholders are
//! `@@name@@`; see [`crate::render`].

use clap::ValueEnum;

/// Which project layout `zapline new` creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Template {
    /// A single hello-world route and an in-process test.
    Minimal,
    /// A structured REST API: routes, models, validation, OpenAPI docs, Docker, CI.
    Api,
    /// Everything in `api` plus layered config, DI, probes, metrics, tracing and a hardened middleware stack.
    Full,
}

/// One file of a template: where it goes and what it contains.
#[derive(Debug, Clone, Copy)]
pub struct TemplateFile {
    /// Destination path relative to the project root.
    pub dest: &'static str,
    /// Template source text.
    pub source: &'static str,
}

macro_rules! file {
    ($dest:literal, $src:literal) => {
        TemplateFile {
            dest: $dest,
            source: include_str!(concat!("../templates/", $src)),
        }
    };
}

const MINIMAL: &[TemplateFile] = &[
    file!("AGENTS.md", "common/AGENTS.md.tmpl"),
    file!("Cargo.toml", "minimal/Cargo.toml.tmpl"),
    file!(".gitignore", "common/gitignore.tmpl"),
    file!("README.md", "minimal/README.md.tmpl"),
    file!("src/main.rs", "common/main.rs.tmpl"),
    file!("src/lib.rs", "minimal/src/lib.rs.tmpl"),
    file!("tests/api.rs", "minimal/tests/api.rs.tmpl"),
];

const API: &[TemplateFile] = &[
    file!("AGENTS.md", "common/AGENTS.md.tmpl"),
    file!("Cargo.toml", "api/Cargo.toml.tmpl"),
    file!(".gitignore", "common/gitignore.tmpl"),
    file!(".dockerignore", "common/dockerignore.tmpl"),
    file!(".env.example", "api/env.example.tmpl"),
    file!(".github/workflows/ci.yml", "common/ci.yml.tmpl"),
    file!("Dockerfile", "api/Dockerfile.tmpl"),
    file!("README.md", "api/README.md.tmpl"),
    file!("src/main.rs", "common/main.rs.tmpl"),
    file!("src/lib.rs", "api/src/lib.rs.tmpl"),
    file!("src/models.rs", "api/src/models.rs.tmpl"),
    file!("src/routes/mod.rs", "api/src/routes/mod.rs.tmpl"),
    file!("src/routes/health.rs", "api/src/routes/health.rs.tmpl"),
    file!("src/routes/users.rs", "api/src/routes/users.rs.tmpl"),
    file!("tests/health.rs", "api/tests/health.rs.tmpl"),
    file!("tests/users.rs", "api/tests/users.rs.tmpl"),
];

const FULL: &[TemplateFile] = &[
    file!("AGENTS.md", "common/AGENTS.md.tmpl"),
    file!("Cargo.toml", "full/Cargo.toml.tmpl"),
    file!(".gitignore", "common/gitignore.tmpl"),
    file!(".dockerignore", "common/dockerignore.tmpl"),
    file!(".env.example", "full/env.example.tmpl"),
    file!(".github/workflows/ci.yml", "common/ci.yml.tmpl"),
    file!("Dockerfile", "full/Dockerfile.tmpl"),
    file!("README.md", "full/README.md.tmpl"),
    file!("config/default.toml", "full/config/default.toml.tmpl"),
    file!(
        "config/development.toml",
        "full/config/development.toml.tmpl"
    ),
    file!("config/production.toml", "full/config/production.toml.tmpl"),
    file!("src/main.rs", "full/src/main.rs.tmpl"),
    file!("src/lib.rs", "full/src/lib.rs.tmpl"),
    file!("src/config.rs", "full/src/config.rs.tmpl"),
    file!("src/models.rs", "api/src/models.rs.tmpl"),
    file!("src/repository.rs", "full/src/repository.rs.tmpl"),
    file!("src/routes/mod.rs", "full/src/routes/mod.rs.tmpl"),
    file!("src/routes/users.rs", "full/src/routes/users.rs.tmpl"),
    file!("tests/ops.rs", "full/tests/ops.rs.tmpl"),
    file!("tests/users.rs", "full/tests/users.rs.tmpl"),
];

impl Template {
    /// The files this template consists of.
    pub fn files(self) -> &'static [TemplateFile] {
        match self {
            Template::Minimal => MINIMAL,
            Template::Api => API,
            Template::Full => FULL,
        }
    }

    /// Cargo features the generated project needs on the `zapline` dependency
    /// beyond the defaults.
    pub fn zapline_features(self) -> &'static [&'static str] {
        match self {
            Template::Minimal | Template::Api => &[],
            Template::Full => &["compression"],
        }
    }

    /// The lowercase name used on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Template::Minimal => "minimal",
            Template::Api => "api",
            Template::Full => "full",
        }
    }
}

// ---- resource / handler / middleware generators ----------------------------------

/// Source of a generated resource module.
pub const RESOURCE: &str = include_str!("../templates/generate/resource.rs.tmpl");
/// Integration test for a generated resource.
pub const RESOURCE_TEST: &str = include_str!("../templates/generate/resource_test.rs.tmpl");
/// Source of a generated handler module.
pub const HANDLER: &str = include_str!("../templates/generate/handler.rs.tmpl");
/// Integration test for a generated handler.
pub const HANDLER_TEST: &str = include_str!("../templates/generate/handler_test.rs.tmpl");
/// Source of a generated middleware module.
pub const MIDDLEWARE: &str = include_str!("../templates/generate/middleware.rs.tmpl");
/// Integration test for a generated middleware.
pub const MIDDLEWARE_TEST: &str = include_str!("../templates/generate/middleware_test.rs.tmpl");
/// A fresh `src/routes/mod.rs` with the registration markers.
pub const ROUTES_MOD: &str = include_str!("../templates/generate/routes_mod.rs.tmpl");
/// A fresh `src/middleware/mod.rs`.
pub const MIDDLEWARE_MOD: &str = include_str!("../templates/generate/middleware_mod.rs.tmpl");
