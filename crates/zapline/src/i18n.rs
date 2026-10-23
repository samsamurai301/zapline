//! Translations: messages in one TOML file per language, the visitor's
//! language picked per request, plurals, and a `t(...)` function in
//! templates.
//!
//! ```no_run
//! use zapline::i18n::{I18n, Locale};
//! use zapline::prelude::*;
//!
//! #[get("/hello/{name}")]
//! async fn hello(t: Locale, name: Path<String>) -> String {
//!     t.t_with("hello", &[("name", &name.0)])
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     let i18n = I18n::load("locales").expect("translations");
//!     App::new().i18n(i18n).mount(hello).run("0.0.0.0:8080")
//! }
//! ```
//!
//! `locales/en.toml` and `locales/fr.toml` (the file name is the language):
//!
//! ```toml
//! hello = "Hello, {name}!"
//!
//! [todos]
//! count.zero = "Nothing to do"
//! count.one = "{count} thing to do"
//! count.other = "{count} things to do"
//! ```
//!
//! ```toml
//! hello = "Bonjour, {name} !"
//! ```
//!
//! Nested tables become dotted keys (`todos.count.one`). The language comes
//! from `?lang=fr` (remembered in a `lang` cookie), then that cookie, then
//! the `Accept-Language` header, then the default language. A message
//! missing in that language falls back to the default language, then to the
//! key itself, so a gap shows up on the page instead of failing it.
//! [`I18n::missing`] lists the gaps, for a test.

use std::collections::{BTreeMap, HashMap};
use std::fmt::{self, Display, Write as _};
use std::path::Path;
use std::sync::Arc;

use crate::extract::FromRequestParts;
use crate::middleware::Next;
use crate::{App, Error, Request, Response, Result};

/// The cookie that remembers a language chosen with `?lang=`.
pub const LANG_COOKIE: &str = "lang";

/// Every language's messages. Register with [`App::i18n`]; handlers take
/// [`Locale`].
#[derive(Clone, Debug)]
pub struct I18n(Arc<Catalogs>);

#[derive(Clone, Debug, Default)]
struct Catalogs {
    /// Language tag (as written in the file name) -> key -> message.
    langs: BTreeMap<String, HashMap<String, String>>,
    default: String,
}

impl I18n {
    /// Load every `*.toml` file in `dir`; the file name (`en`, `fr`,
    /// `pt-BR`) is the language. The default language is `en` when there is
    /// one, otherwise the first file in alphabetical order.
    ///
    /// # Errors
    ///
    /// When `dir` can't be read, has no `.toml` files, or a file is not
    /// valid TOML with string messages.
    ///
    /// ```no_run
    /// let i18n = zapline::i18n::I18n::load("locales")?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn load(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let read_err = |e: std::io::Error| {
            Error::internal(format!("cannot read translations in {}", dir.display())).with_source(e)
        };
        let mut i18n = I18n(Arc::default());
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .map_err(read_err)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        files.sort();
        for path in files {
            let lang = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned();
            let text = std::fs::read_to_string(&path).map_err(read_err)?;
            i18n = i18n.messages(&lang, &text)?;
        }
        if i18n.0.langs.is_empty() {
            return Err(Error::internal(format!(
                "no translations: add files like {}/en.toml",
                dir.display()
            )));
        }
        Ok(i18n)
    }

    /// Add (or extend) language `lang` from TOML text, e.g. a file embedded
    /// with `include_str!`.
    ///
    /// # Errors
    ///
    /// When `toml` is not valid TOML with string messages.
    ///
    /// ```
    /// use zapline::i18n::I18n;
    ///
    /// let i18n = I18n::default()
    ///     .messages("en", r#"hello = "Hello, {name}!""#)?
    ///     .messages("de", r#"hello = "Hallo, {name}!""#)?;
    /// assert_eq!(i18n.locale("de").t_with("hello", &[("name", &"Ada")]), "Hallo, Ada!");
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn messages(mut self, lang: &str, toml: &str) -> Result<Self> {
        let table: toml::Table = toml.parse().map_err(|e: toml::de::Error| {
            Error::internal(format!("translations for {lang:?}: {}", e.message())).with_source(e)
        })?;
        let mut flat = HashMap::new();
        flatten("", &toml::Value::Table(table), &mut flat).map_err(|key| {
            Error::internal(format!("translations for {lang:?}: {key} is not text"))
        })?;
        let catalogs = Arc::make_mut(&mut self.0);
        if catalogs.default.is_empty() || (lang == "en" && !catalogs.langs.contains_key("en")) {
            catalogs.default = lang.to_owned();
        }
        catalogs
            .langs
            .entry(lang.to_owned())
            .or_default()
            .extend(flat);
        Ok(self)
    }

    /// Use `lang` when the visitor's language isn't available.
    ///
    /// ```
    /// # use zapline::i18n::I18n;
    /// let i18n = I18n::default().messages("de", "")?.messages("en", "")?.default_lang("de");
    /// assert_eq!(i18n.locale("ja").lang(), "de");
    /// # Ok::<(), zapline::Error>(())
    /// ```
    #[must_use]
    pub fn default_lang(mut self, lang: &str) -> Self {
        Arc::make_mut(&mut self.0).default = lang.to_owned();
        self
    }

    /// The available languages.
    ///
    /// ```
    /// # use zapline::i18n::I18n;
    /// let i18n = I18n::default().messages("fr", "")?.messages("en", "")?;
    /// assert_eq!(i18n.langs().collect::<Vec<_>>(), ["en", "fr"]);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn langs(&self) -> impl Iterator<Item = &str> {
        self.0.langs.keys().map(String::as_str)
    }

    /// Messages of the default language that another language lacks, as
    /// `(lang, key)`. Plural forms count as one message.
    ///
    /// ```
    /// # use zapline::i18n::I18n;
    /// let i18n = I18n::default()
    ///     .messages("en", "hi = \"Hi\"\nbye = \"Bye\"")?
    ///     .messages("fr", "hi = \"Salut\"")?;
    /// assert_eq!(i18n.missing(), [("fr".to_owned(), "bye".to_owned())]);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn missing(&self) -> Vec<(String, String)> {
        let catalogs = &self.0;
        let Some(default) = catalogs.langs.get(&catalogs.default) else {
            return Vec::new();
        };
        let base = |key: &str| -> String {
            match key.rsplit_once('.') {
                Some((base, form)) if PLURAL_FORMS.contains(&form) => base.to_owned(),
                _ => key.to_owned(),
            }
        };
        let mut wanted: Vec<String> = default.keys().map(|k| base(k)).collect();
        wanted.sort();
        wanted.dedup();
        let mut missing = Vec::new();
        for (lang, messages) in &catalogs.langs {
            if *lang == catalogs.default {
                continue;
            }
            let have: std::collections::HashSet<String> =
                messages.keys().map(|k| base(k)).collect();
            for key in &wanted {
                if !have.contains(key) {
                    missing.push((lang.clone(), key.clone()));
                }
            }
        }
        missing
    }

    /// The messages for `lang` (or the closest available language).
    ///
    /// ```
    /// # use zapline::i18n::I18n;
    /// let i18n = I18n::default().messages("en", "")?.messages("pt", "")?;
    /// assert_eq!(i18n.locale("pt-BR").lang(), "pt");
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn locale(&self, lang: &str) -> Locale {
        let lang = self.best(lang).unwrap_or(&self.0.default).to_owned();
        Locale {
            i18n: self.clone(),
            lang: lang.into(),
        }
    }

    /// The available language matching `tag` exactly (ignoring case), or by
    /// its primary subtag (`pt-BR` -> `pt`, `pt` -> `pt-BR`).
    fn best(&self, tag: &str) -> Option<&str> {
        let tag = tag.trim();
        if tag.is_empty() {
            return None;
        }
        let langs = || self.0.langs.keys().map(String::as_str);
        let primary = |t: &str| {
            t.split(['-', '_'])
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
        };
        langs()
            .find(|l| l.eq_ignore_ascii_case(tag))
            .or_else(|| langs().find(|l| primary(l) == primary(tag) && !l.contains(['-', '_'])))
            .or_else(|| langs().find(|l| primary(l) == primary(tag)))
    }

    /// The language for a request: `?lang=`, the `lang` cookie,
    /// `Accept-Language`, then the default.
    fn negotiate(&self, req: &Request) -> Locale {
        let query = req
            .query_string()
            .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("lang=")));
        let cookie = req
            .headers()
            .get_all(http::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .find_map(|c| c.trim().strip_prefix("lang="));
        if let Some(lang) = [query, cookie]
            .into_iter()
            .flatten()
            .find_map(|l| self.best(l))
        {
            return self.locale(lang);
        }
        let accept = req
            .headers()
            .get(http::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let mut ranked: Vec<(f32, &str)> = accept
            .split(',')
            .filter_map(|part| {
                let mut it = part.split(';');
                let tag = it.next()?.trim();
                let q = it
                    .find_map(|p| p.trim().strip_prefix("q="))
                    .map_or(Some(1.0), |q| q.trim().parse::<f32>().ok())?;
                (q > 0.0 && tag != "*").then_some((q, tag))
            })
            .collect();
        // Stable: equal weights keep the header's order.
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        let lang = ranked.iter().find_map(|(_, tag)| self.best(tag));
        self.locale(lang.unwrap_or(&self.0.default))
    }
}

impl Default for I18n {
    /// No languages yet: add them with [`messages`](Self::messages).
    fn default() -> Self {
        I18n(Arc::default())
    }
}

fn flatten(
    prefix: &str,
    value: &toml::Value,
    out: &mut HashMap<String, String>,
) -> std::result::Result<(), String> {
    match value {
        toml::Value::String(s) => {
            out.insert(prefix.to_owned(), s.clone());
            Ok(())
        }
        toml::Value::Table(table) => {
            for (key, value) in table {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&key, value, out)?;
            }
            Ok(())
        }
        _ => Err(prefix.to_owned()),
    }
}

/// The visitor's language and its messages, as a handler argument.
///
/// In templates (with the `views` feature), pass it in the context and
/// call it: `{{ t("hello", name=user.name) }}`,
/// `{{ t.plural("todos.count", todos|length) }}`, `<html lang="{{ t.lang }}">`.
///
/// ```
/// use zapline::i18n::{I18n, Locale};
///
/// let i18n = I18n::default().messages("en", r#"
///     hello = "Hello, {name}!"
///     items.one = "{count} item"
///     items.other = "{count} items"
/// "#)?;
/// let t: Locale = i18n.locale("en");
/// assert_eq!(t.t("hello"), "Hello, {name}!");
/// assert_eq!(t.t_with("hello", &[("name", &"Ada")]), "Hello, Ada!");
/// assert_eq!(t.plural("items", 3, &[]), "3 items");
/// # Ok::<(), zapline::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Locale {
    i18n: I18n,
    lang: Arc<str>,
}

impl Locale {
    /// The language tag, as its file is named (`en`, `pt-BR`).
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// The message for `key`, without filling in placeholders.
    pub fn t(&self, key: &str) -> String {
        self.message(key).unwrap_or(key).to_owned()
    }

    /// The message for `key` with `{name}` placeholders filled from `args`.
    /// Write `{{` for a literal `{`.
    pub fn t_with(&self, key: &str, args: &[(&str, &dyn Display)]) -> String {
        fill(self.message(key).unwrap_or(key), args, None)
    }

    /// The plural form of `key` for `count` (`key.one`, `key.few`,
    /// `key.other`, ... by the language's rules, or `key.zero` for 0 when
    /// there is one), with `{count}` and `args` filled in.
    pub fn plural(&self, key: &str, count: i64, args: &[(&str, &dyn Display)]) -> String {
        // The forms come from one language: this one if it has any, else
        // the default language (with its own plural rules).
        let catalogs = &self.i18n.0;
        let has_forms = |lang: &str| {
            catalogs.langs.get(lang).filter(|m| {
                PLURAL_FORMS
                    .iter()
                    .any(|f| m.contains_key(&format!("{key}.{f}")))
            })
        };
        let found = has_forms(&self.lang)
            .map(|m| (&*self.lang, m))
            .or_else(|| has_forms(&catalogs.default).map(|m| (catalogs.default.as_str(), m)));
        let message = found.and_then(|(lang, messages)| {
            let form = |form: &str| messages.get(&format!("{key}.{form}"));
            (count == 0)
                .then(|| form("zero"))
                .flatten()
                .or_else(|| form(plural_form(lang, count)))
                .or_else(|| form("other"))
        });
        fill(message.map_or(key, String::as_str), args, Some(count))
    }

    fn message(&self, key: &str) -> Option<&str> {
        let catalogs = &self.i18n.0;
        catalogs
            .langs
            .get(&*self.lang)
            .and_then(|m| m.get(key))
            .or_else(|| {
                catalogs
                    .langs
                    .get(&catalogs.default)
                    .and_then(|m| m.get(key))
            })
            .map(String::as_str)
    }
}

impl FromRequestParts for Locale {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        match req.state::<I18n>() {
            Some(i18n) => Ok(i18n.negotiate(req)),
            None => Err(Error::internal(
                "no translations registered: add `.i18n(I18n::load(\"locales\")?)` to the app",
            )
            .diagnosed("ZL0076", None)),
        }
    }
}

const PLURAL_FORMS: [&str; 6] = ["zero", "one", "two", "few", "many", "other"];

/// The CLDR plural category of an integer `n` in `lang` (common languages;
/// others use `one` for 1 and `other` otherwise).
fn plural_form(lang: &str, n: i64) -> &'static str {
    let primary = lang
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let n = n.unsigned_abs();
    let (n10, n100) = (n % 10, n % 100);
    match primary.as_str() {
        "ja" | "zh" | "ko" | "vi" | "th" | "id" | "ms" | "lo" | "my" | "km" => "other",
        "fr" | "hi" | "bn" | "fa" | "am" | "zu" | "kn" | "gu" => {
            if n <= 1 {
                "one"
            } else {
                "other"
            }
        }
        "pt" if lang.len() == 2 || lang.eq_ignore_ascii_case("pt-BR") => {
            if n <= 1 {
                "one"
            } else {
                "other"
            }
        }
        "ru" | "uk" | "be" | "sr" | "hr" | "bs" => {
            if n10 == 1 && n100 != 11 {
                "one"
            } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
                "few"
            } else {
                "many"
            }
        }
        "pl" => {
            if n == 1 {
                "one"
            } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
                "few"
            } else {
                "many"
            }
        }
        "cs" | "sk" => match n {
            1 => "one",
            2..=4 => "few",
            _ => "other",
        },
        "ar" => match (n, n100) {
            (0, _) => "zero",
            (1, _) => "one",
            (2, _) => "two",
            (_, 3..=10) => "few",
            (_, 11..=99) => "many",
            _ => "other",
        },
        _ => {
            if n == 1 {
                "one"
            } else {
                "other"
            }
        }
    }
}

/// Replace `{name}` with its argument (and `{count}` with `count`); keep
/// unknown placeholders as they are.
fn fill(message: &str, args: &[(&str, &dyn Display)], count: Option<i64>) -> String {
    let mut out = String::with_capacity(message.len() + 16);
    let mut rest = message;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some(after) = rest.strip_prefix("{{") {
            out.push('{');
            rest = after;
            continue;
        }
        let Some(end) = rest.find('}') else {
            break;
        };
        let name = &rest[1..end];
        if let Some((_, value)) = args.iter().find(|(n, _)| *n == name) {
            let _ = write!(out, "{value}");
        } else if let (Some(count), "count") = (count, name) {
            let _ = write!(out, "{count}");
        } else {
            out.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

impl App {
    /// Translate the app: handlers take [`Locale`], the visitor's language.
    /// A `?lang=` that names an available language is remembered in the
    /// `lang` cookie.
    ///
    /// ```
    /// use zapline::i18n::I18n;
    /// use zapline::prelude::*;
    ///
    /// let i18n = I18n::default().messages("en", r#"hi = "Hi""#)?;
    /// let app = App::new().i18n(i18n);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn i18n(self, i18n: I18n) -> Self {
        let remember = i18n.clone();
        let panel = i18n.clone();
        self.state(i18n)
            .wrap(move |req: Request, next: Next| {
                let chosen = req
                    .query_string()
                    .filter(|q| q.contains("lang="))
                    .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("lang=")))
                    .and_then(|l| remember.best(l))
                    .map(str::to_owned);
                async move {
                    let mut res: Response = next.run(req).await;
                    if let Some(lang) = chosen {
                        if let Ok(value) = http::HeaderValue::from_str(&format!(
                            "{LANG_COOKIE}={lang}; Path=/; Max-Age=31536000; SameSite=Lax"
                        )) {
                            res.headers_mut().append(http::header::SET_COOKIE, value);
                        }
                    }
                    res
                }
            })
            .dev_panel(crate::dev::DevPanel::new(
                "i18n",
                "Translations",
                move |_: &Request| {
                    let html = i18n_panel(&panel);
                    Box::pin(async move { html }) as crate::handler::BoxFuture<String>
                },
            ))
    }
}

fn i18n_panel(i18n: &I18n) -> String {
    let missing = i18n.missing();
    let mut html = String::from("<table><tr><th>Language<th>Messages<th>Missing</tr>");
    for (lang, messages) in &i18n.0.langs {
        let gaps: Vec<&str> = missing
            .iter()
            .filter(|(l, _)| l == lang)
            .map(|(_, k)| k.as_str())
            .collect();
        let default = if *lang == i18n.0.default {
            " (default)"
        } else {
            ""
        };
        let _ = write!(
            html,
            "<tr><td>{}{default}<td>{}<td>{}</tr>",
            crate::dev::escape(lang),
            messages.len(),
            crate::dev::escape(&gaps.join(", "))
        );
    }
    html.push_str("</table>");
    html
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.lang)
    }
}

#[cfg(feature = "views")]
mod views {
    use std::sync::Arc;

    use minijinja::value::{Kwargs, Object, Value};
    use minijinja::{Error, ErrorKind, State};

    use super::Locale;

    impl serde::Serialize for Locale {
        /// In a template context, a `Locale` is callable: `t("key", name=...)`.
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            Value::from_object(self.clone()).serialize(serializer)
        }
    }

    fn args(kwargs: &Kwargs) -> Result<Vec<(String, String)>, Error> {
        let mut out = Vec::new();
        for name in kwargs.args() {
            let value: Value = kwargs.get(name)?;
            out.push((name.to_owned(), value.to_string()));
        }
        Ok(out)
    }

    fn call(t: &Locale, key: &str, kwargs: &Kwargs, count: Option<i64>) -> Result<Value, Error> {
        let owned = args(kwargs)?;
        let refs: Vec<(&str, &dyn std::fmt::Display)> = owned
            .iter()
            .map(|(n, v)| (n.as_str(), v as &dyn std::fmt::Display))
            .collect();
        Ok(Value::from(match count {
            Some(count) => t.plural(key, count, &refs),
            None => t.t_with(key, &refs),
        }))
    }

    impl Object for Locale {
        fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
            (key.as_str() == Some("lang")).then(|| Value::from(self.lang()))
        }

        fn call(self: &Arc<Self>, _state: &State<'_, '_>, args: &[Value]) -> Result<Value, Error> {
            let (key, kwargs): (&str, Kwargs) = minijinja::value::from_args(args)?;
            call(self, key, &kwargs, None)
        }

        fn call_method(
            self: &Arc<Self>,
            _state: &State<'_, '_>,
            method: &str,
            args: &[Value],
        ) -> Result<Value, Error> {
            match method {
                "plural" => {
                    let (key, count, kwargs): (&str, i64, Kwargs) =
                        minijinja::value::from_args(args)?;
                    call(self, key, &kwargs, Some(count))
                }
                _ => Err(Error::new(
                    ErrorKind::UnknownMethod,
                    format!("t has no method {method}"),
                )),
            }
        }

        fn render(self: &Arc<Self>, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.lang())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_rules() {
        let forms =
            |lang: &str, ns: &[i64]| ns.iter().map(|&n| plural_form(lang, n)).collect::<Vec<_>>();
        assert_eq!(forms("en", &[0, 1, 2]), ["other", "one", "other"]);
        assert_eq!(forms("fr", &[0, 1, 2]), ["one", "one", "other"]);
        assert_eq!(forms("ja", &[1]), ["other"]);
        assert_eq!(
            forms("ru", &[1, 2, 5, 11, 21, 22, 25, 112]),
            ["one", "few", "many", "many", "one", "few", "many", "many"]
        );
        assert_eq!(
            forms("pl", &[1, 2, 5, 21, 22]),
            ["one", "few", "many", "many", "few"]
        );
        assert_eq!(forms("cs", &[1, 3, 5]), ["one", "few", "other"]);
        assert_eq!(
            forms("ar", &[0, 1, 2, 3, 11, 100]),
            ["zero", "one", "two", "few", "many", "other"]
        );
        assert_eq!(forms("pt-PT", &[0]), ["other"]);
        assert_eq!(forms("pt-BR", &[0]), ["one"]);
    }

    #[test]
    fn placeholders() {
        let args: &[(&str, &dyn Display)] = &[("name", &"Ada"), ("n", &3)];
        assert_eq!(fill("Hi {name}, {n} new", args, None), "Hi Ada, 3 new");
        assert_eq!(
            fill("{{literal}} {missing} {", args, None),
            "{literal}} {missing} {"
        );
        assert_eq!(fill("{count} left", &[], Some(2)), "2 left");
    }

    #[test]
    fn language_matching() {
        let i18n = I18n::default()
            .messages("en", "")
            .unwrap()
            .messages("pt-BR", "")
            .unwrap()
            .messages("de", "")
            .unwrap();
        assert_eq!(i18n.best("PT-br"), Some("pt-BR"));
        assert_eq!(i18n.best("pt"), Some("pt-BR"));
        assert_eq!(i18n.best("de-AT"), Some("de"));
        assert_eq!(i18n.best("ja"), None);
        assert_eq!(i18n.locale("ja").lang(), "en");
    }

    #[test]
    fn non_text_messages_are_rejected() {
        let err = I18n::default().messages("en", "count = 3").unwrap_err();
        assert!(
            err.message().contains("count is not text"),
            "{}",
            err.message()
        );
    }
}
