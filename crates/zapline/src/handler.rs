//! Turning plain `async fn`s into type-erased endpoints.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use crate::extract::{FromRequest, FromRequestParts};
use crate::request::Request;
use crate::response::{IntoResponse, Response};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Implemented for every `async fn(A, B, ..., Last) -> impl IntoResponse`
/// with up to 12 arguments, where all but the last are `FromRequestParts`.
pub trait Handler<T>: Clone + Send + Sync + 'static {
    fn call(&self, req: Request) -> BoxFuture<Response>;
}

impl<F, Fut, R> Handler<((),)> for F
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoResponse,
{
    #[inline]
    fn call(&self, _req: Request) -> BoxFuture<Response> {
        let fut = self();
        Box::pin(async move { fut.await.into_response() })
    }
}

macro_rules! impl_handler {
    ([$($ty:ident),*], $last:ident) => {
        #[allow(non_snake_case, unused_mut)]
        impl<F, Fut, R, M, $($ty,)* $last> Handler<(M, $($ty,)* $last,)> for F
        where
            F: Fn($($ty,)* $last) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
            $($ty: FromRequestParts + 'static,)*
            $last: FromRequest<M> + 'static,
            M: 'static,
        {
            #[inline]
            fn call(&self, mut req: Request) -> BoxFuture<Response> {
                let f = self.clone();
                Box::pin(async move {
                    $(
                        let $ty = match $ty::from_request_parts(&mut req).await {
                            Ok(v) => v,
                            Err(e) => return e.into_response(),
                        };
                    )*
                    let $last = match $last::from_request(req).await {
                        Ok(v) => v,
                        Err(e) => return e.into_response(),
                    };
                    f($($ty,)* $last).await.into_response()
                })
            }
        }
    };
}

impl_handler!([], T1);
impl_handler!([T1], T2);
impl_handler!([T1, T2], T3);
impl_handler!([T1, T2, T3], T4);
impl_handler!([T1, T2, T3, T4], T5);
impl_handler!([T1, T2, T3, T4, T5], T6);
impl_handler!([T1, T2, T3, T4, T5, T6], T7);
impl_handler!([T1, T2, T3, T4, T5, T6, T7], T8);
impl_handler!([T1, T2, T3, T4, T5, T6, T7, T8], T9);
impl_handler!([T1, T2, T3, T4, T5, T6, T7, T8, T9], T10);
impl_handler!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10], T11);
impl_handler!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11], T12);

/// Object-safe endpoint. One dynamic call per request.
pub trait Endpoint: Send + Sync + 'static {
    fn call(&self, req: Request) -> BoxFuture<Response>;
}

pub(crate) type BoxEndpoint = Arc<dyn Endpoint>;

struct HandlerEndpoint<H, T> {
    handler: H,
    _marker: PhantomData<fn() -> T>,
}

impl<H: Handler<T>, T: 'static> Endpoint for HandlerEndpoint<H, T> {
    #[inline]
    fn call(&self, req: Request) -> BoxFuture<Response> {
        self.handler.call(req)
    }
}

pub(crate) fn boxed<H: Handler<T>, T: 'static>(handler: H) -> BoxEndpoint {
    Arc::new(HandlerEndpoint {
        handler,
        _marker: PhantomData,
    })
}
