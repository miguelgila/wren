use anyhow::{Context, Result};
use bubo_core::BuboJob;
use kube::{
    api::{Api, PostParams},
    Client,
};
use std::fs;
use tracing::info;

/// Submit an BuboJob from a YAML file, with optional queue and nodes overrides.
pub async fn run(file: &str, queue: Option<&str>, nodes: Option<u32>) -> Result<()> {
    let yaml = fs::read_to_string(file)
        .with_context(|| format!("failed to read job file: {file}"))?;

    let mut job: BuboJob =
        serde_yaml::from_str(&yaml).context("failed to parse BuboJob YAML")?;

    if let Some(q) = queue {
        job.spec.queue = q.to_string();
    }
    if let Some(n) = nodes {
        job.spec.nodes = n;
    }

    let client = Client::try_default()
        .await
        .context("failed to create Kubernetes client")?;

    let namespace = job
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| "default".to_string());

    let api: Api<BuboJob> = Api::namespaced(client, &namespace);

    let created = api
        .create(&PostParams::default(), &job)
        .await
        .context("failed to create BuboJob")?;

    let name = created.metadata.name.as_deref().unwrap_or("<unknown>");
    info!(job = name, namespace = %namespace, "BuboJob submitted");
    println!("job/{name} submitted to namespace {namespace}");

    Ok(())
}
