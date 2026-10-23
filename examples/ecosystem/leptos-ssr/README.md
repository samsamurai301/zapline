# leptos-ssr: Leptos components served by zapline

A notes board whose pages are [Leptos](https://leptos.dev) 0.8 components,
rendered to HTML on the server by zapline handlers. Data comes from
zapline's SQLite integration; every button is a plain `<form>` post, so the
app works in any browser, with JavaScript off, and needs no WASM toolchain
or bundler.

```sh
cargo run                    # http://localhost:8080
cargo test                   # 5 tests + a doctest
```

## The integration is one function

```rust,ignore
pub fn render<F: FnOnce() -> V, V: IntoView>(view: F) -> Html<String> {
    let owner = Owner::new();                       // signals, memos, context work
    let body = owner.with(|| view().to_html());
    Html(format!("<!DOCTYPE html>{body}"))
}

#[get("/")]
async fn home(db: Db) -> Result<Html<String>> {
    let notes = all_notes(&db).await?;              // load first, with zapline
    Ok(render(move || view! {                       // then render, synchronously
        <Page title="Notes">
            <NotesBoard notes=notes error=None draft=String::new()/>
        </Page>
    }))
}
```

Handlers stay ordinary zapline handlers: extractors, `Db`, `Dep<T>`,
middleware, sessions and tests all work as usual. Leptos only turns data
into HTML, with its escaping (user text can't inject markup; see
`user_text_is_escaped`).

## Forms: Post/Redirect/Get

- A valid post answers `303 See Other` to `/`, so reloading never resubmits.
- An invalid post answers `422` with the whole page re-rendered: the error
  under the form (`role="alert"`) and the user's text still in the box.

## Hydration

This example renders on the server only. To make components interactive in
the browser, build a second crate with Leptos's `hydrate` feature to WASM
(with `cargo-leptos` or `wasm-bindgen`), serve its `pkg/` directory with
`ServeDir`, and render with `HydrationScripts` in the `<head>`; the server
side stays exactly as it is here.
