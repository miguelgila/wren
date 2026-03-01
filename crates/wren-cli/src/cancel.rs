use anyhow::{Context, Result};
use kube::{
    api::{Api, DeleteParams},
    Client,
};
use tracing::info;
use wren_core::WrenJob;

/// Delete an WrenJob by name in the given namespace.
pub async fn run(job: &str, namespace: &str) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("failed to create Kubernetes client")?;

    let api: Api<WrenJob> = Api::namespaced(client, namespace);

    api.delete(job, &DeleteParams::default())
        .await
        .with_context(|| format!("failed to cancel job {job} in namespace {namespace}"))?;

    info!(job, namespace, "WrenJob cancelled");
    println!("job/{job} cancelled");

    Ok(())
}
