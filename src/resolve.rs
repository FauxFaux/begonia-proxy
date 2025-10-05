use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use hickory_resolver::Name;
use hickory_resolver::config::{NameServerConfig, ResolverConfig};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::proto::xfer::Protocol;
use k8s_openapi::api::core::v1::Endpoints;
use kube::Api;
use regex::Regex;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::LazyLock;

#[derive(Clone)]
pub struct ResolveCtx {
    pub cluster_local: String,
    pub default_namespace: String,
    pub client: kube::Client,
    pub dns_servers: Vec<IpAddr>,
}

// resolution order:
// $0.$default.endpoints.local.
// $0.endpoints.local.
// # $0.$default.pod.local.
// # $0.pod.local.
// # $0.$default.pod-by-name.local.
// # $0.pod-by-name.local.

// then: cluster dns with:
// $0.$default.svc.cluster.local
// $0.svc.cluster.local
// $0.cluster.local
pub(crate) async fn resolve(
    ctx: ResolveCtx,
    hostname: &str,
    specified_port: u16,
) -> Result<Vec<SocketAddr>> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            "^([a-zA-Z0-9-]{1,63})(?:\\.([a-zA-Z0-9-]{1,63}))?",
            "\\.(endpoints|pod|pod-by-name)\\.local\\.?$"
        ))
        .unwrap()
    });

    if let Some(custom) = RE.captures(&hostname) {
        let name: &str = &custom[1];
        let ns: String = custom
            .get(2)
            .map(|v| v.as_str().to_string())
            .unwrap_or(ctx.default_namespace);
        let command: &str = &custom[3];

        match command {
            "endpoints" => {
                let mut ips = Vec::with_capacity(16);
                let mut ports = HashSet::<i32>::with_capacity(4);
                let endpoints = Api::<Endpoints>::namespaced(ctx.client.clone(), &ns)
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
                } else {
                    // TODO: map using actual service
                    specified_port
                };

                return Ok(ips
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect());
            }
            _ => unimplemented!("{:?}", command),
        }
    }

    Ok(resolve_against_kube_dns(ctx, &hostname)
        .await?
        .into_iter()
        .map(|ip| SocketAddr::new(ip, specified_port))
        .collect())
}

async fn resolve_against_kube_dns(ctx: ResolveCtx, hostname: &str) -> Result<Vec<IpAddr>> {
    let mut config = ResolverConfig::new();
    for ip in ctx.dns_servers {
        config.add_name_server(NameServerConfig {
            protocol: Protocol::Udp,
            socket_addr: SocketAddr::new(ip, 53),
            tls_dns_name: None,
            trust_negative_responses: true,
            bind_addr: None,
            http_endpoint: None,
        });
        config.add_search(Name::from_str(&format!(
            "{}.svc.{}",
            &ctx.default_namespace, &ctx.cluster_local
        ))?);
        config.add_search(Name::from_str(&format!("svc.{}", &ctx.cluster_local))?);
        config.add_search(Name::from_str(&ctx.cluster_local)?);
    }
    Ok(hickory_resolver::TokioResolver::builder_with_config(
        config,
        TokioConnectionProvider::default(),
    )
    .build()
    .lookup_ip(hostname)
    .await?
    .into_iter()
    .map(|v| v)
    .collect())
}
