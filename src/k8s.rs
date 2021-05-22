use std::convert::TryFrom;
use std::io::BufRead;
use std::net::IpAddr;
use std::process::Command;
use std::process::Stdio;
use std::str::FromStr;

use anyhow::anyhow;
use anyhow::ensure;
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
        .ok_or(anyhow!("no kube-dns endpoints at all"))?
        .into_iter()
        .filter(|endpoint| match endpoint.ports.as_ref() {
            Some(ports) => has_port(53, ports),
            None => false,
        })
        .flat_map(|endpoint| match endpoint.addresses {
            Some(addresses) => addresses.into_iter().map(|address| address.ip).collect(),
            None => Vec::new(),
        })
        .map(|s| Ok(IpAddr::from_str(&s).with_context(|| anyhow!("parsing {:?}", s))?))
        .collect()
}

// Config::infer() works fine, except it finds a TLS url, and:
//  * openssl isn't available on the target box
//  * rustls can't talk to https://1.2.3.4/ urls: https://github.com/clux/kube-rs/issues/153
pub fn client_forking_proxy() -> Result<Client> {
    let mut kc = Command::new("kubectl")
        .arg("proxy")
        .arg("--port=0")
        .arg("--reject-methods=POST,PUT,PATCH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = kc
        .stdout
        .take()
        .expect("configured for piped, and not previously taken");

    // prevent leaks, don't care too much
    tokio::task::spawn_blocking(move || {
        log::warn!("kubectl proxy exited: {:?}", kc.wait());
    });

    let mut line = String::new();
    std::io::BufReader::new(stdout).read_line(&mut line)?;
    let prefix = "Starting to serve on ";
    ensure!(
        line.starts_with(prefix),
        "kubectl appears not to have started: {:?}",
        line
    );
    let mut url = "http://".to_string();
    let server = line[prefix.len()..].trim();
    url.push_str(server);
    let config = kube::Config::new(url::Url::parse(&url)?);
    let client = kube::Client::try_from(config)?;
    Ok(client)
}
