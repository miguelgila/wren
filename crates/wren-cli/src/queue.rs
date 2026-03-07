use anyhow::{Context, Result};
use kube::{
    api::{Api, ListParams},
    Client,
};
use wren_core::WrenJob;

/// List WrenJobs with optional queue and namespace filters, formatted as a table.
pub async fn run(client: Client, queue: Option<&str>, namespace: Option<&str>) -> Result<()> {
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
        "{:<8} {:<32} {:<16} {:<8} {:<16} {:<12}",
        "JOBID", "NAME", "STATE", "NODES", "QUEUE", "AGE"
    );
    println!("{}", "-".repeat(96));

    for job in jobs {
        let job_id = job
            .status
            .as_ref()
            .and_then(|s| s.job_id)
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string());
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
            "{:<8} {:<32} {:<16} {:<8} {:<16} {:<12}",
            job_id, name, state, nodes, queue_name, age
        );
    }

    Ok(())
}

/// Format a DateTime as a human-readable age string (e.g. "5m", "2h", "3d").
pub(crate) fn format_age(ts: &chrono::DateTime<chrono::Utc>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    #[test]
    fn test_format_age_seconds() {
        let ts = Utc::now() - Duration::seconds(30);
        let age = format_age(&ts);
        assert!(age.ends_with('s'), "expected seconds suffix, got: {age}");
    }

    #[test]
    fn test_format_age_minutes() {
        let ts = Utc::now() - Duration::minutes(5);
        let age = format_age(&ts);
        assert_eq!(age, "5m");
    }

    #[test]
    fn test_format_age_hours() {
        let ts = Utc::now() - Duration::hours(3);
        let age = format_age(&ts);
        assert_eq!(age, "3h");
    }

    #[test]
    fn test_format_age_days() {
        let ts = Utc::now() - Duration::days(7);
        let age = format_age(&ts);
        assert_eq!(age, "7d");
    }

    #[test]
    fn test_format_age_future_timestamp() {
        // Future timestamps should clamp to 0s
        let ts = Utc::now() + Duration::hours(1);
        let age = format_age(&ts);
        assert_eq!(age, "0s");
    }
}
