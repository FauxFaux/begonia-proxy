use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use log::error;
use log::info;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::k8s::find_dns;
use crate::resolve::ResolveCtx;

mod k8s;
mod resolve;

#[derive(Debug)]
enum ConnectType<'b> {
    Http { hostname: String },
    Socks4Ip { ip: &'b [u8] },
    Socks4Host { hostname: &'b str },
}

async fn read_initialisation<'b>(
    socket: &mut TcpStream,
    buf: &'b mut [u8],
) -> Result<ConnectType<'b>> {
    let mut progress = 0;
    loop {
        let found = socket.read(&mut buf[progress..]).await?;
        if 0 == found {
            bail!("unexpected eof reading header")
        }
        progress += found;
        let valid = &buf[..progress];
        match valid[0] {
            b'C' | b'c' => {
                let mut headers = [httparse::EMPTY_HEADER; 16];
                let mut req = httparse::Request::new(&mut headers);
                if req.parse(&buf)?.is_partial() {
                    continue;
                }
                ensure!(
                    req.method == Some("CONNECT"),
                    "invalid method {:?}",
                    req.method
                );
                return match req.path {
                    Some(path) => Ok(ConnectType::Http {
                        hostname: path.to_string(),
                    }),
                    None => bail!("connect with no path?"),
                };
            }
            _ => {
                bail!("unrecognised, {:?}", valid);
            }
        }
    }
}

async fn worker(resolve_ctx: ResolveCtx, mut source: TcpStream) -> Result<()> {
    let mut buf = [0; 4096];
    let init = read_initialisation(&mut source, &mut buf).await?;
    let dest = match init {
        ConnectType::Http { hostname } => {
            let addrs = resolve::resolve(resolve_ctx, &hostname).await?;
            info!("establishing connection to {:?} via {:?}", hostname, addrs);
            let conn = TcpStream::connect(&*addrs).await?;
            source.write_all(b"HTTP/1.0 200 OK\r\n\r\n").await?;
            conn
        }
        _ => unimplemented!("{:?}", init),
    };

    let (mut source_read, mut source_write) = source.into_split();
    let (mut dest_read, mut dest_write) = dest.into_split();

    tokio::try_join!(
        tokio::io::copy(&mut source_read, &mut dest_write),
        tokio::io::copy(&mut dest_read, &mut source_write),
    )?;

    Ok(())
}

#[tokio::main]
pub async fn main() -> Result<()> {
    env_logger::init();

    let client = k8s::client_forking_proxy()?;

    let version_info = client
        .apiserver_version()
        .await
        .with_context(|| anyhow!("first request to the server"))?;

    info!(
        "found kube api server running {}.{}",
        version_info.major, version_info.minor
    );

    let dns = find_dns(client.clone())
        .await
        .with_context(|| anyhow!("finding dns servers"))?;
    info!("found kube-dns: {:?}", dns);

    #[cfg(never)]
    let pod = pods.get_status("svc-flags-78966c8947-45gds").await?;
    #[cfg(never)]
    println!(
        "{:?}",
        pod.status
            .unwrap()
            .pod_ips
            .unwrap()
            .into_iter()
            .flat_map(|p| p.ip)
            .collect::<Vec<_>>()
    );

    let addr = "[::]:3438";
    info!("binding to {:?}", addr);
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (socket, client_addr) = listener.accept().await?;
        let resolve_ctx = ResolveCtx {
            cluster_local: "cluster.local".to_string(),
            client: client.clone(),
            default_namespace: "default".to_string(),
            dns_servers: dns.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = worker(resolve_ctx, socket).await {
                error!("{:?} handling {:?}", e, client_addr);
            }
        });
    }
}
