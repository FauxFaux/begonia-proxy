use std::net::IpAddr;
use std::str::FromStr;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
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
        .into_iter()
        .filter(|endpoint| has_port(53, &endpoint.ports))
        .flat_map(|endpoint| endpoint.addresses.into_iter().map(|address| address.ip))
        .map(|s| Ok(IpAddr::from_str(&s).with_context(|| anyhow!("parsing {:?}", s))?))
        .collect()
}
