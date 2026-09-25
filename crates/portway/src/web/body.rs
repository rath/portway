//! The console's response body: a buffer, or a stream of server-sent events
//! fed by a task that ends when the page goes away.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::sync::mpsc;

pub enum WebBody {
    Full(Option<Bytes>),
    Stream(mpsc::Receiver<Bytes>),
}

impl WebBody {
    pub fn full(bytes: impl Into<Bytes>) -> Self {
        let bytes = bytes.into();
        WebBody::Full((!bytes.is_empty()).then_some(bytes))
    }

    pub fn empty() -> Self {
        WebBody::Full(None)
    }
}

impl Body for WebBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            WebBody::Full(slot) => Poll::Ready(slot.take().map(|bytes| Ok(Frame::data(bytes)))),
            WebBody::Stream(receiver) => receiver
                .poll_recv(cx)
                .map(|chunk| chunk.map(|bytes| Ok(Frame::data(bytes)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, WebBody::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            WebBody::Full(slot) => {
                SizeHint::with_exact(slot.as_ref().map_or(0, |b| b.len() as u64))
            }
            WebBody::Stream(_) => SizeHint::default(),
        }
    }
}
