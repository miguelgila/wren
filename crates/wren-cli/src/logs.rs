use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, ListParams, LogParams},
    Client,
};
use tracing::debug;

/// Build a Kubernetes label selector string for fetching job pods.
pub(crate) fn build_label_selector(job: &str, rank: Option<u32>) -> String {
    match rank {
        Some(r) => format!("wren.giar.dev/job-name={job},wren.giar.dev/rank={r}"),
        None => format!("wren.giar.dev/job-name={job}"),
    }
}

/// Fetch logs from pods belonging to a WrenJob, with optional rank and follow support.
pub async fn run(client: Client, job: &str, rank: Option<u32>, follow: bool, namespace: &str) -> Result<()> {

    let pods: Api<Pod> = Api::namespaced(client, namespace);

    let label_selector = build_label_selector(job, rank);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_label_selector_no_rank() {
        let sel = build_label_selector("my-job", None);
        assert_eq!(sel, "wren.giar.dev/job-name=my-job");
    }

    #[test]
    fn test_build_label_selector_with_rank_zero() {
        let sel = build_label_selector("my-job", Some(0));
        assert_eq!(sel, "wren.giar.dev/job-name=my-job,wren.giar.dev/rank=0");
    }

    #[test]
    fn test_build_label_selector_with_rank() {
        let sel = build_label_selector("my-job", Some(5));
        assert_eq!(sel, "wren.giar.dev/job-name=my-job,wren.giar.dev/rank=5");
    }

    #[test]
    fn test_build_label_selector_special_chars_in_name() {
        let sel = build_label_selector("my-complex-job-123", None);
        assert_eq!(sel, "wren.giar.dev/job-name=my-complex-job-123");
    }
}
