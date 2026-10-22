//! Support code for zapline's procedural macros. Not public API.
//!
//! The route macros document handlers by probing their argument and return
//! types with *autoref specialization*: `(&&Probe::<T>::new()).method(doc)`
//! resolves to the `ViaInput`/`ViaOutput` impl when `T` implements
//! `ApiInput`/`ApiOutput`, and to the no-op fallback otherwise. Without the
//! `openapi` feature everything here is a no-op, so expansions still compile.

use std::marker::PhantomData;

use http::Extensions;

/// A zero-sized stand-in for a value of type `T`.
pub struct Probe<T>(PhantomData<fn() -> T>);

impl<T> Probe<T> {
    #[inline]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Probe(PhantomData)
    }
}

/// Stands in for a generic argument the macros cannot name, so wrappers
/// such as `Sse<impl Stream<..>>` can still be probed as `Sse<Opaque>`.
#[derive(Clone, Copy, Debug)]
pub struct Opaque;

#[cfg(feature = "openapi")]
pub use crate::openapi::OperationDoc as Doc;

// Crates `#[model]` derives from, so apps don't need them as dependencies.
#[cfg(feature = "openapi")]
pub use schemars;
pub use serde;
#[cfg(feature = "validate")]
pub use validator;

/// Placeholder when the `openapi` feature is disabled.
#[cfg(not(feature = "openapi"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct Doc;

pub mod describe {
    use super::{Doc, Probe};

    /// Picked when `T: ApiInput`.
    pub trait ViaInput {
        fn zapline_describe_input(&self, doc: &mut Doc);
    }

    /// Picked when `T: ApiOutput`.
    pub trait ViaOutput {
        fn zapline_describe_output(&self, doc: &mut Doc);
    }

    /// Fallback for types that don't document themselves.
    pub trait InputFallback {
        #[inline]
        fn zapline_describe_input(&self, _doc: &mut Doc) {}
    }

    /// Fallback for types that don't document themselves.
    pub trait OutputFallback {
        #[inline]
        fn zapline_describe_output(&self, _doc: &mut Doc) {}
    }

    impl<T> InputFallback for Probe<T> {}
    impl<T> OutputFallback for Probe<T> {}

    #[cfg(feature = "openapi")]
    impl<T: crate::openapi::ApiInput> ViaInput for &Probe<T> {
        #[inline]
        fn zapline_describe_input(&self, doc: &mut Doc) {
            T::describe_input(doc);
        }
    }

    #[cfg(feature = "openapi")]
    impl<T: crate::openapi::ApiOutput> ViaOutput for &Probe<T> {
        #[inline]
        fn zapline_describe_output(&self, doc: &mut Doc) {
            T::describe_output(doc);
        }
    }
}

/// The metadata stored with each route registered by a macro.
#[inline]
pub fn into_meta(doc: &Doc) -> Extensions {
    #[allow(unused_mut)]
    let mut meta = Extensions::new();
    #[cfg(feature = "openapi")]
    meta.insert(doc.clone());
    #[cfg(not(feature = "openapi"))]
    let _ = doc;
    meta
}

#[cfg(feature = "openapi")]
mod imp {
    use super::Doc;

    pub fn doc(handler: &'static str) -> Doc {
        Doc {
            handler: Some(handler),
            ..Doc::default()
        }
    }
    pub fn module(doc: &mut Doc, module: &'static str) {
        doc.module = Some(module);
    }
    pub fn summary(doc: &mut Doc, s: &'static str) {
        doc.summary(s);
    }
    pub fn description(doc: &mut Doc, s: &'static str) {
        doc.description(s);
    }
    pub fn tag(doc: &mut Doc, s: &'static str) {
        doc.tag(s);
    }
    pub fn operation_id(doc: &mut Doc, s: &'static str) {
        doc.operation_id(s);
    }
    pub fn status(doc: &mut Doc, status: u16) {
        doc.success_status(status);
    }
    pub fn deprecated(doc: &mut Doc) {
        doc.deprecated();
    }
    pub fn hidden(doc: &mut Doc) {
        doc.hidden();
    }
    pub fn security(doc: &mut Doc, s: &'static str) {
        doc.security(s);
    }
    pub fn not_found(doc: &mut Doc) {
        doc.error_response(404, "Not Found");
    }
    pub fn error_response(doc: &mut Doc) {
        doc.default_error();
    }
    pub fn validation_error(doc: &mut Doc) {
        doc.error_response(422, "Validation Error");
    }
    pub fn optional_input(doc: &mut Doc, f: impl FnOnce(&mut Doc)) {
        let mark = doc.input_mark();
        f(doc);
        doc.make_optional_since(mark);
    }
}

#[cfg(not(feature = "openapi"))]
mod imp {
    use super::Doc;

    #[inline]
    pub fn doc(_: &'static str) -> Doc {
        Doc
    }
    #[inline]
    pub fn module(_: &mut Doc, _: &'static str) {}
    #[inline]
    pub fn summary(_: &mut Doc, _: &'static str) {}
    #[inline]
    pub fn description(_: &mut Doc, _: &'static str) {}
    #[inline]
    pub fn tag(_: &mut Doc, _: &'static str) {}
    #[inline]
    pub fn operation_id(_: &mut Doc, _: &'static str) {}
    #[inline]
    pub fn status(_: &mut Doc, _: u16) {}
    #[inline]
    pub fn deprecated(_: &mut Doc) {}
    #[inline]
    pub fn hidden(_: &mut Doc) {}
    #[inline]
    pub fn security(_: &mut Doc, _: &'static str) {}
    #[inline]
    pub fn not_found(_: &mut Doc) {}
    #[inline]
    pub fn error_response(_: &mut Doc) {}
    #[inline]
    pub fn validation_error(_: &mut Doc) {}
    #[inline]
    pub fn optional_input(doc: &mut Doc, f: impl FnOnce(&mut Doc)) {
        f(doc);
    }
}

pub use imp::*;
