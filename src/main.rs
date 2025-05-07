mod driver;
mod time;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bstr::BStr;
use clap::Parser;
use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config, IpEndpoint, IpListenEndpoint, Ipv4Cidr, Stack, StackResources, StaticConfigV4,
};
use tokio::net::UdpSocket;
use tokio::select;
use tokio::time::sleep;

use self::driver::UdpDriver;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    simple_logger::init_with_env()?;

    let udp = UdpSocket::bind(&cli.me).await?;
    udp.connect(&cli.remote).await?;
    let driver = UdpDriver::new(Arc::new(udp));
    let Some((cancel_tx, cancel_rx)) = driver.cancel_token() else {
        return Ok(());
    };

    let config = Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Addr::LOCALHOST, 0),
        gateway: None,
        dns_servers: Default::default(),
    });
    let mut resources = StackResources::<10>::new();
    let (stack, mut runner) = embassy_net::new(driver, config, &mut resources, 1);

    ctrlc::set_handler({
        let mut cancel_tx = Some(cancel_tx.clone());
        move || drop(cancel_tx.take())
    })?;

    select! {
        _ = cancel_rx.recv_async() => (),
        _ = runner.run() => (),
        _ = amain(cli, stack) => (),
    }
    Ok(())
}

async fn amain(cli: Cli, stack: Stack<'_>) -> Result<(), Box<dyn std::error::Error>> {
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

    let (mut sock_rx, mut sock_tx) = sock.split();
    let rd_loop = async move {
        let mut buf = vec![0; 128];
        loop {
            let len = sock_rx.read(buf.as_mut_slice()).await.unwrap();
            println!("<< {:?}", BStr::new(&buf[..len]));
        }
    };
    let wr_loop = async move {
        for i in 1..=10 {
            sock_tx.write(format!("{i}\n").as_bytes()).await.unwrap();
            sleep(Duration::from_millis(i * 125)).await;
        }
    };
    select! {
        () = rd_loop => (),
        () = wr_loop => (),
    }

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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Kind {
    Client,
    Server,
}
