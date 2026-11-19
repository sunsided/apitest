//! Attribute macros for apitest. Use them through the `apitest` crate with the
//! `macros` feature: `#[apitest::test]`, `#[apitest::fixture]`, `#[apitest::suite]`.
//!
//! Every annotated item stays an ordinary function and is additionally
//! registered in a `linkme` distributed slice. The libtest-mimic harness
//! collects the registrations at startup (`Plan::registered()`).
//!
//! * The **module is the suite**: items in `mod billing` belong to suite
//!   `billing` unless `suite = "..."` says otherwise.
//! * Parameters of type `Ctx` receive the context; every other parameter is a
//!   **fixture, injected by name** and deserialized into the parameter type:
//!   `token: String`, `social_worker: Actor`. `cx` is optional: requests made
//!   through `Actor`s and helpers like `step`/`undo` find the context themselves.
//! * Test ids are the module path plus the function name (`billing::create_invoice`),
//!   which is what rust-analyzer's "Run test" passes to the harness.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::{Expr, ExprArray, ExprLit, FnArg, ItemFn, Lit, Pat, ReturnType, Type, parse_macro_input};

/// Arguments of any of the attributes; [`parse_args`] rejects the keys an
/// attribute does not support.
#[derive(Default)]
struct Args {
    /// `name = "..."`: display name of a test, or storage key of a fixture.
    name: Option<String>,
    /// `suite = "..."`: suite key, overriding the module path.
    suite: Option<String>,
    /// `scope = "suite" | "vu" | "test"` of a fixture.
    scope: Option<String>,
    /// `teardown = path`: async fn run when a fixture's scope ends.
    teardown: Option<syn::Path>,
    /// `parallel`: the test may run alongside others.
    parallel: bool,
    /// `serial`: the test runs alone (the default); exclusive with `parallel`.
    serial: bool,
    /// `resources = [..]` / `resource = ".."`: locks the test holds while running.
    resources: Vec<String>,
    /// `tags = [..]`: labels for filtering.
    tags: Vec<String>,
    /// `timeout_ms = N`: per-test timeout in milliseconds.
    timeout_ms: Option<u64>,
}

/// Reads a string literal or an array of string literals.
fn strings(e: &Expr) -> syn::Result<Vec<String>> {
    match e {
        Expr::Array(ExprArray { elems, .. }) => elems.iter().map(string).collect(),
        other => Ok(vec![string(other)?]),
    }
}

/// Reads a string literal, rejecting any other expression.
fn string(e: &Expr) -> syn::Result<String> {
    match e {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => Ok(s.value()),
        other => Err(syn::Error::new_spanned(other, "expected a string literal")),
    }
}

/// Parses `key = value` / flag arguments, accepting only the keys in `allowed`.
///
/// Keys may repeat; list arguments accumulate and the last scalar wins.
fn parse_args(attr: TokenStream, allowed: &[&str]) -> syn::Result<Args> {
    let mut a = Args::default();
    let parser = syn::meta::parser(|meta| {
        let key = meta
            .path
            .get_ident()
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        if !allowed.contains(&key.as_str()) {
            return Err(meta.error(format!(
                "unsupported argument `{key}` (supported: {})",
                allowed.join(", ")
            )));
        }
        match key.as_str() {
            "parallel" => a.parallel = true,
            "serial" => a.serial = true,
            "name" => a.name = Some(string(&meta.value()?.parse()?)?),
            "suite" => a.suite = Some(string(&meta.value()?.parse()?)?),
            "scope" => a.scope = Some(string(&meta.value()?.parse()?)?),
            "teardown" => a.teardown = Some(meta.value()?.parse()?),
            "resources" | "resource" => a.resources.extend(strings(&meta.value()?.parse()?)?),
            "tags" => a.tags.extend(strings(&meta.value()?.parse()?)?),
            "timeout_ms" => {
                let lit: syn::LitInt = meta.value()?.parse()?;
                a.timeout_ms = Some(lit.base10_parse()?);
            }
            _ => unreachable!(),
        }

        Ok(())
    });
    parser.parse(attr)?;

    if a.parallel && a.serial {
        return Err(syn::Error::new(
            Span::call_site(),
            "a test is either `parallel` or `serial`",
        ));
    }

    Ok(a)
}

/// Tokens for an `Option<&'static str>` literal.
fn opt_str(s: Option<&str>) -> TokenStream2 {
    if let Some(v) = s {
        quote!(::core::option::Option::Some(#v))
    } else {
        quote!(::core::option::Option::None)
    }
}

/// How a parameter of an annotated function is supplied.
enum Param {
    /// `cx: Ctx` (any name): receives the context.
    Ctx,
    /// Anything else: a fixture looked up by the parameter name.
    Fixture(syn::Ident, Box<Type>),
}

/// Whether `ty` names `Ctx` (by any path, by value or by reference).
fn is_ctx_type(ty: &Type) -> bool {
    match ty {
        Type::Path(p) => p.path.segments.last().is_some_and(|s| s.ident == "Ctx"),
        Type::Reference(r) => is_ctx_type(&r.elem),
        _ => false,
    }
}

/// Classifies the parameters of `async fn f(a: A, cx: Ctx, b: B)`.
fn params(f: &ItemFn) -> syn::Result<Vec<Param>> {
    if f.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            f.sig.fn_token,
            "apitest items must be `async fn`",
        ));
    }

    f.sig
        .inputs
        .iter()
        .map(|p| match p {
            FnArg::Typed(t) if is_ctx_type(&t.ty) => {
                if matches!(&*t.ty, Type::Reference(_)) {
                    Err(syn::Error::new_spanned(
                        &t.ty,
                        "take `Ctx` by value (it is a cheap handle)",
                    ))
                } else {
                    Ok(Param::Ctx)
                }
            }
            FnArg::Typed(t) => match &*t.pat {
                Pat::Ident(id) => Ok(Param::Fixture(id.ident.clone(), t.ty.clone())),
                other => Err(syn::Error::new_spanned(
                    other,
                    "fixture parameters must be plain identifiers",
                )),
            },
            FnArg::Receiver(r) => Err(syn::Error::new_spanned(r, "methods are not supported")),
        })
        .collect()
}

/// Generates `fn wrapper(cx: Ctx) -> BoxFut<_>` that resolves fixtures and calls the user fn.
fn wrapper(
    f: &ItemFn,
    wrapper_name: &syn::Ident,
    output: &TokenStream2,
    map_result: &TokenStream2,
) -> syn::Result<TokenStream2> {
    let params = params(f)?;
    let ident = &f.sig.ident;
    let mut resolve = vec![];
    let mut args = vec![];
    for (i, p) in params.iter().enumerate() {
        match p {
            Param::Ctx => args.push(quote!(cx.clone())),
            Param::Fixture(name, ty) => {
                let key = name.to_string();
                let var = format_ident!("__fixture_{}", i);
                resolve.push(quote!(let #var: #ty = cx.get_as::<#ty>(#key)?;));
                args.push(quote!(#var));
            }
        }
    }

    Ok(quote! {
        #[doc(hidden)]
        fn #wrapper_name(cx: ::apitest::Ctx) -> ::apitest::suite::BoxFut<#output> {
            ::std::boxed::Box::pin(async move {
                #(#resolve)*
                let out = #ident(#(#args),*).await;
                #map_result
            })
        }
    })
}

/// Whether `f` declares no return type.
fn returns_unit(f: &ItemFn) -> bool {
    matches!(f.sig.output, ReturnType::Default)
}

/// Registers an async test.
///
/// Not compiled here: the expansion refers to `apitest`, which this proc-macro crate
/// cannot depend on. The `apitest` integration tests exercise it.
///
/// ```ignore
/// #[apitest::test(parallel, resources = ["invoices"], timeout_ms = 5000)]
/// async fn create_invoice(cx: Ctx, token: String) -> Result<()> { ... }
/// ```
///
/// Arguments: `parallel` | `serial` (default), `resources = [..]`, `timeout_ms = N`,
/// `tags = [..]`, `name = "display name"`, `suite = "suite key"` (default: module path).
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    match expand_test(attr, &f) {
        Ok(t) => t.into(),
        Err(e) => {
            let err = e.to_compile_error();
            quote!(#f #err).into()
        }
    }
}

/// Expands `#[apitest::test]`: the user fn under a hidden name, its wrapper, a
/// tooling marker under the original name, the options fn and the registration.
fn expand_test(attr: TokenStream, f: &ItemFn) -> syn::Result<TokenStream2> {
    let a = parse_args(
        attr,
        &[
            "parallel",
            "serial",
            "resources",
            "resource",
            "tags",
            "timeout_ms",
            "name",
            "suite",
        ],
    )?;
    let ident = &f.sig.ident;
    let fn_name = ident.to_string();
    let body_fn = format_ident!("__apitest_test_{}", ident);
    let opts_fn = format_ident!("__apitest_opts_{}", ident);
    let stat = format_ident!("__APITEST_TEST_{}", ident.to_string().to_uppercase());

    // The user's function is kept under a hidden name; its original name becomes a
    // `#[test]` marker that only exists for tooling (see below).
    let mut imp = f.clone();
    imp.sig.ident = format_ident!("__apitest_impl_{}", ident);
    let wrap = wrapper(&imp, &body_fn, &quote!(()), &test_result_map(f))?;
    let marker_msg = format!(
        "`{fn_name}` is an #[apitest::test]; it runs through apitest::harness::main in a `harness = false` test target, not through libtest"
    );
    let opts = test_opts_fn(&a, &opts_fn);
    let (name, suite) = (opt_str(a.name.as_deref()), opt_str(a.suite.as_deref()));

    Ok(quote! {
        #imp
        #wrap
        // Marker for rust-analyzer & co.: they recognise tests by a `#[test]`-like
        // attribute on the expanded item, and run them as
        // `cargo test --test <target> -- <module path>::<name> --exact`, which the
        // apitest harness resolves to this test. `harness = false` targets are not
        // compiled with `--test`, so the marker is removed there; in a regular
        // libtest target it fails loudly instead of passing silently.
        #[cfg(test)]
        #[::core::prelude::v1::test]
        #[doc(hidden)]
        fn #ident() {
            ::core::panic!(#marker_msg);
        }
        #opts
        #[::apitest::__private::linkme::distributed_slice(::apitest::__private::TESTS)]
        #[linkme(crate = ::apitest::__private::linkme)]
        #[doc(hidden)]
        static #stat: ::apitest::__private::TestDef = ::apitest::__private::TestDef {
            module: ::core::module_path!(),
            fn_name: #fn_name,
            name: #name,
            suite: #suite,
            file: ::core::file!(),
            line: ::core::line!(),
            opts: #opts_fn,
            body: #body_fn,
        };
    })
}

/// Tokens that turn the test fn's output `out` into `anyhow::Result<()>`:
/// `()` passes, any `Result` has its error converted.
fn test_result_map(f: &ItemFn) -> TokenStream2 {
    if returns_unit(f) {
        quote!({
            let () = out;
            ::core::result::Result::Ok(())
        })
    } else {
        quote!(::core::result::Result::map_err(
            out,
            ::core::convert::Into::<::apitest::__private::anyhow::Error>::into
        ))
    }
}

/// Generates `fn #opts_fn() -> TestOpts` from the test's attribute arguments.
fn test_opts_fn(a: &Args, opts_fn: &syn::Ident) -> TokenStream2 {
    let concurrency = if a.parallel {
        quote!(::apitest::Concurrency::Parallel)
    } else {
        quote!(::apitest::Concurrency::Serial)
    };
    let resources = &a.resources;
    let tags = &a.tags;
    let timeout = if let Some(ms) = a.timeout_ms {
        quote!(::core::option::Option::Some(::std::time::Duration::from_millis(#ms)))
    } else {
        quote!(::core::option::Option::None)
    };

    quote! {
        #[doc(hidden)]
        fn #opts_fn() -> ::apitest::TestOpts {
            let mut o = ::apitest::TestOpts::default();
            o.concurrency = #concurrency;
            o.resources = ::std::vec![#(::std::string::String::from(#resources)),*];
            o.tags = ::std::vec![#(::std::string::String::from(#tags)),*];
            o.timeout = #timeout;
            o
        }
    }
}

/// Registers an async fixture. Its return value (any `Serialize`) is stored
/// under the function name and injected into tests by parameter name.
///
/// Not compiled here: the expansion refers to `apitest`, which this proc-macro crate
/// cannot depend on. The `apitest` integration tests exercise it.
///
/// ```ignore
/// #[apitest::fixture(scope = "suite", teardown = logout)]
/// async fn token(cx: Ctx) -> Result<String> { ... }
///
/// async fn logout(cx: Ctx) -> Result<()> { ... }
/// ```
///
/// Arguments: `scope = "suite" | "vu" | "test"` (default `test`), `teardown = path`,
/// `name = "..."`, `suite = "..."`. Fixtures may take other fixtures as parameters.
#[proc_macro_attribute]
pub fn fixture(attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    match expand_fixture(attr, &f) {
        Ok(t) => t.into(),
        Err(e) => {
            let err = e.to_compile_error();
            quote!(#f #err).into()
        }
    }
}

/// Expands `#[apitest::fixture]`: the unchanged user fn, its setup wrapper, an
/// optional teardown wrapper and the registration.
fn expand_fixture(attr: TokenStream, f: &ItemFn) -> syn::Result<TokenStream2> {
    let a = parse_args(attr, &["scope", "teardown", "name", "suite"])?;
    let ident = &f.sig.ident;
    let fname = a.name.clone().unwrap_or_else(|| ident.to_string());
    let scope = a.scope.clone().unwrap_or_else(|| "test".into());
    if !["suite", "vu", "test"].contains(&scope.as_str()) {
        return Err(syn::Error::new(
            Span::call_site(),
            "scope must be \"suite\", \"vu\" or \"test\"",
        ));
    }

    let setup_fn = format_ident!("__apitest_fixture_{}", ident);
    let stat = format_ident!("__APITEST_FIXTURE_{}", ident.to_string().to_uppercase());
    let map = quote! {
        let v = ::core::result::Result::map_err(out, ::core::convert::Into::<::apitest::__private::anyhow::Error>::into)?;
        ::core::result::Result::Ok(::apitest::__private::serde_json::to_value(v)?)
    };
    let wrap = wrapper(
        f,
        &setup_fn,
        &quote!(::apitest::__private::serde_json::Value),
        &map,
    )?;
    let (teardown_fn, teardown_field) = match &a.teardown {
        Some(path) => {
            let td = format_ident!("__apitest_teardown_{}", ident);
            (
                quote! {
                    #[doc(hidden)]
                    fn #td(cx: ::apitest::Ctx) -> ::apitest::suite::BoxFut<()> {
                        ::std::boxed::Box::pin(async move {
                            ::core::result::Result::map_err(#path(cx).await, ::core::convert::Into::<::apitest::__private::anyhow::Error>::into)
                        })
                    }
                },
                quote!(::core::option::Option::Some(#td)),
            )
        }
        None => (quote!(), quote!(::core::option::Option::None)),
    };
    let suite = opt_str(a.suite.as_deref());

    Ok(quote! {
        #f
        #wrap
        #teardown_fn
        #[::apitest::__private::linkme::distributed_slice(::apitest::__private::FIXTURES)]
        #[linkme(crate = ::apitest::__private::linkme)]
        #[doc(hidden)]
        static #stat: ::apitest::__private::FixtureDef = ::apitest::__private::FixtureDef {
            module: ::core::module_path!(),
            name: #fname,
            suite: #suite,
            scope: #scope,
            file: ::core::file!(),
            line: ::core::line!(),
            setup: #setup_fn,
            teardown: #teardown_field,
        };
    })
}

/// Declares the base `apitest::Suite` for a module: contracts, hooks, extra builder-style tests.
///
/// Not compiled here: the expansion refers to `apitest`, which this proc-macro crate
/// cannot depend on. The `apitest` integration tests exercise it.
///
/// ```ignore
/// #[apitest::suite]
/// fn billing() -> anyhow::Result<Suite> {
///     Suite::new("billing").pinned("billing", "specs/billing-1.0.yaml")
/// }
/// ```
///
/// Arguments: `suite = "..."` (default: module path).
#[proc_macro_attribute]
pub fn suite(attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    let a = match parse_args(attr, &["suite"]) {
        Ok(a) => a,
        Err(e) => {
            let err = e.to_compile_error();
            return quote!(#f #err).into();
        }
    };
    if f.sig.asyncness.is_some() || !f.sig.inputs.is_empty() {
        let err = syn::Error::new_spanned(
            &f.sig,
            "#[apitest::suite] expects `fn name() -> Result<Suite>`",
        )
        .to_compile_error();
        return quote!(#f #err).into();
    }

    let ident = &f.sig.ident;
    let stat = format_ident!("__APITEST_SUITE_{}", ident.to_string().to_uppercase());
    let suite = opt_str(a.suite.as_deref());
    quote! {
        #f
        #[::apitest::__private::linkme::distributed_slice(::apitest::__private::SUITES)]
        #[linkme(crate = ::apitest::__private::linkme)]
        #[doc(hidden)]
        static #stat: ::apitest::__private::SuiteDef = ::apitest::__private::SuiteDef {
            module: ::core::module_path!(),
            suite: #suite,
            build: || ::core::result::Result::map_err(#ident(), ::core::convert::Into::<::apitest::__private::anyhow::Error>::into),
        };
    }
    .into()
}
