use anyhow::{Context, Result};
use kube::{api::Api, Client};
use wren_core::{WrenJob, WrenJobSpec, WrenJobStatus};

/// Format job status information as a displayable string.
/// Extracted from the async `run` function for testability.
pub(crate) fn format_job_status(
    name: &str,
    namespace: &str,
    spec: &WrenJobSpec,
    status: Option<&WrenJobStatus>,
) -> String {
    let mut out = String::new();
    use std::fmt::Write;

    if let Some(s) = status {
        if let Some(id) = s.job_id {
            writeln!(out, "JobID:       {}", id).unwrap();
        }
    }
    writeln!(out, "Name:        {}", name).unwrap();
    writeln!(out, "Namespace:   {}", namespace).unwrap();
    writeln!(out, "Queue:       {}", spec.queue).unwrap();
    writeln!(out, "Priority:    {}", spec.priority).unwrap();
    writeln!(out, "Nodes:       {}", spec.nodes).unwrap();
    writeln!(out, "Tasks/Node:  {}", spec.tasks_per_node).unwrap();
    writeln!(
        out,
        "Backend:     {}",
        format!("{:?}", spec.backend).to_lowercase()
    )
    .unwrap();
    if let Some(wt) = &spec.walltime {
        writeln!(out, "Walltime:    {}", wt).unwrap();
    }

    writeln!(out).unwrap();
    writeln!(out, "Status:").unwrap();
    if let Some(s) = status {
        writeln!(out, "  State:       {}", s.state).unwrap();
        if let Some(msg) = &s.message {
            writeln!(out, "  Message:     {}", msg).unwrap();
        }
        writeln!(
            out,
            "  Workers:     {}/{}",
            s.ready_workers, s.total_workers
        )
        .unwrap();
        if let Some(start) = &s.start_time {
            writeln!(out, "  Start Time:  {}", start).unwrap();
        }
        if let Some(end) = &s.completion_time {
            writeln!(out, "  End Time:    {}", end).unwrap();
        }
        if !s.assigned_nodes.is_empty() {
            writeln!(out, "  Nodes:       {}", s.assigned_nodes.join(", ")).unwrap();
        }
    } else {
        writeln!(out, "  State:       Unknown (no status subresource)").unwrap();
    }

    if !spec.dependencies.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "Dependencies:").unwrap();
        for dep in &spec.dependencies {
            writeln!(out, "  {:?} -> {}", dep.dep_type, dep.job).unwrap();
        }
    }

    out
}

/// Show detailed status information about a specific WrenJob.
pub async fn run(client: Client, job: &str, namespace: &str) -> Result<()> {
    let api: Api<WrenJob> = Api::namespaced(client, namespace);

    let wrenjob = api
        .get(job)
        .await
        .with_context(|| format!("job {job} not found in namespace {namespace}"))?;

    let output = format_job_status(job, namespace, &wrenjob.spec, wrenjob.status.as_ref());
    print!("{}", output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_core::{ContainerSpec, DependencyType, ExecutionBackendType, JobDependency, JobState};

    fn make_spec() -> WrenJobSpec {
        WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes: 2,
            tasks_per_node: 1,
            backend: ExecutionBackendType::Container,
            container: Some(ContainerSpec {
                image: "busybox".to_string(),
                command: vec![],
                args: vec![],
                resources: None,
                host_network: false,
                volume_mounts: vec![],
                env: vec![],
            }),
            reaper: None,
            mpi: None,
            topology: None,
            dependencies: vec![],
        }
    }

    #[test]
    fn test_format_job_status_basic() {
        let spec = make_spec();
        let output = format_job_status("my-job", "default", &spec, None);
        assert!(!output.contains("JobID:"));
        assert!(output.contains("Name:        my-job"));
        assert!(output.contains("Namespace:   default"));
        assert!(output.contains("Queue:       default"));
        assert!(output.contains("Nodes:       2"));
        assert!(output.contains("Unknown (no status subresource)"));
    }

    #[test]
    fn test_format_job_status_with_status() {
        let spec = make_spec();
        let status = WrenJobStatus {
            job_id: Some(7),
            state: JobState::Running,
            message: Some("all workers ready".to_string()),
            assigned_nodes: vec!["node-0".to_string(), "node-1".to_string()],
            start_time: Some("2024-01-01T00:00:00Z".to_string()),
            completion_time: None,
            ready_workers: 2,
            total_workers: 2,
        };
        let output = format_job_status("my-job", "ns", &spec, Some(&status));
        assert!(output.contains("JobID:       7"));
        assert!(output.contains("Running"));
        assert!(output.contains("all workers ready"));
        assert!(output.contains("Workers:     2/2"));
        assert!(output.contains("node-0, node-1"));
        assert!(output.contains("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn test_format_job_status_with_walltime() {
        let mut spec = make_spec();
        spec.walltime = Some("4h".to_string());
        let output = format_job_status("job", "ns", &spec, None);
        assert!(output.contains("Walltime:    4h"));
    }

    #[test]
    fn test_format_job_status_with_completion_time() {
        let spec = make_spec();
        let status = WrenJobStatus {
            state: JobState::Succeeded,
            completion_time: Some("2024-01-01T04:00:00Z".to_string()),
            ..Default::default()
        };
        let output = format_job_status("job", "ns", &spec, Some(&status));
        assert!(output.contains("End Time:    2024-01-01T04:00:00Z"));
    }

    #[test]
    fn test_format_job_status_with_dependencies() {
        let mut spec = make_spec();
        spec.dependencies = vec![JobDependency {
            dep_type: DependencyType::AfterOk,
            job: "prev-job".to_string(),
        }];
        let output = format_job_status("job", "ns", &spec, None);
        assert!(output.contains("Dependencies:"));
        assert!(output.contains("AfterOk -> prev-job"));
    }

    #[test]
    fn test_format_job_status_no_message() {
        let spec = make_spec();
        let status = WrenJobStatus {
            state: JobState::Pending,
            message: None,
            ..Default::default()
        };
        let output = format_job_status("job", "ns", &spec, Some(&status));
        assert!(!output.contains("Message:"));
    }
}
