use std::convert::TryInto;
use std::io;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use kube::Client;
use log::debug;
use log::error;
use log::info;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::k8s::find_dns;
use crate::resolve::ResolveCtx;

mod k8s;
mod resolve;

#[derive(Debug)]
enum ConnectType {
    Http { hostname: String, port: u16 },
    Socks4Ip { ip: Ipv4Addr, port: u16 },
    Socks4Host { hostname: String, port: u16 },

    // not a connect, but we're gonna reply anyway
    InvalidHttpGet { path: String },
}

async fn read_initialisation(socket: &mut TcpStream, buf: &mut [u8]) -> Result<ConnectType> {
    let mut progress = 0;
    loop {
        let found = socket.read(&mut buf[progress..]).await?;
        if 0 == found {
            bail!("unexpected eof reading header")
        }
        progress += found;
        let valid = &buf[..progress];
        return match valid[0] {
            // https-style CONNECT
            b'C' | b'G' => {
                // curl -p -x http://localhost:3438 http://kube-dns.kube-system:9153/metrics
                let mut headers = [httparse::EMPTY_HEADER; 16];
                let mut req = httparse::Request::new(&mut headers);
                if req.parse(&buf)?.is_partial() {
                    continue;
                }
                let path = req.path.ok_or(anyhow!("no path on a valid request?"))?;
                let host_with_port = match req.method {
                    Some("CONNECT") => path,
                    Some("GET") => {
                        return Ok(ConnectType::InvalidHttpGet {
                            path: path.to_string(),
                        })
                    }
                    method => bail!("invalid method {:?}", method),
                };
                let colon = host_with_port
                    .rfind(|c| c == ':')
                    .ok_or(anyhow!("port required in hostname"))?;
                let (hostname, port) = host_with_port.split_at(colon);
                if port.is_empty() {
                    bail!("empty port");
                }
                let port = u16::from_str(&port[1..])?;
                Ok(ConnectType::Http {
                    hostname: hostname.to_string(),
                    port,
                })
            }
            // socks 4 + socks 4a
            0x04 => {
                let fixed_header_len = 1 + 1 + 2 + 4;
                if valid.len() < fixed_header_len {
                    debug!("socks4 client only sent {} bytes", valid.len());
                    continue;
                }

                // establish connection
                if valid[1] != 0x01 {
                    socket.write_all(b"\0\x5b").await?;
                    socket.write_all(&valid[2..8]).await?;
                    bail!("invalid socks4 command: {:02x}", valid[1]);
                }

                let user_end = match valid
                    .iter()
                    .skip(fixed_header_len)
                    .position(|&c| c == b'\0')
                {
                    Some(pos) => fixed_header_len + pos,
                    None => continue,
                };
                // ignoring actual user data (typically empty anyway)

                // strip null
                let user_end = user_end + 1;

                let port = u16::from_be_bytes(valid[2..4].try_into().expect("explicit slice"));
                // TODO: do we need to byteswap this?
                let ip: [u8; 4] = valid[4..8].try_into().expect("explicit slice");
                let ip = Ipv4Addr::from(ip);

                if socks4a_marker_ip(&ip) {
                    let hostname_end = match valid.iter().skip(user_end).position(|&c| c == b'\0') {
                        Some(pos) => user_end + pos,
                        None => continue,
                    };
                    Ok(ConnectType::Socks4Host {
                        hostname: String::from_utf8(valid[user_end..hostname_end].to_vec())?,
                        port,
                    })
                } else {
                    Ok(ConnectType::Socks4Ip { ip, port })
                }
            }
            _ => {
                bail!("unrecognised, {:?}", valid);
            }
        };
    }
}

fn socks4a_marker_ip(ip: &Ipv4Addr) -> bool {
    let oc = ip.octets();
    oc[0] == 0 && oc[1] == 0 && oc[2] == 0 && oc[3] != 0
}

async fn worker(resolve_ctx: ResolveCtx, mut source: TcpStream) -> Result<()> {
    let peer = source.peer_addr()?;

    let mut buf = [0; 4096];
    let init = read_initialisation(&mut source, &mut buf).await?;
    let (hint, ok_message, addrs) = match init {
        // TODO: these are all clearly the same
        ConnectType::Http { hostname, port } => {
            let addrs = resolve::resolve(resolve_ctx, &hostname, port).await?;
            (
                format!("HTTP CONNECT to {}", hostname),
                b"HTTP/1.0 200 OK\r\n\r\n".to_vec(),
                addrs,
            )
        }
        ConnectType::Socks4Host { hostname, port } => {
            let addrs = match resolve::resolve(resolve_ctx, &hostname, port).await {
                Ok(addrs) => addrs,
                Err(err) => {
                    info!("invalid client request: {:?}", err);
                    // 5b: generic rejection (no error propagation available)
                    source.write_all(b"\0\x5b\0\0\0\0\0\0").await?;
                    return Ok(());
                }
            };
            // 5a: OK!, other fields irrelevant for a connect request
            (
                format!("Socks4a to {}", hostname),
                b"\0\x5a\0\0\0\0\0\0".to_vec(),
                addrs,
            )
        }
        ConnectType::Socks4Ip { ip, port } => {
            let addr = SocketAddr::new(IpAddr::V4(ip), port);
            // 5a: OK!, other fields irrelevant for a connect request
            (
                format!("Socks4 legacy to {:?}", ip),
                b"\0\x5a\0\0\0\0\0\0".to_vec(),
                vec![addr],
            )
        }

        ConnectType::InvalidHttpGet { path } => {
            let msg = match path.as_ref() {
                "/" => concat!("HTTP/1.0 200 OK\r\n\r\n", env!("CARGO_CRATE_NAME")),
                "/healthcheck" => {
                    "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}"
                }
                _ => "HTTP/1.0 404 NO\r\n\r\n",
            };
            source.write_all(msg.as_bytes()).await?;
            return Ok(());
        }
    };

    info!("establishing {} via {:?}", hint, addrs);
    let dest = TcpStream::connect(&*addrs).await?;
    source.write_all(&ok_message).await?;

    let (mut source_read, mut source_write) = source.into_split();
    let (mut dest_read, mut dest_write) = dest.into_split();

    // let sent = tokio::io::copy(&mut source_read, &mut dest_write).await?;
    // dest_write.shutdown().await?;

    tokio::try_join!(
        copy_close(&mut source_read, &mut dest_write),
        copy_close(&mut dest_read, &mut source_write),
    )?;

    info!("{:?} exited cleanly", peer);

    Ok(())
}

pub async fn copy_close<'a, R, W>(reader: &'a mut R, writer: &'a mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let b = tokio::io::copy(reader, writer).await?;
    writer.shutdown().await?;
    Ok(b)
}

#[tokio::main]
pub async fn main() -> Result<()> {
    env_logger::init();

    let client = Client::try_default().await.context("initialising client")?;

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
