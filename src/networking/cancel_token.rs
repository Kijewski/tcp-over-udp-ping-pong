use std::convert::Infallible;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use flume::{Receiver, Sender, bounded};

pub fn make_cancel_token() -> CancelToken {
    let (cancel_tx, cancel_rx) = bounded(1);
    let cancel_tx = CancelSender(Arc::new(Mutex::new(Some(cancel_tx))));
    let cancel_rx = CancelReceiver(Arc::new(cancel_rx));
    (cancel_tx, cancel_rx)
}

pub type CancelToken = (CancelSender, CancelReceiver);

#[derive(Debug, Clone)]
pub struct CancelSender(Arc<Mutex<Option<Sender<Infallible>>>>);

impl CancelSender {
    pub fn cancel(&self) {
        // drop `sender` only after releasing the lock
        let sender = self.0.lock().unwrap().take();
        drop(sender);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }
}

impl Drop for CancelSender {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug, Clone)]
pub struct CancelReceiver(Arc<Receiver<Infallible>>);

impl Future for CancelReceiver {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match pin!(self.0.recv_async()).poll(cx) {
            Poll::Ready(_) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }
}
