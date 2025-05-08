use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use yamux::Stream;

pub fn split_stream(stream: Arc<Mutex<Stream>>) -> (RdStream, WrStream) {
    (RdStream(Arc::clone(&stream)), WrStream(stream))
}

#[derive(Debug, Clone)]
pub struct RdStream(Arc<Mutex<Stream>>);

impl AsyncRead for RdStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        mutex_poll_fn(&self.0, cx, |stream, cx| stream.poll_read(cx, buf))
    }
}

#[derive(Debug, Clone)]
pub struct WrStream(Arc<Mutex<Stream>>);

impl AsyncWrite for WrStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        mutex_poll_fn(&self.0, cx, |stream, cx| stream.poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        mutex_poll_fn(&self.0, cx, |stream, cx| stream.poll_flush(cx))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        mutex_poll_fn(&self.0, cx, |stream, cx| stream.poll_close(cx))
    }
}

pub fn mutex_poll_fn<'cx, P, D, F, R>(mutex: &P, cx: &mut Context<'cx>, fun: F) -> Poll<R>
where
    P: Pinner<Inner = Mutex<D>>,
    F: FnOnce(Pin<&mut D>, &mut Context<'cx>) -> Poll<R>,
{
    if let Poll::Ready(mut guard) = pin!(mutex.get_inner().lock()).poll(cx) {
        // SAFETY: the Pinner pinned the data down
        let data = unsafe { Pin::new_unchecked(&mut *guard) };
        if let Poll::Ready(result) = fun(data, cx) {
            return Poll::Ready(result);
        }
    }
    Poll::Pending
}

pub unsafe trait Pinner {
    type Inner;

    fn get_inner(&self) -> &Self::Inner;
}

unsafe impl<T> Pinner for Arc<T> {
    type Inner = T;

    #[inline]
    fn get_inner(&self) -> &Self::Inner {
        self
    }
}

unsafe impl<T> Pinner for Rc<T> {
    type Inner = T;

    #[inline]
    fn get_inner(&self) -> &Self::Inner {
        self
    }
}
