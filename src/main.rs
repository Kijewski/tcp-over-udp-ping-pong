mod driver;
mod time;

use std::future::poll_fn;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bstr::BStr;
use clap::Parser;
use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config, IpEndpoint, IpListenEndpoint, Ipv4Cidr, Stack, StackResources, StaticConfigV4,
};
use futures_io::{AsyncRead, AsyncWrite};
use futures_util::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::{Mutex, oneshot};
use tokio::time::sleep;
use yamux::{Connection, Stream};

use self::driver::UdpDriver;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    main_setup().await
}

async fn main_setup() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    simple_logger::init_with_env()?;

    let udp = UdpSocket::bind(&cli.me).await?;
    udp.connect(&cli.remote).await?;
    let driver = UdpDriver::new(Arc::new(udp));
    let Some((cancel_tx, cancel_rx)) = driver.cancel_token() else {
        return Ok(());
    };

    ctrlc::set_handler({
        let mut cancel_tx = Some(cancel_tx.clone());
        move || drop(cancel_tx.take())
    })?;

    let config = Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Addr::LOCALHOST, 0),
        gateway: None,
        dns_servers: Default::default(),
    });
    let mut resources = Box::new(StackResources::<10>::new());
    let (stack, mut runner) = embassy_net::new(driver, config, &mut resources, 1);

    select! {
        biased;
        _ = cancel_rx.recv_async() => (),
        () = async {
            select! {
                _ = runner.run() => (),
                _ = main_connect(cli, stack) => (),
            }
        } => (),
    };

    Ok(())
}

async fn main_connect(cli: Cli, stack: Stack<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let mut rx_buffer = vec![0; 1 << 14];
    let mut tx_buffer = vec![0; 1 << 14];
    let mut sock = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    match cli.kind {
        Kind::Client => {
            let remote_endpoint = IpEndpoint {
                addr: Ipv4Addr::LOCALHOST.into(),
                port: 12345,
            };
            sock.connect(remote_endpoint).await.unwrap();
        }
        Kind::Server => {
            let local_endpoint = IpListenEndpoint {
                addr: Some(Ipv4Addr::LOCALHOST.into()),
                port: 12345,
            };
            sock.accept(local_endpoint).await.unwrap();
        }
    }

    let mut cfg = yamux::Config::default();
    cfg.set_max_num_streams(16);
    cfg.set_split_send_size(1100);
    let mode = match cli.kind {
        Kind::Client => yamux::Mode::Client,
        Kind::Server => yamux::Mode::Server,
    };
    let connection = Connection::new(AsyncRwSock(sock), cfg, mode);
    let connection = Rc::new(Mutex::new(connection));

    let (tx_stream, rx_stream) = oneshot::channel();

    let inbound = {
        let connection = Rc::clone(&connection);
        let mut tx_stream = Some(tx_stream);
        async move {
            loop {
                let fut = poll_fn(|cx| {
                    mutex_poll_fn(&connection, cx, |mut connection, cx| {
                        connection.poll_next_inbound(cx)
                    })
                });
                let Some(stream) = fut.await.transpose()? else {
                    return Result::<(), Box<dyn std::error::Error>>::Ok(());
                };
                let Some(tx_stream) = tx_stream.take() else {
                    panic!("did not expect multiple streams");
                };
                tx_stream.send(stream).unwrap();
            }
        }
    };

    select! {
        result = inbound => result?,
        result = repl(cli.kind, connection, rx_stream) => result?,
    }
    Ok(())
}

async fn repl(
    kind: Kind,
    connection: Rc<Mutex<Connection<AsyncRwSock<'_>>>>,
    rx_stream: oneshot::Receiver<Stream>,
) -> Result<(), Box<dyn std::error::Error + 'static>> {
    let stream = match kind {
        Kind::Client => {
            let result = poll_fn(|cx| {
                mutex_poll_fn(&connection, cx, |mut connection, cx| {
                    connection.poll_new_outbound(cx)
                })
            });
            match result.await {
                Ok(stream) => stream,
                Err(_) => return Err(std::io::Error::from(ErrorKind::ConnectionReset).into()),
            }
        }
        Kind::Server => rx_stream.await?,
    };
    let stream = Rc::new(Mutex::new(stream));

    let mut rd_stream = RdStream(Rc::clone(&stream));
    let rd_loop = async move {
        let mut buf = vec![0; 128];
        loop {
            let len = rd_stream.read(buf.as_mut_slice()).await?;
            println!("<< {:?}", BStr::new(&buf[..len]));
        }
        #[allow(unreachable_code)]
        std::io::Result::Ok(())
    };

    let mut wr_stream = WrStream(stream);
    let wr_loop = async move {
        for i in 1..=10 {
            wr_stream.write_all(format!("{i}\n").as_bytes()).await?;
            sleep(Duration::from_millis(i * 125)).await;
        }
        std::io::Result::Ok(())
    };

    select! {
        result = rd_loop => result?,
        result = wr_loop => result?,
    }
    Ok(())
}

struct RdStream(Rc<Mutex<Stream>>);

impl AsyncRead for RdStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        mutex_poll_fn(&self.0, cx, |stream, cx| stream.poll_read(cx, buf))
    }
}

struct WrStream(Rc<Mutex<Stream>>);

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

fn mutex_poll_fn<'cx, F, D, R>(mutex: &Rc<Mutex<D>>, cx: &mut Context<'cx>, fun: F) -> Poll<R>
where
    F: FnOnce(Pin<&mut D>, &mut Context<'cx>) -> Poll<R>,
{
    if let Poll::Ready(mut guard) = pin!(mutex.lock()).poll(cx) {
        // SAFETY: the Rc pinned the data down
        let data = unsafe { Pin::new_unchecked(&mut *guard) };
        if let Poll::Ready(result) = fun(data, cx) {
            return Poll::Ready(result);
        }
    }
    Poll::Pending
}

struct AsyncRwSock<'a>(TcpSocket<'a>);

impl AsyncRead for AsyncRwSock<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        match pin!(self.0.read(buf)).poll(cx) {
            Poll::Ready(Ok(len)) => Poll::Ready(Ok(len)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(ErrorKind::ConnectionReset.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for AsyncRwSock<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match pin!(self.0.write(buf)).poll(cx) {
            Poll::Ready(Ok(len)) => Poll::Ready(Ok(len)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(ErrorKind::ConnectionReset.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match pin!(self.0.flush()).poll(cx) {
            Poll::Ready(Ok(len)) => Poll::Ready(Ok(len)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(ErrorKind::ConnectionReset.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.0.close();
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug, Clone, Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// Kind
    kind: Kind,
    /// My address
    me: SocketAddr,
    /// Remote address
    remote: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Kind {
    Client,
    Server,
}
