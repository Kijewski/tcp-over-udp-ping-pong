use std::fmt;
use std::future::poll_fn;
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::ops::ControlFlow;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config, IpEndpoint, IpListenEndpoint, Ipv4Cidr, Stack, StackResources, StaticConfigV4,
};
use flume::{Receiver, Sender, bounded};
use futures_io::{AsyncRead, AsyncWrite};
use futures_util::FutureExt;
use tokio::net::UdpSocket;
use tokio::runtime::Handle;
use tokio::select;
use tokio::sync::{Mutex, oneshot};
use tokio::task::{JoinHandle, spawn_blocking};
use tokio::time::timeout;
use yamux::{Connection, ConnectionError, Stream};

use super::cancel_token::{CancelSender, CancelToken, make_cancel_token};
use super::driver::UdpDriver;
use super::split_stream::mutex_poll_fn;
use crate::Kind;

pub struct NetworkConnection {
    task: JoinHandle<Result<(), Error>>,
    command_tx: Sender<Command>,
    cancel_tx: CancelSender,
}

impl NetworkConnection {
    pub async fn new(kind: Kind, udp: Arc<UdpSocket>) -> Result<(Self, StreamReceiver), Error> {
        let (cancel_tx, cancel_rx) = make_cancel_token();
        let (stream_tx, stream_rx) = bounded(1);
        let (command_tx, command_rx) = bounded(1);
        let (setup_tx, setup_rx) = oneshot::channel();
        let task = spawn_blocking({
            let cancel_token = (cancel_tx.clone(), cancel_rx);
            move || {
                Handle::current().block_on(run(
                    kind,
                    udp,
                    cancel_token,
                    stream_tx,
                    command_rx,
                    setup_tx,
                ))
            }
        });

        let result = setup_rx.await;
        match result {
            Ok(Ok(())) => {
                let conn = Self {
                    task,
                    command_tx,
                    cancel_tx,
                };
                Ok((conn, StreamReceiver(stream_rx)))
            }
            Ok(Err(err)) => Err(err),
            Err(_) => Err(task.await.unwrap().unwrap_err()),
        }
    }

    pub async fn close(&mut self) -> Result<(), Error> {
        let result = self.repl_command(Command::Close).await;
        self.cancel_tx.cancel();
        result
    }

    pub async fn open_stream(&mut self) -> Result<Stream, Error> {
        self.repl_command(Command::OpenStream).await
    }

    async fn repl_command<T, F>(&mut self, fun: F) -> Result<T, Error>
    where
        F: FnOnce(oneshot::Sender<T>) -> Command,
    {
        let (tx, rx) = oneshot::channel();
        if self.command_tx.send(fun(tx)).is_err() {
            return Err(task_result(&mut self.task).await);
        }
        select! {
            biased;
            result = rx => match result {
                Ok(result) => Ok(result),
                Err(_) => Err(task_result(&mut self.task).await),
            },
            result = &mut self.task => match result.unwrap() {
                Ok(()) => Err(InnerError::Disconnected(None).into()),
                Err(err) => Err(err),
            },
        }
    }
}

async fn task_result(task: &mut JoinHandle<Result<(), Error>>) -> Error {
    match timeout(Duration::from_millis(50), task).await {
        Ok(Ok(Ok(()))) => InnerError::Disconnected(None).into(),
        Ok(Ok(Err(err))) => err,
        Ok(Err(_)) => unreachable!(),
        Err(_) => InnerError::NoReason.into(),
    }
}

enum Command {
    OpenStream(oneshot::Sender<Stream>),
    Close(oneshot::Sender<()>),
}

#[derive(Debug, Clone)]
pub struct StreamReceiver(Receiver<Option<Result<Stream, Error>>>);

impl From<InnerError> for Error {
    fn from(err: InnerError) -> Self {
        Self(Arc::new(err))
    }
}

impl futures_util::Stream for StreamReceiver {
    type Item = Result<Stream, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Poll::Ready(result) = self.0.recv_async().poll_unpin(cx) else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        match result {
            Ok(item) => Poll::Ready(item),
            Err(_) => Poll::Ready(None),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Error(Arc<InnerError>);

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
enum InnerError {
    /// Could not determine error
    NoReason,
    /// Connection shut down immediately
    ImmediateShutdown(#[source] Option<ConnectionError>),
    /// Connection was shut down
    Disconnected(#[source] Option<ConnectionError>),
    /// Receiver disconnected [should never be seen]
    ReceiverDisconnected,
}

async fn run(
    kind: Kind,
    udp: Arc<UdpSocket>,
    cancel_token: CancelToken,
    stream_tx: Sender<Option<Result<Stream, Error>>>,
    command_rx: Receiver<Command>,
    setup_tx: oneshot::Sender<Result<(), Error>>,
) -> Result<(), Error> {
    let (_cancel_tx, cancel_rx) = cancel_token.clone();
    let driver = UdpDriver::new(udp, cancel_token);

    let config = Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Addr::LOCALHOST, 0),
        gateway: None,
        dns_servers: Default::default(),
    });
    let mut resources = Box::new(StackResources::<1>::new());
    let (stack, mut runner) = embassy_net::new(driver, config, &mut resources, 1);

    let execution =
        async { execution(kind, stream_tx, stack, command_rx, setup_tx).await }.shared();

    select! {
        _ = runner.run() => unreachable!(),
        result = execution.clone() => result,
        () = cancel_rx => {
            match timeout(Duration::from_millis(50), execution).await {
                Ok(result) => result,
                Err(_) => Err(InnerError::NoReason.into()),
            }
        }
    }
}

async fn execution(
    kind: Kind,
    stream_tx: Sender<Option<Result<Stream, Error>>>,
    stack: Stack<'_>,
    command_rx: Receiver<Command>,
    setup_tx: oneshot::Sender<Result<(), Error>>,
) -> Result<(), Error> {
    let mut rx_buffer = vec![0; 1 << 14];
    let mut tx_buffer = vec![0; 1 << 14];
    let mut sock = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    match kind {
        Kind::Client => {
            let remote_endpoint = IpEndpoint {
                addr: Ipv4Addr::LOCALHOST.into(),
                port: 12345,
            };
            sock.connect(remote_endpoint)
                .await
                .map_err(|_| InnerError::ImmediateShutdown(None))?;
        }
        Kind::Server => {
            let local_endpoint = IpListenEndpoint {
                addr: Some(Ipv4Addr::LOCALHOST.into()),
                port: 12345,
            };
            sock.accept(local_endpoint)
                .await
                .map_err(|_| InnerError::ImmediateShutdown(None))?;
        }
    }
    let mut cfg = yamux::Config::default();
    cfg.set_max_num_streams(16);
    cfg.set_split_send_size(1100);
    let mode = match kind {
        Kind::Client => yamux::Mode::Client,
        Kind::Server => yamux::Mode::Server,
    };
    let connection = Connection::new(AsyncRwSock(sock), cfg, mode);
    let connection = Rc::new(Mutex::new(connection));

    let poll_loop = async {
        let mut setup_tx = Some(setup_tx);
        loop {
            match poll_handler(&stream_tx, &connection, &mut setup_tx).await {
                ControlFlow::Continue(()) => (),
                ControlFlow::Break(result) => break result,
            }
        }
    };
    let command_loop = async {
        loop {
            let Ok(command) = command_rx.recv_async().await else {
                return Result::<(), Error>::Err(InnerError::ReceiverDisconnected.into());
            };
            match command_handler(&connection, command).await {
                ControlFlow::Continue(()) => (),
                ControlFlow::Break(result) => break result,
            }
        }
    };
    select! {
        result = poll_loop => result?,
        result = command_loop => result?,
    }
    Ok(())
}

async fn poll_handler(
    stream_tx: &Sender<Option<Result<Stream, Error>>>,
    connection: &Rc<Mutex<Connection<AsyncRwSock<'_>>>>,
    setup_tx: &mut Option<oneshot::Sender<Result<(), Error>>>,
) -> ControlFlow<Result<(), Error>> {
    let fut = poll_fn(|cx| {
        let mut setup_tx = setup_tx.take();
        mutex_poll_fn(connection, cx, |mut connection, cx| {
            let result = connection.poll_next_inbound(cx);
            let Some(setup_tx) = setup_tx.take() else {
                return result;
            };

            match result {
                Poll::Ready(Some(Err(err))) => {
                    let _ = setup_tx.send(Err(InnerError::ImmediateShutdown(Some(err)).into()));
                    Poll::Ready(Some(Err(ConnectionError::Closed)))
                }
                Poll::Ready(None) => {
                    let _ = setup_tx.send(Err(InnerError::ImmediateShutdown(None).into()));
                    Poll::Ready(None)
                }
                Poll::Ready(Some(Ok(_))) => {
                    let _ = setup_tx.send(Ok(()));
                    result
                }
                Poll::Pending => {
                    let _ = setup_tx.send(Ok(()));
                    cx.waker().wake_by_ref();
                    result
                }
            }
        })
    });
    match fut.await {
        Some(Ok(stream)) => match stream_tx.send_async(Some(Ok(stream))).await {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(Err(InnerError::ReceiverDisconnected.into())),
        },
        Some(Err(err)) => ControlFlow::Break(Err(InnerError::Disconnected(Some(err)).into())),
        None => ControlFlow::Break(Ok(())),
    }
}

async fn command_handler(
    connection: &Rc<Mutex<Connection<AsyncRwSock<'_>>>>,
    command: Command,
) -> ControlFlow<Result<(), Error>> {
    match command {
        Command::OpenStream(resp) => {
            repl_command(connection, resp, |mut connection, cx| {
                connection.poll_new_outbound(cx)
            })
            .await
        }
        Command::Close(resp) => {
            repl_command(connection, resp, |mut connection, cx| {
                connection.poll_close(cx)
            })
            .await
        }
    }
}

fn repl_command<T, C, F>(
    connection: &Rc<Mutex<Connection<C>>>,
    resp: oneshot::Sender<T>,
    mut fun: F,
) -> impl Future<Output = ControlFlow<Result<(), Error>>>
where
    T: Send,
    C: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(Pin<&mut Connection<C>>, &mut Context<'_>) -> Poll<Result<T, ConnectionError>>,
{
    let mut resp = Some(resp);
    poll_fn(move |cx| {
        mutex_poll_fn(connection, cx, |connection, cx| {
            let Poll::Ready(stream) = fun(connection, cx) else {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            };
            match stream {
                Ok(value) => {
                    let _ = resp.take().unwrap().send(value);
                    Poll::Ready(ControlFlow::Continue(()))
                }
                Err(err) => {
                    let err = InnerError::Disconnected(Some(err));
                    Poll::Ready(ControlFlow::Break(Err(err.into())))
                }
            }
        })
    })
}

struct AsyncRwSock<'a>(TcpSocket<'a>);

impl AsyncRead for AsyncRwSock<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let Poll::Ready(result) = pin!(self.0.read(buf)).poll(cx) else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        match result {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(_) => Poll::Ready(Err(ErrorKind::ConnectionReset.into())),
        }
    }
}

impl AsyncWrite for AsyncRwSock<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let Poll::Ready(result) = pin!(self.0.write(buf)).poll(cx) else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        match result {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(_) => Poll::Ready(Err(ErrorKind::ConnectionReset.into())),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let Poll::Ready(result) = pin!(self.0.flush()).poll(cx) else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        match result {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(_) => Poll::Ready(Err(ErrorKind::ConnectionReset.into())),
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.0.close();
        Poll::Ready(Ok(()))
    }
}
