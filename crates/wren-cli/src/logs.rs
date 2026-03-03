use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, ListParams, LogParams},
    Client,
};
use tracing::debug;

/// Fetch logs from pods belonging to a WrenJob, with optional rank and follow support.
pub async fn run(job: &str, rank: Option<u32>, follow: bool, namespace: &str) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("failed to create Kubernetes client")?;

    let pods: Api<Pod> = Api::namespaced(client, namespace);

    let label_selector = match rank {
        Some(r) => format!("wren.scops-hpc.com/job-name={job},wren.scops-hpc.com/rank={r}"),
        None => format!("wren.scops-hpc.com/job-name={job}"),
    };

    let lp = ListParams::default().labels(&label_selector);
    let pod_list = pods
        .list(&lp)
        .await
        .with_context(|| format!("failed to list pods for job {job}"))?;

    if pod_list.items.is_empty() {
        if let Some(r) = rank {
            println!("No pods found for job {job} rank {r} in namespace {namespace}");
        } else {
            println!("No pods found for job {job} in namespace {namespace}");
        }
        return Ok(());
    }

    for pod in &pod_list.items {
        let pod_name = pod.metadata.name.as_deref().unwrap_or("<unknown>");
        debug!(pod = pod_name, "fetching logs");

        let log_params = LogParams {
            follow,
            ..Default::default()
        };

        // Use non-streaming logs API for simplicity.
        // For follow mode, kube-rs log_stream returns futures-io AsyncBufRead;
        // a full streaming implementation would bridge to tokio. For v0.1 we
        // fetch a snapshot instead.
        let log_text = pods
            .logs(pod_name, &log_params)
            .await
            .with_context(|| format!("failed to get logs for pod {pod_name}"))?;

        if pod_list.items.len() > 1 {
            for line in log_text.lines() {
                println!("[{pod_name}] {line}");
            }
        } else {
            print!("{log_text}");
        }
    }

    Ok(())
}
