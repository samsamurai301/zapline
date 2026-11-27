# Stability & versioning

What you can rely on when you upgrade zapline, and how changes reach you.

## What counts as the public API

A change to any of these is a change you may have to act on, and is treated by the rules below:

| Surface | Examples |
|---|---|
| Public Rust items | Types, functions, traits, macros and their signatures in `zapline`, `zapline-macros` and `zapline-codegen` |
| Cargo features | Their names, and what each one turns on |
| Wire formats | The JSON error body (`{"error", "code", ...}`), error `code` strings, `ZL####` diagnostic codes, response headers the framework sets |
| Configuration | `zapline.toml` keys, environment variables the framework reads (`DATABASE_URL`, `SESSION_SECRET`, ...) |
| The CLI | `zapline` subcommands and flags, and the layout of projects `zapline new` and `zapline generate` create |

Not covered: items marked `#[doc(hidden)]`, anything in `__private` modules, the exact wording of error messages and log lines, the dev console's HTML, and performance (which is tracked by the benchmarks, not promised).

## Versions

zapline follows [Semantic Versioning](https://semver.org).

- **Before 1.0 (now: 0.x):** a minor release (0.1 → 0.2) may break the public API; a patch release (0.1.0 → 0.1.1) never does. Every break is listed under **Changed** or **Removed** in the [CHANGELOG](https://github.com/samsamurai301/zapline/blob/main/CHANGELOG.md), with what to do instead.
- **From 1.0:** breaking changes only in a major release.
- **Checked by a tool:** before each release, [`cargo-semver-checks`](https://github.com/obi1kenobi/cargo-semver-checks) compares the public Rust API with the previous release. A break without the matching version bump stops the release (see `RELEASING.md`).

## Deprecation

An API is not removed without warning:

1. **It's deprecated first.** It gets `#[deprecated(since = "0.x.y", note = "use ... instead")]`. Your build warns and the note names the replacement. The CHANGELOG lists it under **Deprecated**.
2. **It stays for at least one minor release** before 1.0, and until the next major release after 1.0.
3. **Then it's removed**, listed under **Removed** in the CHANGELOG.

Generated project files are yours once written: a new release never rewrites them. When a template changes, the CHANGELOG says what to update by hand.

## Minimum supported Rust version (MSRV)

- **Today: Rust 1.88**, the `rust-version` in `Cargo.toml`, checked by CI on every pull request (`cargo check` with 1.88 and all features).
- **Raising it is not a breaking change,** but it only happens in a minor release, is listed in the CHANGELOG, and stays at least six months (about four stable releases) behind the newest stable Rust.
- **Dependencies follow the MSRV:** the lockfile pins versions that build with it (`CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback cargo update`).

## Security fixes

Fixes for vulnerabilities go into the latest release as a patch release, and into the previous minor release when it is less than three months old. Report vulnerabilities privately as described in `SECURITY.md`.
