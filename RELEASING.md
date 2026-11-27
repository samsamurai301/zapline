# Releasing

zapline is five crates published in dependency order. Everything except the
final `cargo publish` and the visibility switch is safe to repeat.

## Checklist

1. `./scripts/check.sh` and `./scripts/check.sh ecosystem` pass on `main`
   locally (and `.\scripts\check.ps1` on Windows). There is no hosted CI
   before the first release.
2. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and the
   `version = "..."` on the `zapline-macros` and `zapline-codegen` path
   dependencies in `crates/zapline/Cargo.toml` and on `zapline-codegen` in
   `crates/zapline-cli/Cargo.toml`. The CLI derives the version requirement it
   writes into new projects from its own version.
3. Replace `Unreleased` in `CHANGELOG.md` with the release date.
4. The version bump matches the API changes. From the second release on,
   compare the public API with the previous release's tag (needs
   `cargo install cargo-semver-checks`):

   ```sh
   cargo semver-checks -p zapline -p zapline-macros -p zapline-codegen --baseline-rev v0.1.0
   ```

   A reported break needs a minor bump before 1.0 (a major one after),
   and a **Changed** or **Removed** entry in the CHANGELOG (see the guide's
   *Stability & versioning* chapter).
   The CLI has no such tool: compare `zapline --help` and each subcommand's
   `--help` with the previous release, and diff a fresh `zapline new` project
   and `zapline g auth` against the previous release's output. A removed or
   renamed command or flag, or a changed generated layout, is a break too.
5. Dry run, which packages and builds all five crates against each other:

   ```sh
   cargo publish --dry-run -p zapline-macros -p zapline-codegen -p zapline -p zapline-cli -p zapline-wasm
   ```

6. Tag and publish (needs a crates.io token: `cargo login`):

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   cargo publish -p zapline-macros -p zapline-codegen -p zapline -p zapline-cli -p zapline-wasm
   ```

   Cargo publishes them in dependency order and waits for each to appear in
   the index. **Publishing cannot be undone**; a bad release can only be
   yanked.

7. Create the GitHub release from the tag with the changelog section.
8. Publish the docs site: `mdbook build docs` and upload `docs/book` to
   GitHub Pages (*Settings → Pages*). It is not deployed automatically.

## First public launch

- Make the repository public (*Settings → General → Danger zone*).
- Enable Discussions and private vulnerability reporting.
- Add topics: `rust`, `web-framework`, `http`, `openapi`, `fullstack`,
  `llm`, `ai-friendly`.
- Set the repository website to the docs site.
- Announce: r/rust, This Week in Rust (PR to `this-week-in-rust`), the Rust
  users forum, then a Show HN once the flagship example is live.
