use anyhow::{Context, Result};
use bubo_core::MPIJob;
use kube::{
    api::{Api, DeleteParams},
    Client,
};
use tracing::info;

/// Delete an MPIJob by name in the given namespace.
pub async fn run(job: &str, namespace: &str) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("failed to create Kubernetes client")?;

    let api: Api<MPIJob> = Api::namespaced(client, namespace);

    api.delete(job, &DeleteParams::default())
        .await
        .with_context(|| format!("failed to cancel job {job} in namespace {namespace}"))?;

    info!(job, namespace, "MPIJob cancelled");
    println!("job/{job} cancelled");

    Ok(())
}
