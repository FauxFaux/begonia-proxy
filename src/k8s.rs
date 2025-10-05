use std::net::IpAddr;
use std::str::FromStr;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use k8s_openapi::api::core::v1::{EndpointPort, Endpoints};
use kube::Api;
use kube::Client;

fn has_port(port: i32, ports: &[EndpointPort]) -> bool {
    ports.into_iter().any(|ep| ep.port == port)
}

pub async fn find_dns(client: Client) -> Result<Vec<IpAddr>> {
    let endpoints = Api::<Endpoints>::namespaced(client, "kube-system")
        .get("kube-dns")
        .await
        .with_context(|| anyhow!("listing endpoints"))?;
    endpoints
        .subsets
        .unwrap_or_default()
        .into_iter()
        .filter(|endpoint| {
            endpoint
                .ports
                .as_ref()
                .map(|v| has_port(53, v))
                .unwrap_or(false)
        })
        .flat_map(|endpoint| {
            endpoint
                .addresses
                .unwrap_or_default()
                .into_iter()
                .map(|address| address.ip)
        })
        .map(|s| Ok(IpAddr::from_str(&s).with_context(|| anyhow!("parsing {:?}", s))?))
        .collect()
}
