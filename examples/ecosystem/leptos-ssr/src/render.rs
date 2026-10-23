//! Rendering Leptos views as zapline responses.

use leptos::prelude::*;
use zapline::prelude::*;

/// Renders the view that `view` builds into a complete HTML document.
///
/// The view is built inside its own reactive [`Owner`], so components may
/// create signals, memos and context as they would in the browser; the
/// owner, and everything it holds, is dropped once the HTML is written.
///
/// ```
/// use leptos::prelude::*;
/// use leptos_ssr::render;
///
/// let name = "<you>";
/// let page = render(move || view! { <h1>{name}</h1> });
/// assert_eq!(page.0, "<!DOCTYPE html><h1>&lt;you&gt;</h1>");
/// ```
pub fn render<F, V>(view: F) -> Html<String>
where
    F: FnOnce() -> V,
    V: IntoView,
{
    let owner = Owner::new();
    let body = owner.with(|| view().to_html());
    let mut html = String::with_capacity(body.len() + 15);
    html.push_str("<!DOCTYPE html>");
    html.push_str(&body);
    Html(html)
}
