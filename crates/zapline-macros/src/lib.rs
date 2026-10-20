//! Route attribute macros for zapline.
//!
//! `#[get("/users/{id}")] async fn get_user(...)` turns `get_user` into a
//! zero-sized route value that can be passed to `App::mount`.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{parse_macro_input, Ident, ItemFn, LitStr, Token};

struct RouteArgs {
    methods: Vec<Ident>,
    path: LitStr,
}

/// `#[route("/path", method = "GET", method = "POST")]`
impl Parse for RouteArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;
        let mut methods = Vec::new();
        if input.parse::<Option<Token![,]>>()?.is_some() {
            let pairs = Punctuated::<syn::MetaNameValue, Token![,]>::parse_terminated(input)?;
            for p in pairs {
                if !p.path.is_ident("method") {
                    return Err(syn::Error::new_spanned(
                        p.path,
                        "expected `method = \"...\"`",
                    ));
                }
                match p.value {
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) => {
                        methods.push(Ident::new(&s.value().to_uppercase(), s.span()));
                    }
                    other => return Err(syn::Error::new_spanned(other, "expected a string")),
                }
            }
        }
        if methods.is_empty() {
            return Err(input.error("`#[route]` needs at least one `method = \"...\"`"));
        }
        Ok(RouteArgs { methods, path })
    }
}

fn expand(methods: Vec<Ident>, path: LitStr, item: ItemFn) -> TokenStream {
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
    let attrs: Vec<_> = item
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .collect();
    let mut inner = item.clone();
    inner.vis = syn::Visibility::Inherited;
    inner.attrs.retain(|a| !a.path().is_ident("doc"));

    let registrations = methods.iter().map(|m| {
        let m = Ident::new(&m.to_string(), Span::call_site());
        quote! { let routes = routes.route(::zapline::Method::#m, #path, #name); }
    });

    quote! {
        #(#attrs)*
        #[allow(non_camel_case_types, missing_docs)]
        #[derive(Clone, Copy, Debug)]
        #vis struct #name;

        impl ::zapline::IntoRoutes for #name {
            fn into_routes(self, routes: ::zapline::Routes) -> ::zapline::Routes {
                #inner
                #(#registrations)*
                routes
            }
        }
    }
    .into()
}

macro_rules! method_macro {
    ($($name:ident => $method:literal),* $(,)?) => {$(
        #[doc = concat!("Register an `async fn` as a `", $method, "` route.")]
        #[proc_macro_attribute]
        pub fn $name(attr: TokenStream, item: TokenStream) -> TokenStream {
            let path = parse_macro_input!(attr as LitStr);
            let item = parse_macro_input!(item as ItemFn);
            expand(vec![Ident::new($method, Span::call_site())], path, item)
        }
    )*};
}

method_macro! {
    get => "GET", post => "POST", put => "PUT", patch => "PATCH",
    delete => "DELETE", head => "HEAD", options => "OPTIONS",
}

/// Register an `async fn` for several methods:
/// `#[route("/items", method = "GET", method = "POST")]`.
#[proc_macro_attribute]
pub fn route(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as RouteArgs);
    let item = parse_macro_input!(item as ItemFn);
    expand(args.methods, args.path, item)
}
