//! Request and response body types.
//!
//! Both are small enums so the common case (a fully buffered body) never
//! touches a trait object or an extra allocation.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};
use http_body_util::BodyExt;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

type BoxBody = Pin<Box<dyn http_body::Body<Data = Bytes, Error = BoxError> + Send>>;

/// Outgoing response body.
#[derive(Default)]
pub enum Body {
    #[default]
    Empty,
    Full(Option<Bytes>),
    Stream(BoxBody),
}

impl Body {
    #[inline]
    pub fn empty() -> Self {
        Body::Empty
    }

    /// Wrap any `http_body::Body` (e.g. a stream adapter) as a response body.
    pub fn stream<B>(body: B) -> Self
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        Body::Stream(Box::pin(body.map_err(Into::into)))
    }
}

impl From<Bytes> for Body {
    #[inline]
    fn from(b: Bytes) -> Self {
        Body::Full(Some(b))
    }
}
impl From<&'static str> for Body {
    #[inline]
    fn from(s: &'static str) -> Self {
        Body::Full(Some(Bytes::from_static(s.as_bytes())))
    }
}
impl From<&'static [u8]> for Body {
    #[inline]
    fn from(s: &'static [u8]) -> Self {
        Body::Full(Some(Bytes::from_static(s)))
    }
}
impl From<String> for Body {
    #[inline]
    fn from(s: String) -> Self {
        Body::Full(Some(Bytes::from(s)))
    }
}
impl From<Vec<u8>> for Body {
    #[inline]
    fn from(v: Vec<u8>) -> Self {
        Body::Full(Some(Bytes::from(v)))
    }
}

impl http_body::Body for Body {
    type Data = Bytes;
    type Error = BoxError;

    #[inline]
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match self.get_mut() {
            Body::Empty => Poll::Ready(None),
            Body::Full(b) => Poll::Ready(b.take().map(|b| Ok(Frame::data(b)))),
            Body::Stream(s) => s.as_mut().poll_frame(cx),
        }
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        match self {
            Body::Empty => true,
            Body::Full(b) => b.is_none(),
            Body::Stream(s) => s.is_end_stream(),
        }
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        match self {
            Body::Empty => SizeHint::with_exact(0),
            Body::Full(Some(b)) => SizeHint::with_exact(b.len() as u64),
            Body::Full(None) => SizeHint::with_exact(0),
            Body::Stream(s) => s.size_hint(),
        }
    }
}

/// Incoming request body. `Full` is used by the in-process test client.
pub enum ReqBody {
    Incoming(hyper::body::Incoming),
    Full(Bytes),
}

impl ReqBody {
    /// Buffer the whole body, refusing anything larger than `limit` bytes.
    pub async fn collect_limited(self, limit: usize) -> Result<Bytes, crate::Error> {
        match self {
            ReqBody::Full(b) => {
                if b.len() > limit {
                    return Err(crate::Error::payload_too_large());
                }
                Ok(b)
            }
            ReqBody::Incoming(inc) => {
                if let Some(upper) = http_body::Body::size_hint(&inc).upper() {
                    if upper > limit as u64 {
                        return Err(crate::Error::payload_too_large());
                    }
                }
                let limited = http_body_util::Limited::new(inc, limit);
                match limited.collect().await {
                    Ok(c) => Ok(c.to_bytes()),
                    Err(e) if e.is::<http_body_util::LengthLimitError>() => {
                        Err(crate::Error::payload_too_large())
                    }
                    Err(e) => Err(crate::Error::bad_request(e.to_string())),
                }
            }
        }
    }
}
