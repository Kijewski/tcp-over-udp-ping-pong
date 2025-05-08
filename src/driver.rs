use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex as SyncMutex};
use std::task::{Context, Waker};

use embassy_net::driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use flume::{Receiver, Sender, TryRecvError, bounded};
use tokio::net::UdpSocket;
use tokio::{select, spawn};

#[derive(Debug)]
pub struct UdpDriver {
    queue_in: (Receiver<Box<[u8]>>, Arc<SyncMutex<VecDeque<Waker>>>),
    queue_out: Sender<Box<[u8]>>,
    cancel_token: Option<(CancelSender, Arc<Receiver<Infallible>>)>,
}

#[derive(Debug, Clone)]
pub struct CancelSender(Arc<SyncMutex<Option<Sender<Infallible>>>>);

impl CancelSender {
    pub fn cancel(&self) {
        let _ = self.0.lock().unwrap().take();
    }
}

impl Drop for CancelSender {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl UdpDriver {
    pub fn new(udp: Arc<UdpSocket>) -> Self {
        let cancel_token = make_cancel_token();

        let (in_tx, in_rx) = bounded(16);
        let waker_queue = Arc::new(SyncMutex::new(VecDeque::new()));
        spawn_recv_loop(
            Arc::clone(&udp),
            cancel_token.clone(),
            in_tx,
            Arc::clone(&waker_queue),
        );

        let (out_tx, out_rx) = bounded(16);
        spawn_send_loop(udp, cancel_token.clone(), out_rx);

        Self {
            queue_in: (in_rx, waker_queue),
            queue_out: out_tx,
            cancel_token: Some(cancel_token),
        }
    }

    pub fn cancel_token(&self) -> Option<(CancelSender, Arc<Receiver<Infallible>>)> {
        self.cancel_token.clone()
    }
}

fn make_cancel_token() -> (CancelSender, Arc<Receiver<Infallible>>) {
    let (cancel_tx, cancel_rx) = bounded(1);
    let cancel_tx = CancelSender(Arc::new(SyncMutex::new(Some(cancel_tx))));
    let cancel_rx = Arc::new(cancel_rx);
    (cancel_tx, cancel_rx)
}

fn spawn_recv_loop(
    udp: Arc<UdpSocket>,
    cancel_token: (CancelSender, Arc<Receiver<Infallible>>),
    bytes_queue: Sender<Box<[u8]>>,
    waker_queue: Arc<SyncMutex<VecDeque<Waker>>>,
) {
    spawn_cancellable(cancel_token, async move {
        let mut buf = vec![0; 1500];
        loop {
            let len = match udp.recv(buf.as_mut_slice()).await {
                Ok(len) => len,
                Err(err) => {
                    dbg!(err);
                    break;
                }
            };
            if let Err(err) = bytes_queue.send_async(buf[..len].into()).await {
                dbg!(err);
                break;
            }
            if let Some(waker) = waker_queue.lock().unwrap().pop_front() {
                waker.wake();
            }
        }
    });
}

fn spawn_send_loop(
    udp: Arc<UdpSocket>,
    cancel_token: (CancelSender, Arc<Receiver<Infallible>>),
    queue_out: Receiver<Box<[u8]>>,
) {
    spawn_cancellable(cancel_token, async move {
        loop {
            let bytes = match queue_out.recv_async().await {
                Ok(bytes) => bytes,
                Err(err) => {
                    dbg!(err);
                    break;
                }
            };
            if let Err(err) = udp.send(&bytes).await {
                dbg!(err);
                break;
            }
        }
    });
}

fn spawn_cancellable<F: Future<Output: Send> + Send + 'static>(
    (cancel_tx, cancel_rx): (CancelSender, Arc<Receiver<Infallible>>),
    fut: F,
) {
    spawn(async move {
        let _cancel_tx = cancel_tx;
        select! {
            biased;
            _ = cancel_rx.recv_async() => (),
            _ = fut => (),
        }
    });
}

const _: () = {
    impl Driver for UdpDriver {
        type RxToken<'a>
            = UdpRxToken
        where
            Self: 'a;

        type TxToken<'a>
            = UdpTxToken<'a>
        where
            Self: 'a;

        fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            let mut waker_queue = self.queue_in.1.lock().unwrap();
            match self.queue_in.0.try_recv() {
                Ok(bytes) => Some((UdpRxToken { bytes }, UdpTxToken { driver: self })),
                Err(TryRecvError::Disconnected) => unimplemented!(),
                Err(TryRecvError::Empty) => {
                    waker_queue.push_back(cx.waker().clone());
                    None
                }
            }
        }

        fn transmit(&mut self, _: &mut Context) -> Option<Self::TxToken<'_>> {
            Some(UdpTxToken { driver: self })
        }

        fn link_state(&mut self, _: &mut Context) -> LinkState {
            LinkState::Up
        }

        fn capabilities(&self) -> Capabilities {
            let mut caps = Capabilities::default();
            caps.max_transmission_unit = 1200;
            caps
        }

        fn hardware_address(&self) -> HardwareAddress {
            HardwareAddress::Ip
        }
    }

    #[derive(Debug)]
    pub struct UdpRxToken {
        bytes: Box<[u8]>,
    }

    impl RxToken for UdpRxToken {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(mut self, f: F) -> R {
            f(&mut self.bytes)
        }
    }

    #[derive(Debug)]
    pub struct UdpTxToken<'a> {
        driver: &'a UdpDriver,
    }

    impl TxToken for UdpTxToken<'_> {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            let mut bytes = vec![0; len].into_boxed_slice();
            let result = f(&mut bytes);
            self.driver.queue_out.send(bytes).unwrap();
            result
        }
    }
};
