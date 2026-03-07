use anyhow::{Context, Result};
use kube::api::{Api, DeleteParams};
use kube::Client;
use tracing::info;
use wren_core::WrenJob;

/// Delete an WrenJob by name in the given namespace.
pub async fn run(client: Client, job: &str, namespace: &str) -> Result<()> {
    let api: Api<WrenJob> = Api::namespaced(client, namespace);

    api.delete(job, &DeleteParams::default())
        .await
        .with_context(|| format!("failed to cancel job {job} in namespace {namespace}"))?;

    info!(job, namespace, "WrenJob cancelled");
    println!("job/{job} cancelled");

    Ok(())
}
