use std::collections::VecDeque;
use std::sync::{Arc, Mutex as SyncMutex};
use std::task::{Context, Waker};

use crc::{CRC_64_ECMA_182, Crc};
use embassy_net::driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use flume::{Receiver, Sender, TryRecvError, bounded};
use tokio::net::UdpSocket;
use tokio::{select, spawn};

use super::cancel_token::{CancelSender, CancelToken};

#[derive(Debug)]
pub struct UdpDriver {
    queue_in: (Receiver<Box<[u8]>>, Arc<SyncMutex<VecDeque<Waker>>>),
    queue_out: Sender<Box<[u8]>>,
    cancel_tx: Option<CancelSender>,
}

impl UdpDriver {
    pub fn new(udp: Arc<UdpSocket>, cancel_token: CancelToken) -> Self {
        let (in_tx, in_rx) = bounded(16);
        let waker_queue = Arc::new(SyncMutex::new(VecDeque::new()));
        spawn_recv_loop(
            Arc::clone(&udp),
            cancel_token.clone(),
            in_tx,
            Arc::clone(&waker_queue),
        );

        let (cancel_tx, cancel_rx) = cancel_token;
        let (out_tx, out_rx) = bounded(16);
        spawn_send_loop(udp, (cancel_tx.clone(), cancel_rx), out_rx);

        Self {
            queue_in: (in_rx, waker_queue),
            queue_out: out_tx,
            cancel_tx: Some(cancel_tx),
        }
    }
}

fn spawn_recv_loop(
    udp: Arc<UdpSocket>,
    cancel_token: CancelToken,
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

            if len < size_of::<u64>() {
                continue;
            }
            let Some(buf) = buf.get(..len) else {
                continue;
            };
            let (front, back) = buf.split_at(size_of::<u64>());

            let crc = Crc::<u64>::new(&CRC_64_ECMA_182);
            let mut digest = crc.digest();
            digest.update(back);
            let crc = digest.finalize().to_le_bytes();
            if front != crc.as_slice() {
                continue;
            }

            if let Err(err) = bytes_queue.send_async(back.into()).await {
                dbg!(err);
                break;
            }
            if let Some(waker) = waker_queue.lock().unwrap().pop_front() {
                waker.wake();
            }
        }
    });
}

fn spawn_send_loop(udp: Arc<UdpSocket>, cancel_token: CancelToken, queue_out: Receiver<Box<[u8]>>) {
    spawn_cancellable(cancel_token, async move {
        let mut buf = vec![0; 1500];
        loop {
            let bytes = match queue_out.recv_async().await {
                Ok(bytes) => bytes,
                Err(err) => {
                    dbg!(err);
                    break;
                }
            };

            let Some(buf) = buf.get_mut(..bytes.len() + size_of::<u64>()) else {
                continue;
            };
            let (front, back) = buf.split_at_mut(size_of::<u64>());
            back.copy_from_slice(&bytes);

            let crc = Crc::<u64>::new(&CRC_64_ECMA_182);
            let mut digest = crc.digest();
            digest.update(&bytes);
            front.copy_from_slice(&digest.finalize().to_le_bytes());

            if let Err(err) = udp.send(buf).await {
                dbg!(err);
                break;
            }
        }
    });
}

fn spawn_cancellable<F: Future<Output: Send> + Send + 'static>(
    (cancel_tx, cancel_rx): CancelToken,
    fut: F,
) {
    spawn(async move {
        let _cancel_tx = cancel_tx;
        select! {
            biased;
            () = cancel_rx => (),
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
            if let Some(cancel_tx) = &self.cancel_tx {
                if !cancel_tx.is_cancelled() {
                    return LinkState::Up;
                }
            }
            LinkState::Down
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
            let _ = self.driver.queue_out.send(bytes);
            result
        }
    }
};
