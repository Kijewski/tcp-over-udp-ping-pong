mod networking;

use std::future::poll_fn;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bstr::BStr;
use clap::Parser;
use futures_io::AsyncWrite;
use futures_util::{AsyncReadExt, AsyncWriteExt, TryStreamExt};
use networking::mutex_poll_fn;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::Mutex;
use tokio::time::sleep;

use self::networking::{NetworkConnection, split_stream};

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    simple_logger::init_with_env().unwrap();
    main_setup().await
}

async fn main_setup() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let udp = UdpSocket::bind(&cli.me).await?;
    udp.connect(&cli.remote).await?;
    let (mut conn, mut stream_rx) = NetworkConnection::new(cli.kind, Arc::new(udp)).await?;

    let stream = match cli.kind {
        Kind::Client => conn.open_stream().await?,
        Kind::Server => match stream_rx.try_next().await? {
            Some(stream) => stream,
            None => return Ok(()),
        },
    };
    let stream = Arc::new(Mutex::new(stream));
    let (mut rd_stream, mut wr_stream) = split_stream(Arc::clone(&stream));

    let rd_loop = async move {
        let mut buf = vec![0; 128];
        loop {
            let len = rd_stream.read(buf.as_mut_slice()).await?;
            if len == 0 {
                break;
            }
            println!("<< {:?}", BStr::new(&buf[..len]));
        }
        std::io::Result::Ok(())
    };

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

    poll_fn(|cx| mutex_poll_fn(&stream, cx, |s, cx| s.poll_close(cx))).await?;
    conn.close().await?;

    Ok(())
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
