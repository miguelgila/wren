use anyhow::{Context, Result};
use bubo_core::MPIJob;
use kube::{
    api::{Api, PostParams},
    Client,
};
use std::fs;
use tracing::info;

/// Submit an MPIJob from a YAML file, with optional queue and nodes overrides.
pub async fn run(file: &str, queue: Option<&str>, nodes: Option<u32>) -> Result<()> {
    let yaml = fs::read_to_string(file)
        .with_context(|| format!("failed to read job file: {file}"))?;

    let mut job: MPIJob =
        serde_yaml::from_str(&yaml).context("failed to parse MPIJob YAML")?;

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

    let api: Api<MPIJob> = Api::namespaced(client, &namespace);

    let created = api
        .create(&PostParams::default(), &job)
        .await
        .context("failed to create MPIJob")?;

    let name = created.metadata.name.as_deref().unwrap_or("<unknown>");
    info!(job = name, namespace = %namespace, "MPIJob submitted");
    println!("job/{name} submitted to namespace {namespace}");

    Ok(())
}
