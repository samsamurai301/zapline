//! Route attribute macros for zapline.
//!
//! `#[get("/users/{id}")] async fn get_user(...)` turns `get_user` into a
//! zero-sized route value that can be passed to `App::mount`.
//!
//! Besides registering the route, the macros document it for OpenAPI:
//! doc comments become the summary/description, and the handler's argument
//! and return types describe parameters, request bodies and responses.
//! Extra metadata can be given as arguments:
//!
//! ```text
//! #[post("/users", tag = "users", status = 201, security = "bearer")]
//! #[get("/legacy", deprecated, hidden, operation_id = "legacyThing")]
//! #[route("/items", method = "GET", method = "PUT", tags("items", "admin"))]
//! ```

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::{quote, quote_spanned};
use syn::ext::IdentExt;
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    bracketed, parenthesized, parse_macro_input, Attribute, Expr, ExprLit, FnArg, GenericArgument,
    Ident, ItemFn, Lit, LitInt, LitStr, Meta, PathArguments, ReturnType, Token, Type,
};

const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

const ARGS: &str =
    "tag, tags, summary, description, operation_id, status, deprecated, hidden, security";

/// Parsed attribute arguments: the path plus optional methods and docs.
struct RouteArgs {
    path: LitStr,
    methods: Vec<Ident>,
    docs: DocArgs,
}

#[derive(Default)]
struct DocArgs {
    tags: Vec<LitStr>,
    summary: Option<LitStr>,
    description: Option<LitStr>,
    operation_id: Option<LitStr>,
    status: Option<LitInt>,
    deprecated: bool,
    hidden: bool,
    security: Vec<LitStr>,
}

impl RouteArgs {
    /// `"/path" (, key [= value | (..)])*`. `allow_method` is true for `#[route]`.
    fn parse(input: ParseStream, allow_method: bool) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error("expected the route path, e.g. `#[get(\"/users\")]`"));
        }
        let path: LitStr = input.parse()?;
        let mut methods = Vec::new();
        let mut docs = DocArgs::default();
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key = input.call(Ident::parse_any)?;
            match key.to_string().as_str() {
                "method" if allow_method => {
                    let s = parse_eq_str(input, &key)?;
                    let m = s.value().to_uppercase();
                    if !METHODS.contains(&m.as_str()) {
                        return Err(syn::Error::new(
                            s.span(),
                            format!("unsupported HTTP method `{}`", s.value()),
                        ));
                    }
                    if methods.iter().any(|x: &Ident| *x == m) {
                        return Err(syn::Error::new(s.span(), format!("duplicate method `{m}`")));
                    }
                    methods.push(Ident::new(&m, s.span()));
                }
                "method" => {
                    return Err(syn::Error::new(
                        key.span(),
                        "`method` is only allowed in `#[route(...)]`",
                    ));
                }
                "tag" => docs.tags.push(parse_eq_str(input, &key)?),
                "tags" => docs.tags.extend(parse_str_list(input, &key)?),
                "summary" => set_once(&mut docs.summary, parse_eq_str(input, &key)?, &key)?,
                "description" => set_once(&mut docs.description, parse_eq_str(input, &key)?, &key)?,
                "operation_id" => {
                    let id = parse_eq_str(input, &key)?;
                    if id.value().trim().is_empty() {
                        return Err(syn::Error::new(
                            id.span(),
                            "`operation_id` must not be empty",
                        ));
                    }
                    set_once(&mut docs.operation_id, id, &key)?;
                }
                "status" => {
                    expect_eq(input, &key, "status = 201")?;
                    let lit: LitInt = input.parse()?;
                    let code: u16 = lit.base10_parse()?;
                    if !(100..=599).contains(&code) {
                        return Err(syn::Error::new(
                            lit.span(),
                            "`status` must be an HTTP status code between 100 and 599",
                        ));
                    }
                    set_once(&mut docs.status, lit, &key)?;
                }
                "deprecated" => set_flag(&mut docs.deprecated, input)?,
                "hidden" => set_flag(&mut docs.hidden, input)?,
                "security" => docs.security.extend(parse_str_list(input, &key)?),
                other => {
                    let hint = if allow_method {
                        format!("method, {ARGS}")
                    } else {
                        ARGS.to_owned()
                    };
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown argument `{other}`; expected one of: {hint}"),
                    ));
                }
            }
        }
        if allow_method && methods.is_empty() {
            return Err(syn::Error::new(
                path.span(),
                "`#[route]` needs at least one `method = \"...\"`",
            ));
        }
        Ok(RouteArgs {
            path,
            methods,
            docs,
        })
    }
}

fn expect_eq(input: ParseStream, key: &Ident, example: &str) -> syn::Result<()> {
    if input.peek(Token![=]) {
        input.parse::<Token![=]>()?;
        Ok(())
    } else {
        Err(syn::Error::new(key.span(), format!("expected `{example}`")))
    }
}

fn parse_eq_str(input: ParseStream, key: &Ident) -> syn::Result<LitStr> {
    expect_eq(input, key, &format!("{key} = \"...\""))?;
    input.parse()
}

/// `key = "a"`, `key = ["a", "b"]` or `key("a", "b")`.
fn parse_str_list(input: ParseStream, key: &Ident) -> syn::Result<Vec<LitStr>> {
    let list = |content: ParseStream| -> syn::Result<Vec<LitStr>> {
        Ok(Punctuated::<LitStr, Token![,]>::parse_terminated(content)?
            .into_iter()
            .collect())
    };
    if input.peek(syn::token::Paren) {
        let content;
        parenthesized!(content in input);
        return list(&content);
    }
    expect_eq(
        input,
        key,
        &format!("{key} = \"...\"` or `{key}(\"a\", \"b\")"),
    )?;
    if input.peek(syn::token::Bracket) {
        let content;
        bracketed!(content in input);
        list(&content)
    } else {
        Ok(vec![input.parse()?])
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T, key: &Ident) -> syn::Result<()> {
    if slot.is_some() {
        return Err(syn::Error::new(key.span(), format!("duplicate `{key}`")));
    }
    *slot = Some(value);
    Ok(())
}

/// A bare flag (`deprecated`) or `deprecated = true/false`.
fn set_flag(flag: &mut bool, input: ParseStream) -> syn::Result<()> {
    if input.peek(Token![=]) {
        input.parse::<Token![=]>()?;
        *flag = input.parse::<syn::LitBool>()?.value;
    } else {
        *flag = true;
    }
    Ok(())
}

/// `(summary, description)` from `///` doc comments: the first paragraph is
/// the summary, the rest the description.
fn doc_comment(attrs: &[Attribute]) -> (Option<String>, Option<String>) {
    let mut raw = Vec::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("doc")) {
        if let Meta::NameValue(nv) = &attr.meta {
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
            {
                // `split`, not `lines`: an empty `///` must stay a blank line.
                raw.extend(s.value().split('\n').map(|l| l.trim_end().to_owned()));
            }
        }
    }
    let lines = unindent(raw);
    let mut iter = lines.into_iter().skip_while(|l| l.trim().is_empty());
    let summary: Vec<String> = iter
        .by_ref()
        .take_while(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_owned())
        .collect();
    let rest: Vec<String> = iter.skip_while(|l| l.trim().is_empty()).collect();
    let summary = (!summary.is_empty()).then(|| summary.join(" "));
    let description = rest.join("\n").trim_end().to_owned();
    (summary, (!description.is_empty()).then_some(description))
}

/// Strip `/** * ... */` decorations and the common indentation, like rustdoc.
fn unindent(mut lines: Vec<String>) -> Vec<String> {
    let starred = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.trim_start().starts_with('*'));
    if starred {
        for l in &mut lines {
            let t = l.trim_start();
            let t = t.strip_prefix('*').unwrap_or(t);
            *l = t.to_owned();
        }
    }
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .into_iter()
        .map(|l| l.get(indent..).unwrap_or("").to_owned())
        .collect()
}

/// Whether a type can be named in expression position inside the generated
/// code (no `impl Trait`, `_`, `Self` or macros).
fn nameable(ty: &Type) -> bool {
    fn scan(ts: TokenStream2) -> bool {
        ts.into_iter().all(|tt| match tt {
            TokenTree::Ident(i) => i != "impl" && i != "Self" && i != "_",
            TokenTree::Punct(p) => p.as_char() != '_' && p.as_char() != '!',
            TokenTree::Group(g) => scan(g.stream()),
            TokenTree::Literal(_) => true,
        })
    }
    !matches!(ty, Type::Macro(_) | Type::Never(_) | Type::Infer(_)) && scan(quote!(#ty))
}

fn strip_parens(mut ty: &Type) -> &Type {
    while let Type::Paren(p) = ty {
        ty = &p.elem;
    }
    if let Type::Group(g) = ty {
        return strip_parens(&g.elem);
    }
    ty
}

/// `(last segment name, generic type args)` for a path type like `a::b::Option<T>`.
fn last_segment(ty: &Type) -> Option<(String, Vec<&Type>)> {
    let Type::Path(p) = ty else { return None };
    if p.qself.is_some() {
        return None;
    }
    let seg = p.path.segments.last()?;
    let args = match &seg.arguments {
        PathArguments::AngleBracketed(a) => a
            .args
            .iter()
            .filter_map(|a| match a {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some((seg.ident.to_string(), args))
}

/// Statements documenting one handler argument.
fn describe_input(ty: &Type) -> TokenStream2 {
    let ty = strip_parens(ty);
    if let Some((name, args)) = last_segment(ty) {
        match (name.as_str(), args.as_slice()) {
            ("Valid", [inner]) => {
                let inner = describe_input(inner);
                return quote! { ::zapline::__private::validation_error(doc); #inner };
            }
            ("Option", [inner]) => {
                let inner = describe_input(inner);
                return quote! { ::zapline::__private::optional_input(doc, |doc| { #inner }); };
            }
            ("Result", [inner, ..]) => return describe_input(inner),
            _ => {}
        }
    }
    probe(ty, quote!(zapline_describe_input))
}

/// Statements documenting the handler's return type.
fn describe_output(ty: &Type) -> TokenStream2 {
    let ty = strip_parens(ty);
    if let Type::Tuple(t) = ty {
        if t.elems.len() >= 2 {
            // `(StatusCode, T)`, `([headers; N], T)`, `(StatusCode, [..], T)`.
            return describe_output(t.elems.last().unwrap());
        }
    }
    if let Some((name, args)) = last_segment(ty) {
        match (name.as_str(), args.as_slice()) {
            ("Option", [inner]) => {
                let inner = describe_output(inner);
                return quote! { #inner ::zapline::__private::not_found(doc); };
            }
            ("Result", [inner, ..]) => {
                let inner = describe_output(inner);
                return quote! { #inner ::zapline::__private::error_response(doc); };
            }
            _ => {}
        }
    }
    probe(ty, quote!(zapline_describe_output))
}

/// `(&&Probe::<T>::new()).method(doc);` for a leaf type, or nothing when the
/// type can't be named here (`impl Trait`, `_`...). Wrappers around such
/// types (`Result<impl IntoResponse>`) are still documented.
fn probe(ty: &Type, method: TokenStream2) -> TokenStream2 {
    if !nameable(ty) {
        return TokenStream2::new();
    }
    quote_spanned! {ty.span()=>
        (&&::zapline::__private::Probe::<#ty>::new()).#method(doc);
    }
}

fn expand(args: RouteArgs, item: ItemFn) -> TokenStream {
    let RouteArgs {
        path,
        methods,
        docs,
    } = args;
    if item.sig.asyncness.is_none() {
        return syn::Error::new_spanned(item.sig.fn_token, "route handlers must be `async fn`")
            .to_compile_error()
            .into();
    }
    if !path.value().starts_with('/') {
        return syn::Error::new_spanned(&path, "route path must start with `/`")
            .to_compile_error()
            .into();
    }

    let vis = &item.vis;
    let name = &item.sig.ident;
    let name_str = name.unraw().to_string();
    let attrs: Vec<_> = item
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .collect();
    let mut inner = item.clone();
    inner.vis = syn::Visibility::Inherited;
    inner.attrs.retain(|a| !a.path().is_ident("doc"));

    let doc_stmts = doc_statements(&item, docs);

    let registrations = methods.iter().map(|m| {
        let m = Ident::new(&m.to_string(), Span::call_site());
        quote! {
            let routes = routes.route_with(
                ::zapline::Method::#m, #path, #name, ::zapline::__private::into_meta(&__doc),
            );
        }
    });

    quote! {
        #(#attrs)*
        #[allow(non_camel_case_types, missing_docs)]
        #[derive(Clone, Copy, Debug)]
        #vis struct #name;

        impl ::zapline::IntoRoutes for #name {
            fn into_routes(self, routes: ::zapline::Routes) -> ::zapline::Routes {
                #inner

                #[allow(unused, clippy::all)]
                fn __zapline_route_doc() -> ::zapline::__private::Doc {
                    use ::zapline::__private::describe::{
                        InputFallback as _, OutputFallback as _, ViaInput as _, ViaOutput as _,
                    };
                    let mut __doc = ::zapline::__private::doc(#name_str);
                    {
                        let doc = &mut __doc;
                        #doc_stmts
                    }
                    __doc
                }

                let __doc = __zapline_route_doc();
                #(#registrations)*
                routes
            }
        }
    }
    .into()
}

/// Statements that fill the route's `Doc` from attribute args, doc comments
/// and the handler signature.
fn doc_statements(item: &ItemFn, docs: DocArgs) -> TokenStream2 {
    let (doc_summary, doc_description) = doc_comment(&item.attrs);
    let summary = docs.summary.map(|s| s.value()).or(doc_summary);
    let description = docs.description.map(|s| s.value()).or(doc_description);

    let mut out = TokenStream2::new();
    if let Some(s) = summary {
        out.extend(quote! { ::zapline::__private::summary(doc, #s); });
    }
    if let Some(s) = description {
        out.extend(quote! { ::zapline::__private::description(doc, #s); });
    }
    for t in &docs.tags {
        out.extend(quote! { ::zapline::__private::tag(doc, #t); });
    }
    if let Some(id) = &docs.operation_id {
        out.extend(quote! { ::zapline::__private::operation_id(doc, #id); });
    }
    if let Some(status) = &docs.status {
        out.extend(quote! { ::zapline::__private::status(doc, #status); });
    }
    if docs.deprecated {
        out.extend(quote! { ::zapline::__private::deprecated(doc); });
    }
    if docs.hidden {
        out.extend(quote! { ::zapline::__private::hidden(doc); });
    }
    for s in &docs.security {
        out.extend(quote! { ::zapline::__private::security(doc, #s); });
    }

    // Generic handlers cannot be probed: their types are not concrete here.
    if !item.sig.generics.params.is_empty() {
        return out;
    }
    for arg in &item.sig.inputs {
        if let FnArg::Typed(pt) = arg {
            out.extend(describe_input(&pt.ty));
        }
    }
    match &item.sig.output {
        ReturnType::Default => out.extend(describe_output(&syn::parse_quote!(()))),
        ReturnType::Type(_, ty) => out.extend(describe_output(ty)),
    }
    out
}

macro_rules! method_macro {
    ($($name:ident => $method:literal),* $(,)?) => {$(
        #[doc = concat!("Register an `async fn` as a `", $method, "` route.")]
        ///
        /// Optional arguments after the path document the route for OpenAPI:
        /// `tag = "x"`, `tags("a", "b")`, `summary = ".."`, `description = ".."`,
        /// `operation_id = ".."`, `status = 201`, `deprecated`, `hidden`,
        /// `security = "scheme"`. Doc comments become the summary (first
        /// paragraph) and description (the rest).
        #[proc_macro_attribute]
        pub fn $name(attr: TokenStream, item: TokenStream) -> TokenStream {
            let mut args = parse_macro_input!(attr with method_args);
            let item = parse_macro_input!(item as ItemFn);
            args.methods.push(Ident::new($method, Span::call_site()));
            expand(args, item)
        }
    )*};
}

fn method_args(input: ParseStream) -> syn::Result<RouteArgs> {
    RouteArgs::parse(input, false)
}

fn route_args(input: ParseStream) -> syn::Result<RouteArgs> {
    RouteArgs::parse(input, true)
}

method_macro! {
    get => "GET", post => "POST", put => "PUT", patch => "PATCH",
    delete => "DELETE", head => "HEAD", options => "OPTIONS",
}

/// Register an `async fn` for several methods:
/// `#[route("/items", method = "GET", method = "POST")]`.
///
/// Accepts the same documentation arguments as `#[get]`.
#[proc_macro_attribute]
pub fn route(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr with route_args);
    let item = parse_macro_input!(item as ItemFn);
    expand(args, item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse::Parser;

    fn parse(tokens: TokenStream2, allow_method: bool) -> syn::Result<RouteArgs> {
        (|input: ParseStream| RouteArgs::parse(input, allow_method)).parse2(tokens)
    }

    fn err(tokens: TokenStream2, allow_method: bool) -> String {
        match parse(tokens, allow_method) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn parses_all_arguments() {
        let a = parse(
            quote!(
                "/x",
                tag = "a",
                tags("b", "c"),
                tags = ["d"],
                summary = "s",
                description = "d",
                operation_id = "op",
                status = 201,
                deprecated,
                hidden = true,
                security = "k",
                security("j"),
            ),
            false,
        )
        .unwrap();
        assert_eq!(a.path.value(), "/x");
        let tags: Vec<String> = a.docs.tags.iter().map(LitStr::value).collect();
        assert_eq!(tags, ["a", "b", "c", "d"]);
        assert_eq!(a.docs.summary.unwrap().value(), "s");
        assert_eq!(a.docs.description.unwrap().value(), "d");
        assert_eq!(a.docs.operation_id.unwrap().value(), "op");
        assert_eq!(a.docs.status.unwrap().base10_parse::<u16>().unwrap(), 201);
        assert!(a.docs.deprecated && a.docs.hidden);
        assert_eq!(a.docs.security.len(), 2);
    }

    #[test]
    fn parses_route_methods() {
        let a = parse(quote!("/x", method = "get", method = "Put"), true).unwrap();
        let methods: Vec<String> = a.methods.iter().map(Ident::to_string).collect();
        assert_eq!(methods, ["GET", "PUT"]);
    }

    #[test]
    fn helpful_errors() {
        assert!(err(quote!(), false).contains("expected the route path"));
        let e = err(quote!("/x", tagz = "a"), false);
        assert!(e.contains("unknown argument `tagz`"), "{e}");
        assert!(e.contains("expected one of: tag, tags, summary"), "{e}");
        assert!(err(quote!("/x", nope), true).contains("method, tag"));
        assert!(err(quote!("/x", method = "GET"), false).contains("only allowed in `#[route"));
        assert!(
            err(quote!("/x", summary = "a", summary = "b"), false).contains("duplicate `summary`")
        );
        assert!(err(quote!("/x", status = 42), false).contains("between 100 and 599"));
        assert!(err(quote!("/x", tag "a"), false).contains("expected `tag = \"...\"`"));
        assert!(err(quote!("/x", operation_id = " "), false).contains("must not be empty"));
        assert!(err(quote!("/x"), true).contains("at least one `method"));
        assert!(err(quote!("/x", method = "FETCH"), true).contains("unsupported HTTP method"));
        assert!(
            err(quote!("/x", method = "GET", method = "get"), true).contains("duplicate method")
        );
    }

    #[test]
    fn doc_comments() {
        let f: ItemFn = syn::parse_quote! {
            /// First line
            /// continues here.
            ///
            /// Details.
            ///
            ///     indented code
            async fn f() {}
        };
        let (summary, description) = doc_comment(&f.attrs);
        assert_eq!(summary.unwrap(), "First line continues here.");
        assert_eq!(description.unwrap(), "Details.\n\n    indented code");

        let f: ItemFn = syn::parse_quote! { async fn f() {} };
        assert_eq!(doc_comment(&f.attrs), (None, None));

        let f: ItemFn = syn::parse_quote! {
            #[doc = include_str!("lib.rs")]
            async fn f() {}
        };
        assert_eq!(doc_comment(&f.attrs), (None, None));
    }

    #[test]
    fn nameable_types() {
        let t = |ty: TokenStream2| nameable(&syn::parse2::<Type>(ty).unwrap());
        assert!(t(quote!(Json<User>)));
        assert!(t(quote!(&'static str)));
        assert!(t(quote!((
            StatusCode,
            [(HeaderName, HeaderValue); 1],
            Json<T>
        ))));
        assert!(!t(quote!(impl IntoResponse)));
        assert!(!t(quote!(Result<impl IntoResponse>)));
        assert!(!t(quote!(Json<_>)));
        assert!(!t(quote!(Self)));
        assert!(!t(quote!(!)));
    }

    #[test]
    fn output_wrappers_are_peeled() {
        let out = |ty: TokenStream2| describe_output(&syn::parse2::<Type>(ty).unwrap()).to_string();
        let s = out(quote!(Option<Result<Json<User>>>));
        assert!(
            s.contains("not_found") && s.contains("error_response"),
            "{s}"
        );
        assert!(s.contains("Probe :: < Json < User > >"), "{s}");
        let s = out(quote!((StatusCode, Json<User>)));
        assert!(s.contains("Probe :: < Json < User > >"), "{s}");
        assert!(out(quote!(impl IntoResponse)).is_empty());
        let s = out(quote!(Result<impl IntoResponse>));
        assert!(s.contains("error_response") && !s.contains("Probe"), "{s}");

        let input =
            |ty: TokenStream2| describe_input(&syn::parse2::<Type>(ty).unwrap()).to_string();
        let s = input(quote!(Valid<Json<User>>));
        assert!(
            s.contains("validation_error") && s.contains("Probe :: < Json < User > >"),
            "{s}"
        );
        assert!(input(quote!(Option<Query<Q>>)).contains("optional_input"));
    }
}
