//! Case conversion, English pluralisation and name validation.
//!
//! Everything here is dependency-free and deterministic. The pluralisation is a
//! pragmatic rule set (regular rules plus a table of irregular and uncountable
//! nouns), which is plenty for resource names such as `user`, `category` or
//! `blog_post`.

use anyhow::{bail, Result};

/// Rust keywords (strict and reserved, edition 2021) that cannot be used as
/// module or function names without `r#`.
const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

/// Crate names that would clash with the std library or with a dependency the
/// generated project already uses (a package cannot share its name with a
/// dependency because both would define a crate of that name).
const RESERVED_CRATE_NAMES: &[&str] = &[
    "test",
    "core",
    "std",
    "alloc",
    "proc_macro",
    "proc-macro",
    "zapline",
    "serde",
    "serde_json",
    "schemars",
    "validator",
    "tokio",
    "tracing",
    "async_trait",
    "async-trait",
];

/// Names Windows refuses to create as files.
const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Whether `s` is a Rust keyword.
pub fn is_keyword(s: &str) -> bool {
    KEYWORDS.contains(&s)
}

/// Splits an identifier-ish string into lowercase words.
///
/// Word boundaries are any non-alphanumeric character, a lower-to-upper
/// transition (`userProfile`), and the end of an acronym (`HTTPServer` splits
/// into `http` and `server`). Digits stay attached to the word before them.
pub fn words(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = prev.is_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_uppercase() && next_is_lower);
            if boundary {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `UserProfile` / `user-profile` / `user profile` -> `user_profile`.
pub fn snake_case(input: &str) -> String {
    words(input).join("_")
}

/// `user_profile` -> `user-profile`.
pub fn kebab_case(input: &str) -> String {
    words(input).join("-")
}

/// `user_profile` -> `UserProfile`.
pub fn pascal_case(input: &str) -> String {
    words(input).iter().map(|w| capitalize(w)).collect()
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ---- pluralisation -----------------------------------------------------------

const UNCOUNTABLE: &[&str] = &[
    "sheep",
    "fish",
    "series",
    "species",
    "news",
    "data",
    "metadata",
    "info",
    "information",
    "equipment",
    "money",
    "rice",
    "feedback",
    "software",
    "hardware",
    "firmware",
    "analytics",
    "media",
    "traffic",
    "advice",
];

/// (singular, plural)
const IRREGULAR: &[(&str, &str)] = &[
    ("person", "people"),
    ("man", "men"),
    ("woman", "women"),
    ("child", "children"),
    ("foot", "feet"),
    ("tooth", "teeth"),
    ("goose", "geese"),
    ("mouse", "mice"),
    ("ox", "oxen"),
    ("leaf", "leaves"),
    ("knife", "knives"),
    ("wife", "wives"),
    ("life", "lives"),
    ("half", "halves"),
    ("wolf", "wolves"),
    ("shelf", "shelves"),
    ("calf", "calves"),
    ("loaf", "loaves"),
    ("thief", "thieves"),
    ("self", "selves"),
    ("hero", "heroes"),
    ("potato", "potatoes"),
    ("tomato", "tomatoes"),
    ("echo", "echoes"),
    ("veto", "vetoes"),
    ("quiz", "quizzes"),
    ("analysis", "analyses"),
    ("basis", "bases"),
    ("crisis", "crises"),
    ("diagnosis", "diagnoses"),
    ("thesis", "theses"),
    ("axis", "axes"),
    ("criterion", "criteria"),
    ("phenomenon", "phenomena"),
    ("matrix", "matrices"),
    ("vertex", "vertices"),
    ("index", "indices"),
    ("status", "statuses"),
    ("bus", "buses"),
    ("alias", "aliases"),
    ("virus", "viruses"),
    ("bonus", "bonuses"),
    ("campus", "campuses"),
    ("census", "censuses"),
    ("focus", "focuses"),
    ("circus", "circuses"),
    ("chorus", "choruses"),
    ("gas", "gases"),
    ("bias", "biases"),
    ("atlas", "atlases"),
    ("movie", "movies"),
    ("cookie", "cookies"),
    ("zombie", "zombies"),
    ("brownie", "brownies"),
    ("selfie", "selfies"),
    ("calorie", "calories"),
    ("rookie", "rookies"),
    ("smoothie", "smoothies"),
    ("hoodie", "hoodies"),
    ("newbie", "newbies"),
    ("veggie", "veggies"),
    ("tie", "ties"),
    ("pie", "pies"),
    ("lie", "lies"),
];

/// Splits `snake_case` input into (everything-before, last word).
fn split_last(word: &str) -> (&str, &str) {
    match word.rfind('_') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    }
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// English plural of a singular noun. Only the last `_`-separated word of a
/// snake_case name is inflected: `blog_post` -> `blog_posts`.
pub fn pluralize(word: &str) -> String {
    let (head, last) = split_last(word);
    format!("{head}{}", pluralize_word(last))
}

fn pluralize_word(w: &str) -> String {
    if w.is_empty() || UNCOUNTABLE.contains(&w) {
        return w.to_string();
    }
    if let Some((_, plural)) = IRREGULAR.iter().find(|(s, _)| *s == w) {
        return (*plural).to_string();
    }
    // Already a known plural form: leave it alone.
    if IRREGULAR.iter().any(|(_, p)| *p == w) {
        return w.to_string();
    }
    if let Some(stem) = w.strip_suffix("sis") {
        return format!("{stem}ses");
    }
    if ["s", "x", "z", "ch", "sh"].iter().any(|e| w.ends_with(e)) {
        return format!("{w}es");
    }
    if let Some(stem) = w.strip_suffix('y') {
        if stem.chars().last().is_some_and(|c| !is_vowel(c)) {
            return format!("{stem}ies");
        }
    }
    format!("{w}s")
}

/// English singular of a plural noun; singular input is returned unchanged
/// where it can be recognised (`status`, `address`, `analysis`).
pub fn singularize(word: &str) -> String {
    let (head, last) = split_last(word);
    format!("{head}{}", singularize_word(last))
}

fn singularize_word(w: &str) -> String {
    if w.is_empty() || UNCOUNTABLE.contains(&w) {
        return w.to_string();
    }
    if let Some((s, _)) = IRREGULAR.iter().find(|(_, p)| *p == w) {
        return (*s).to_string();
    }
    if IRREGULAR.iter().any(|(s, _)| *s == w) {
        return w.to_string();
    }
    // Looks singular already.
    if ["ss", "us", "is"].iter().any(|e| w.ends_with(e)) {
        return w.to_string();
    }
    if let Some(stem) = w.strip_suffix("ies") {
        if stem.chars().count() >= 2 {
            return format!("{stem}y");
        }
    }
    for suffix in ["xes", "ches", "shes", "sses", "zzes"] {
        if w.ends_with(suffix) {
            return w[..w.len() - 2].to_string();
        }
    }
    match w.strip_suffix('s') {
        Some(stem) if !stem.is_empty() => stem.to_string(),
        _ => w.to_string(),
    }
}

// ---- resource names ----------------------------------------------------------

/// All spellings of a resource name used by the generators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceNames {
    /// `blog_post` (module-level singular, used for variables and the type name).
    pub singular: String,
    /// `blog_posts` (file and module name).
    pub plural: String,
    /// `BlogPost`.
    pub pascal: String,
    /// `BlogPosts`.
    pub pascal_plural: String,
    /// `/blog-posts` (URL collection path).
    pub path: String,
    /// `blog post` (for prose in doc comments).
    pub human: String,
    /// `blog posts`.
    pub plural_human: String,
}

impl ResourceNames {
    /// Derives all names from user input in any case style, singular or plural.
    /// `plural_override` forces the plural form for irregular nouns.
    pub fn parse(input: &str, plural_override: Option<&str>) -> Result<Self> {
        let singular = singularize(&snake_case(input));
        validate_identifier(&singular, input)?;
        let plural = match plural_override {
            Some(p) => {
                let p = snake_case(p);
                validate_identifier(&p, p.as_str())?;
                p
            }
            None => pluralize(&singular),
        };
        Ok(ResourceNames {
            pascal: pascal_case(&singular),
            pascal_plural: pascal_case(&plural),
            path: format!("/{}", kebab_case(&plural)),
            human: singular.replace('_', " "),
            plural_human: plural.replace('_', " "),
            singular,
            plural,
        })
    }
}

/// Validates a name that will become a Rust module/function identifier.
pub fn validate_identifier(ident: &str, original: &str) -> Result<()> {
    if ident.is_empty() {
        bail!("`{original}` does not contain any letters or digits");
    }
    // Checked on the original text: `../x` or `a/b` would otherwise be quietly
    // turned into `x` / `a_b`.
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ');
    if !original.chars().all(plain) || !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("`{original}` must only contain ASCII letters, digits, `-`, `_` or spaces");
    }
    if ident.starts_with(|c: char| c.is_ascii_digit()) {
        bail!("`{original}` must not start with a digit");
    }
    if is_keyword(ident) {
        bail!("`{ident}` is a Rust keyword; choose another name");
    }
    Ok(())
}

/// Validates a new project name against cargo's rules plus a few extras for
/// scaffolding (no dependency clashes, no reserved Windows file names).
pub fn validate_crate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("project name must not be empty");
    }
    if name.len() > 64 {
        bail!("project name `{name}` is too long (max 64 characters)");
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('_');
    if !first.is_ascii_alphabetic() {
        bail!("project name `{name}` must start with a letter");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        bail!("project name `{name}` contains invalid character `{bad}`; use letters, digits, `-` and `_`");
    }
    let lib = crate_ident(name);
    if is_keyword(&lib) {
        bail!("project name `{name}` is a Rust keyword; choose another name");
    }
    if RESERVED_CRATE_NAMES.contains(&name) || RESERVED_CRATE_NAMES.contains(&lib.as_str()) {
        bail!("project name `{name}` clashes with a crate the project depends on; choose another name");
    }
    if WINDOWS_RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
        bail!("project name `{name}` is a reserved file name on Windows; choose another name");
    }
    Ok(())
}

/// The identifier cargo derives from a package name: `my-app` -> `my_app`.
pub fn crate_ident(package_name: &str) -> String {
    package_name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_words() {
        assert_eq!(words("UserProfile"), ["user", "profile"]);
        assert_eq!(words("user_profile"), ["user", "profile"]);
        assert_eq!(words("user-profile"), ["user", "profile"]);
        assert_eq!(words("  user  profile "), ["user", "profile"]);
        assert_eq!(words("userProfile"), ["user", "profile"]);
        assert_eq!(words("HTTPServer"), ["http", "server"]);
        assert_eq!(words("parseHTTP"), ["parse", "http"]);
        assert_eq!(words("oauth2Token"), ["oauth2", "token"]);
        assert_eq!(words("v2"), ["v2"]);
        assert_eq!(words("___"), Vec::<String>::new());
        assert_eq!(words(""), Vec::<String>::new());
    }

    #[test]
    fn case_conversions() {
        assert_eq!(snake_case("BlogPost"), "blog_post");
        assert_eq!(snake_case("Blog Posts"), "blog_posts");
        assert_eq!(kebab_case("blog_post"), "blog-post");
        assert_eq!(kebab_case("BlogPost"), "blog-post");
        assert_eq!(pascal_case("blog_post"), "BlogPost");
        assert_eq!(pascal_case("blog-post"), "BlogPost");
        assert_eq!(pascal_case("user"), "User");
        assert_eq!(pascal_case("API key"), "ApiKey");
        assert_eq!(pascal_case(""), "");
    }

    #[test]
    fn pluralizes() {
        for (s, p) in [
            ("user", "users"),
            ("post", "posts"),
            ("category", "categories"),
            ("story", "stories"),
            ("day", "days"),
            ("key", "keys"),
            ("box", "boxes"),
            ("church", "churches"),
            ("dish", "dishes"),
            ("class", "classes"),
            ("address", "addresses"),
            ("status", "statuses"),
            ("bus", "buses"),
            ("analysis", "analyses"),
            ("person", "people"),
            ("child", "children"),
            ("man", "men"),
            ("leaf", "leaves"),
            ("knife", "knives"),
            ("hero", "heroes"),
            ("photo", "photos"),
            ("quiz", "quizzes"),
            ("movie", "movies"),
            ("sheep", "sheep"),
            ("series", "series"),
            ("news", "news"),
            ("data", "data"),
            ("blog_post", "blog_posts"),
            ("order_item", "order_items"),
            ("team_member", "team_members"),
            ("api_key", "api_keys"),
            ("blog_category", "blog_categories"),
            ("line_item", "line_items"),
        ] {
            assert_eq!(pluralize(s), p, "pluralize({s})");
        }
    }

    #[test]
    fn singularizes() {
        for (p, s) in [
            ("users", "user"),
            ("posts", "post"),
            ("categories", "category"),
            ("stories", "story"),
            ("boxes", "box"),
            ("churches", "church"),
            ("dishes", "dish"),
            ("classes", "class"),
            ("addresses", "address"),
            ("statuses", "status"),
            ("buses", "bus"),
            ("analyses", "analysis"),
            ("people", "person"),
            ("children", "child"),
            ("leaves", "leaf"),
            ("movies", "movie"),
            ("cookies", "cookie"),
            ("databases", "database"),
            ("houses", "house"),
            ("responses", "response"),
            ("archives", "archive"),
            ("blog_posts", "blog_post"),
            ("s", "s"),
            ("blog_categories", "blog_category"),
            ("team_people", "team_person"),
            // Already singular: unchanged.
            ("user", "user"),
            ("status", "status"),
            ("address", "address"),
            ("analysis", "analysis"),
            ("class", "class"),
            ("bonus", "bonus"),
            ("news", "news"),
            ("sheep", "sheep"),
            ("data", "data"),
            ("person", "person"),
            ("blog_post", "blog_post"),
        ] {
            assert_eq!(singularize(p), s, "singularize({p})");
        }
    }

    #[test]
    fn pluralize_is_stable_on_round_trip() {
        for w in [
            "user",
            "category",
            "box",
            "person",
            "status",
            "blog_post",
            "analysis",
        ] {
            assert_eq!(singularize(&pluralize(w)), w);
        }
    }

    #[test]
    fn resource_names_accept_any_style() {
        for input in [
            "BlogPost",
            "blog_post",
            "blog-posts",
            "Blog Posts",
            "blogPosts",
        ] {
            let n = ResourceNames::parse(input, None).unwrap();
            assert_eq!(n.singular, "blog_post", "{input}");
            assert_eq!(n.plural, "blog_posts", "{input}");
            assert_eq!(n.pascal, "BlogPost", "{input}");
            assert_eq!(n.pascal_plural, "BlogPosts", "{input}");
            assert_eq!(n.path, "/blog-posts", "{input}");
            assert_eq!(n.human, "blog post", "{input}");
            assert_eq!(n.plural_human, "blog posts", "{input}");
        }
    }

    #[test]
    fn resource_names_plural_override() {
        let n = ResourceNames::parse("goose", Some("gooses")).unwrap();
        assert_eq!(n.plural, "gooses");
        assert_eq!(n.singular, "goose");
        assert_eq!(n.path, "/gooses");
    }

    #[test]
    fn resource_names_reject_bad_input() {
        for bad in [
            "",
            "   ",
            "---",
            "9lives",
            "type",
            "Match",
            "caf\u{e9}",
            "a/b",
            "../x",
        ] {
            assert!(
                ResourceNames::parse(bad, None).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn crate_names() {
        for ok in ["my-app", "my_app", "app", "App2", "a", "hello-world-api"] {
            assert!(validate_crate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "1app",
            "-app",
            "_app",
            "my app",
            "my/app",
            "my.app",
            "../evil",
            "fn",
            "self",
            "test",
            "core",
            "zapline",
            "serde",
            "serde-json",
            "tokio",
            "nul",
            "CON",
            &"a".repeat(65),
            "caf\u{e9}",
        ] {
            assert!(
                validate_crate_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn crate_ident_replaces_dashes() {
        assert_eq!(crate_ident("my-cool-app"), "my_cool_app");
    }
}
