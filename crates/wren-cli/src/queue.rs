use anyhow::{Context, Result};
use kube::{
    api::{Api, ListParams},
    Client,
};
use wren_core::WrenJob;

/// List WrenJobs with optional queue and namespace filters, formatted as a table.
pub async fn run(queue: Option<&str>, namespace: Option<&str>) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("failed to create Kubernetes client")?;

    let jobs: Vec<WrenJob> = if let Some(ns) = namespace {
        let api: Api<WrenJob> = Api::namespaced(client, ns);
        api.list(&ListParams::default())
            .await
            .context("failed to list WrenJobs")?
            .items
    } else {
        let api: Api<WrenJob> = Api::all(client);
        api.list(&ListParams::default())
            .await
            .context("failed to list WrenJobs")?
            .items
    };

    // Filter by queue if requested
    let jobs: Vec<&WrenJob> = if let Some(q) = queue {
        jobs.iter().filter(|j| j.spec.queue == q).collect()
    } else {
        jobs.iter().collect()
    };

    if jobs.is_empty() {
        println!("No jobs found.");
        return Ok(());
    }

    // Print table header
    println!(
        "{:<32} {:<16} {:<8} {:<16} {:<12}",
        "NAME", "STATE", "NODES", "QUEUE", "AGE"
    );
    println!("{}", "-".repeat(88));

    for job in jobs {
        let name = job.metadata.name.as_deref().unwrap_or("<unknown>");
        let state = job
            .status
            .as_ref()
            .map(|s| s.state.to_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let nodes = job.spec.nodes;
        let queue_name = &job.spec.queue;
        let age = job
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|ts| format_age(&ts.0))
            .unwrap_or_else(|| "<unknown>".to_string());

        println!(
            "{:<32} {:<16} {:<8} {:<16} {:<12}",
            name, state, nodes, queue_name, age
        );
    }

    Ok(())
}

/// Format a DateTime as a human-readable age string (e.g. "5m", "2h", "3d").
fn format_age(ts: &chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let elapsed = now.signed_duration_since(*ts);
    let secs = elapsed.num_seconds().max(0) as u64;

    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}
