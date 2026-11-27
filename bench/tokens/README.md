# Token benchmark

How many tokens an AI assistant spends with zapline compared with axum and
actix-web: writing the same app, and reading tool output while fixing it.
Latest numbers: [results.md](results.md).

```sh
./run.sh        # needs cargo, node and npm; writes results.md
```

- **Writing**: [`src/bin/`](src/bin/) has the same notes API in each framework:
  list, create with validation (422 with field errors), fetch (404), delete,
  an OpenAPI document, interactive docs, and one end-to-end test. Each is
  idiomatic for its framework (axum and actix-web use utoipa for OpenAPI and
  validator for validation, the usual choices), formatted with rustfmt, and
  `cargo test` runs all three.
- **Reading**: the output of `cargo test` / `cargo check` for one failing
  test and one type error, against `zapline test` / `zapline check`, with
  cargo's default settings (`RUST_BACKTRACE` unset).

Tokens are counted with o200k_base (`count.mjs`, via the `gpt-tokenizer`
package). Claude's tokenizer is not public; the ratios are what matter, and
they hold across modern BPE tokenizers.

The crate is outside the zapline workspace so the comparison frameworks stay
out of its lockfile. Improvements to the axum and actix-web versions are
welcome: the point is a fair comparison.
