use std::collections::HashSet;
use std::convert::TryFrom;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use anyhow::anyhow;
use anyhow::ensure;
use anyhow::Result;
use anyhow::{bail, Context};
use k8s_openapi::api::core::v1::{Endpoints, Pod};
use kube::{Api, Client};
use lazy_static::lazy_static;
use log::error;
use log::info;
use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::system_conf::read_system_conf;
use trust_dns_resolver::{Name, Resolver};

use crate::k8s::find_dns;

mod k8s;

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

// resolution order:
// $0.default.svc.local.
// # $0.svc.local.
// $0.default.pod.local.
// # $0.pod.local.
// $0.default.pod-by-name.local.
// # $0.pod-by-name.local.

// then: cluster dns with:
// $0.default.svc.cluster.local
// $0.svc.cluster.local
// $0.cluster.local
async fn resolve(client: &Client, hostname: &str) -> Result<Vec<SocketAddr>> {
    lazy_static! {
        static ref RE: Regex = Regex::new(concat!(
            "^([a-zA-Z0-9-]{1,63})(?:\\.([a-zA-Z0-9-]{1,63}))?",
            "\\.(svc|pod|pod-by-name)\\.local\\.?(?::(\\d+))?$"
        ))
        .unwrap();
    }

    if let Some(custom) = RE.captures(hostname) {
        let name: &str = &custom[1];
        let ns: &str = &custom.get(2).map(|v| v.as_str()).unwrap_or("default");
        let command: &str = &custom[3];
        let specified_port: Option<&str> = custom.get(4).map(|v| v.as_str());

        match command {
            "svc" => {
                let mut ips = Vec::with_capacity(16);
                let mut ports = HashSet::<i32>::with_capacity(4);
                let endpoints = Api::<Endpoints>::namespaced(client.clone(), ns)
                    .get(name)
                    .await?;

                // lazily ignoring the idea that there might be differences between sets
                for subset in endpoints.subsets.unwrap_or_default() {
                    for port in subset.ports.unwrap_or_default() {
                        ports.insert(port.port);
                    }

                    for address in subset.addresses.unwrap_or_default() {
                        ips.push(
                            IpAddr::from_str(&address.ip)
                                .with_context(|| anyhow!("parsing {:?}", address.ip))?,
                        );
                    }
                }

                if ports.is_empty() || ips.is_empty() {
                    return Ok(Vec::new());
                }

                let port = if ports.len() == 1 {
                    u16::try_from(
                        ports
                            .into_iter()
                            .next()
                            .expect("is of length one so has an element"),
                    )?
                } else if let Some(specified_port) = specified_port {
                    // TODO: map using actual service
                    u16::from_str(specified_port)?
                } else {
                    unimplemented!()
                };

                return Ok(ips
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect());
            }
            _ => unimplemented!("{:?}", command),
        }
    }

    return resolve_against_kube_dns(hostname).await;
}

async fn resolve_against_kube_dns(hostname: &str) -> Result<Vec<SocketAddr>> {
    unimplemented!()
}

async fn worker(client: Client, mut source: TcpStream) -> Result<()> {
    let mut buf = [0; 4096];
    let init = read_initialisation(&mut source, &mut buf).await?;
    let dest = match init {
        ConnectType::Http { hostname } => {
            let addrs = resolve(&client, &hostname).await?;
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

    let namespace = "default";

    let pods = Api::<Pod>::namespaced(client.clone(), &namespace);
    let pod = pods.get_status("svc-flags-78966c8947-45gds").await?;
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
        let client = client.clone();
        tokio::spawn(async move {
            if let Err(e) = worker(client, socket).await {
                error!("{:?} handling {:?}", e, client_addr);
            }
        });
    }
}
