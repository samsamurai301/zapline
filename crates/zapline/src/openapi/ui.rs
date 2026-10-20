//! Static HTML pages for interactive API docs.
//!
//! Assets load from jsDelivr (or a mirror, see `OpenApi::cdn`) with exact
//! pinned versions and Subresource Integrity hashes, so a compromised CDN
//! cannot inject code.

/// An interactive documentation UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DocsUi {
    /// [Swagger UI](https://swagger.io/tools/swagger-ui/): try requests in the browser.
    Swagger,
    /// [Scalar](https://scalar.com): a modern API reference with a built-in client.
    Scalar,
    /// [Redoc](https://redocly.com/redoc): a clean three-panel reference.
    Redoc,
}

/// Default base URL for UI assets (npm package layout).
pub(crate) const DEFAULT_CDN: &str = "https://cdn.jsdelivr.net/npm";

const SWAGGER_CSS: &str = "swagger-ui-dist@5.33.1/swagger-ui.css";
const SWAGGER_CSS_SRI: &str =
    "sha384-Ov4/wv3j2bmct8cDc5X4ngJZohVPzEmc6uDPH8WeljUxO5vtoykvMEfbu9Vh6RaW";
const SWAGGER_JS: &str = "swagger-ui-dist@5.33.1/swagger-ui-bundle.js";
const SWAGGER_JS_SRI: &str =
    "sha384-ZPehFMQommnnuaZ4rpxgkgTT2DKFVp4hZC/7pLit+9Lek9T1YGSo23eHFbvNkXkw";
const REDOC_JS: &str = "redoc@2.5.4/bundles/redoc.standalone.js";
const REDOC_JS_SRI: &str =
    "sha384-w447zOpYfw/1Tv/5AK9NfHTlQIqE3RVR6KY62jCyy9zNDgO64cMwGGP1Fj0zJVf5";
const SCALAR_JS: &str = "@scalar/api-reference@1.72.3/dist/browser/standalone.js";
const SCALAR_JS_SRI: &str =
    "sha384-HWi/QCSPi64AQ0xBXFGDk+7gmvZ4hJ/7sZMIXqWVz6Ikb6+Cxej/hWKaomOStyFb";

impl DocsUi {
    /// The full HTML page for this UI, loading the spec from `spec_url` and
    /// assets from `cdn`.
    pub(crate) fn page(self, title: &str, spec_url: &str, cdn: &str) -> String {
        let title = html_escape(title);
        let url_js = js_string(spec_url);
        let cdn = html_escape(cdn.trim_end_matches('/'));
        let (name, head, body) = match self {
            DocsUi::Swagger => (
                "Swagger UI",
                format!(
                    r#"<link rel="stylesheet" href="{cdn}/{SWAGGER_CSS}" integrity="{SWAGGER_CSS_SRI}" crossorigin="anonymous">"#
                ),
                format!(
                    r##"<div id="swagger-ui"></div>
<script src="{cdn}/{SWAGGER_JS}" integrity="{SWAGGER_JS_SRI}" crossorigin="anonymous"></script>
<script>
window.onload = function () {{
  window.ui = SwaggerUIBundle({{
    url: {url_js},
    dom_id: "#swagger-ui",
    deepLinking: true,
    persistAuthorization: true
  }});
}};
</script>"##
                ),
            ),
            DocsUi::Scalar => (
                "API Reference",
                String::new(),
                format!(
                    r##"<div id="app"></div>
<script src="{cdn}/{SCALAR_JS}" integrity="{SCALAR_JS_SRI}" crossorigin="anonymous"></script>
<script>
Scalar.createApiReference("#app", {{ url: {url_js} }});
</script>"##
                ),
            ),
            DocsUi::Redoc => (
                "ReDoc",
                String::new(),
                format!(
                    r#"<redoc spec-url="{}"></redoc>
<script src="{cdn}/{REDOC_JS}" integrity="{REDOC_JS_SRI}" crossorigin="anonymous"></script>"#,
                    html_escape(spec_url)
                ),
            ),
        };
        format!(
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - {name}</title>
{head}
<style>body {{ margin: 0; }}</style>
</head>
<body>
{body}
</body>
</html>
"#
        )
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A JS string literal that is safe inside a `<script>` element.
fn js_string(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".to_owned())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_title_and_url() {
        let page = DocsUi::Swagger.page("<b>API</b>", "/x</script><script>alert(1)", DEFAULT_CDN);
        assert!(page.contains("&lt;b&gt;API&lt;/b&gt;"));
        assert!(!page.contains("</script><script>alert"));
        let page = DocsUi::Redoc.page("t", "/a\"b", "https://mirror.local/npm/");
        assert!(page.contains(r#"spec-url="/a&quot;b""#));
        assert!(page.contains(r#"src="https://mirror.local/npm/redoc@2.5.4/"#));
    }
}
