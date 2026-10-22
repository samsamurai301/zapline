# Releasing

zapline is three crates published in dependency order. Everything except the
final `cargo publish` and the visibility switch is safe to repeat.

## Checklist

1. `main` is green on CI (Linux, macOS, Windows, MSRV, docs, scaffold).
2. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and the
   `version = "..."` on the `zapline-macros` path dependency in
   `crates/zapline/Cargo.toml`. The CLI derives the version requirement it
   writes into new projects from its own version.
3. Replace `Unreleased` in `CHANGELOG.md` with the release date.
4. Dry run, which packages and builds all three crates against each other:

   ```sh
   cargo publish --dry-run -p zapline-macros -p zapline -p zapline-cli
   ```

5. Tag and publish (needs a crates.io token: `cargo login`):

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   cargo publish -p zapline-macros -p zapline -p zapline-cli
   ```

   Cargo publishes them in dependency order and waits for each to appear in
   the index. **Publishing cannot be undone**; a bad release can only be
   yanked.

6. Create the GitHub release from the tag with the changelog section.
7. The docs site deploys from `main` (`.github/workflows/pages.yml`). Enable
   it once under *Settings → Pages → Source: GitHub Actions*.

## First public launch

- Make the repository public (*Settings → General → Danger zone*).
- Enable Discussions and private vulnerability reporting.
- Add topics: `rust`, `web-framework`, `http`, `openapi`, `fullstack`,
  `llm`, `ai-friendly`.
- Set the repository website to the docs site.
- Announce: r/rust, This Week in Rust (PR to `this-week-in-rust`), the Rust
  users forum, then a Show HN once the flagship example is live.
