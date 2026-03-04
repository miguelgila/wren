use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use wren_core::backend::{BackendJobStatus, ExecutionBackend};
use wren_core::{ClusterState, JobState, WalltimeDuration, WrenError, WrenJob, WrenJobStatus};
use wren_scheduler::gang::GangScheduler;

use crate::metrics::Metrics;
use crate::reservation::ReservationManager;

/// Context shared across reconciliation calls.
pub struct ReconcilerContext {
    pub client: Client,
    pub cluster_state: Arc<RwLock<ClusterState>>,
    pub backend: Arc<dyn ExecutionBackend>,
    pub metrics: Metrics,
    pub reservations: RwLock<ReservationManager>,
}

/// Reconcile a single WrenJob. Called by the controller whenever the resource changes.
pub async fn reconcile(job: &WrenJob, ctx: &ReconcilerContext) -> Result<(), WrenError> {
    let name = job.metadata.name.as_deref().unwrap_or("unknown");
    let namespace = job.metadata.namespace.as_deref().unwrap_or("default");

    let status = job.status.as_ref().cloned().unwrap_or_default();
    let spec = &job.spec;

    info!(
        job = name,
        namespace,
        state = %status.state,
        "reconciling WrenJob"
    );

    match status.state {
        JobState::Pending => handle_pending(name, namespace, spec, ctx).await,
        JobState::Scheduling => handle_scheduling(name, namespace, spec, ctx).await,
        JobState::Running => handle_running(name, namespace, spec, &status, ctx).await,
        JobState::Succeeded
        | JobState::Failed
        | JobState::Cancelled
        | JobState::WalltimeExceeded => {
            // Terminal states — clean up resources, preserve pods for log TTL
            handle_terminal(name, namespace, &status, ctx).await
        }
    }
}

async fn handle_pending(
    name: &str,
    namespace: &str,
    spec: &wren_core::WrenJobSpec,
    ctx: &ReconcilerContext,
) -> Result<(), WrenError> {
    // Validate the job spec
    if let Err(msg) = validate_job_spec(spec) {
        update_status(name, namespace, JobState::Failed, Some(&msg), ctx).await?;
        return Ok(());
    }

    // Transition to Scheduling
    update_status(name, namespace, JobState::Scheduling, None, ctx).await?;
    ctx.metrics
        .record_job_state_change(&spec.queue, "Pending", "Scheduling");
    Ok(())
}

async fn handle_scheduling(
    name: &str,
    namespace: &str,
    spec: &wren_core::WrenJobSpec,
    ctx: &ReconcilerContext,
) -> Result<(), WrenError> {
    // Attempt gang scheduling
    let cluster = ctx.cluster_state.read().await;

    // For now, use default resource estimates.
    // TODO: parse from container spec resources
    let cpu_per_node = 1000u64; // 1 core default
    let mem_per_node = 1_000_000_000u64; // 1 GB default
    let gpus_per_node = 0u32;

    let result = GangScheduler::schedule(
        &cluster,
        name,
        spec.nodes,
        cpu_per_node,
        mem_per_node,
        gpus_per_node,
        spec.topology.as_ref(),
    );
    drop(cluster); // release read lock

    let scheduling_start = std::time::Instant::now();

    match result {
        Ok(placement) => {
            let scheduling_secs = scheduling_start.elapsed().as_secs_f64();
            ctx.metrics
                .record_scheduling_latency("success", scheduling_secs);
            ctx.metrics.record_topology_score(placement.score);

            info!(
                job = name,
                nodes = ?placement.nodes,
                score = placement.score,
                "placement found"
            );

            // Create reservation to prevent double-booking during launch
            {
                let mut reservations = ctx.reservations.write().await;
                reservations.reserve(name, &placement, cpu_per_node, mem_per_node, gpus_per_node);
                ctx.metrics.record_reservation_start();
            }

            // Record allocations
            {
                let mut cluster = ctx.cluster_state.write().await;
                for node in &placement.nodes {
                    cluster.allocate(node, cpu_per_node, mem_per_node, gpus_per_node, name);
                }
            }

            // Launch via backend
            match ctx.backend.launch(name, namespace, spec, &placement).await {
                Ok(launch_result) => {
                    // Release reservation — resources are now committed
                    {
                        let mut reservations = ctx.reservations.write().await;
                        reservations.release(name);
                        ctx.metrics.record_reservation_end();
                    }

                    info!(
                        job = name,
                        resources = ?launch_result.resource_ids,
                        "job launched successfully"
                    );

                    // Update status to Running with assigned nodes
                    let api: Api<WrenJob> = Api::namespaced(ctx.client.clone(), namespace);
                    let now = chrono::Utc::now().to_rfc3339();
                    let status = serde_json::json!({
                        "status": {
                            "state": "Running",
                            "message": launch_result.message,
                            "assignedNodes": placement.nodes,
                            "startTime": now,
                            "totalWorkers": spec.nodes,
                            "readyWorkers": 0,
                        }
                    });
                    api.patch_status(
                        name,
                        &PatchParams::apply("wren-controller"),
                        &Patch::Merge(&status),
                    )
                    .await
                    .map_err(WrenError::KubeError)?;

                    ctx.metrics
                        .record_job_state_change(&spec.queue, "Scheduling", "Running");
                    ctx.metrics
                        .scheduling_attempts
                        .with_label_values(&["success"])
                        .inc();
                }
                Err(e) => {
                    // Release reservation on failure
                    {
                        let mut reservations = ctx.reservations.write().await;
                        reservations.release(name);
                        ctx.metrics.record_reservation_end();
                    }

                    error!(job = name, error = %e, "failed to launch job");
                    // Release allocations
                    let mut cluster = ctx.cluster_state.write().await;
                    for node in &placement.nodes {
                        cluster.deallocate(node, cpu_per_node, mem_per_node, gpus_per_node, name);
                    }
                    update_status(
                        name,
                        namespace,
                        JobState::Failed,
                        Some(&format!("launch failed: {}", e)),
                        ctx,
                    )
                    .await?;
                    ctx.metrics
                        .scheduling_attempts
                        .with_label_values(&["failed"])
                        .inc();
                }
            }
        }
        Err(WrenError::NoFeasiblePlacement { .. }) => {
            let scheduling_secs = scheduling_start.elapsed().as_secs_f64();
            ctx.metrics
                .record_scheduling_latency("no_placement", scheduling_secs);
            // Stay in Scheduling state — will retry on next reconcile
            info!(job = name, "no feasible placement, will retry");
            ctx.metrics
                .scheduling_attempts
                .with_label_values(&["no_placement"])
                .inc();

            // Clean up allocations for jobs that no longer exist (e.g. deleted
            // via kubectl). This prevents resource leaks when handle_terminal
            // is never called.
            cleanup_stale_allocations(namespace, ctx).await;
        }
        Err(e) => {
            let scheduling_secs = scheduling_start.elapsed().as_secs_f64();
            ctx.metrics
                .record_scheduling_latency("error", scheduling_secs);
            error!(job = name, error = %e, "scheduling error");
            update_status(
                name,
                namespace,
                JobState::Failed,
                Some(&format!("scheduling error: {}", e)),
                ctx,
            )
            .await?;
        }
    }

    Ok(())
}

async fn handle_running(
    name: &str,
    namespace: &str,
    spec: &wren_core::WrenJobSpec,
    job_status: &WrenJobStatus,
    ctx: &ReconcilerContext,
) -> Result<(), WrenError> {
    // Check backend status
    let backend_status = ctx.backend.status(name, namespace).await?;

    match backend_status {
        BackendJobStatus::Succeeded => {
            info!(job = name, "job completed successfully");
            let api: Api<WrenJob> = Api::namespaced(ctx.client.clone(), namespace);
            let now = chrono::Utc::now().to_rfc3339();
            let status = serde_json::json!({
                "status": {
                    "state": "Succeeded",
                    "message": "Job completed successfully",
                    "completionTime": now,
                }
            });
            api.patch_status(
                name,
                &PatchParams::apply("wren-controller"),
                &Patch::Merge(&status),
            )
            .await
            .map_err(WrenError::KubeError)?;
            ctx.metrics
                .record_job_state_change(&spec.queue, "Running", "Succeeded");
        }
        BackendJobStatus::Failed { message } => {
            warn!(job = name, reason = %message, "job failed");
            update_status(name, namespace, JobState::Failed, Some(&message), ctx).await?;
            ctx.metrics
                .record_job_state_change(&spec.queue, "Running", "Failed");
        }
        BackendJobStatus::Running => {
            // Check walltime
            if let Some(msg) =
                check_walltime_exceeded(spec.walltime.as_deref(), job_status.start_time.as_deref())
            {
                warn!(job = name, "walltime exceeded, terminating");
                ctx.backend.terminate(name, namespace).await?;
                update_status(name, namespace, JobState::WalltimeExceeded, Some(&msg), ctx).await?;
                ctx.metrics
                    .record_job_state_change(&spec.queue, "Running", "WalltimeExceeded");
            }
        }
        BackendJobStatus::Launching { ready, total } => {
            // Update ready worker count
            let api: Api<WrenJob> = Api::namespaced(ctx.client.clone(), namespace);
            let status = serde_json::json!({
                "status": {
                    "readyWorkers": ready,
                    "totalWorkers": total,
                }
            });
            api.patch_status(
                name,
                &PatchParams::apply("wren-controller"),
                &Patch::Merge(&status),
            )
            .await
            .map_err(WrenError::KubeError)?;
        }
        BackendJobStatus::NotFound => {
            warn!(job = name, "backend resources not found for running job");
            update_status(
                name,
                namespace,
                JobState::Failed,
                Some("backend resources disappeared"),
                ctx,
            )
            .await?;
        }
    }

    Ok(())
}

/// Validate a WrenJob spec, returning an error message if invalid.
pub(crate) fn validate_job_spec(spec: &wren_core::WrenJobSpec) -> Result<(), String> {
    if spec.nodes == 0 {
        return Err("nodes must be > 0".to_string());
    }
    if spec.backend == wren_core::ExecutionBackendType::Container && spec.container.is_none() {
        return Err("container spec required for container backend".to_string());
    }
    Ok(())
}

/// Check whether a job's walltime has been exceeded.
/// Returns Some(message) if exceeded, None otherwise.
pub(crate) fn check_walltime_exceeded(
    walltime_str: Option<&str>,
    start_time_str: Option<&str>,
) -> Option<String> {
    let wt_str = walltime_str?;
    let wt = WalltimeDuration::parse(wt_str).ok()?;
    let start_str = start_time_str?;
    let start = chrono::DateTime::parse_from_rfc3339(start_str).ok()?;
    let elapsed = chrono::Utc::now() - start.with_timezone(&chrono::Utc);
    if elapsed.num_seconds() as u64 > wt.seconds {
        Some(format!("walltime of {} exceeded", wt))
    } else {
        None
    }
}

/// Check whether completed pods should be cleaned up based on the TTL.
/// Uses completion_time if available, otherwise start_time.
pub(crate) fn should_cleanup_pods(status: &WrenJobStatus) -> bool {
    let finished_at = status
        .completion_time
        .as_deref()
        .or(status.start_time.as_deref());

    if let Some(time_str) = finished_at {
        if let Ok(finished) = chrono::DateTime::parse_from_rfc3339(time_str) {
            let elapsed = chrono::Utc::now() - finished.with_timezone(&chrono::Utc);
            return elapsed.num_seconds() as u64 > POD_TTL_AFTER_FINISHED.as_secs();
        }
    }
    false
}

/// Collect unique job names from cluster allocations.
pub(crate) fn collect_allocated_job_names(cluster: &ClusterState) -> Vec<String> {
    let mut names = std::collections::HashSet::new();
    for alloc in cluster.allocations.values() {
        for job in &alloc.jobs {
            names.insert(job.clone());
        }
    }
    names.into_iter().collect()
}

/// How long completed pods are preserved for log retrieval before deletion.
const POD_TTL_AFTER_FINISHED: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60); // 24 hours

async fn handle_terminal(
    name: &str,
    namespace: &str,
    status: &WrenJobStatus,
    ctx: &ReconcilerContext,
) -> Result<(), WrenError> {
    // Ensure backend resources (services, configmaps) are cleaned up.
    // Pods are intentionally kept for log retrieval.
    if let Err(e) = ctx.backend.cleanup(name, namespace).await {
        warn!(job = name, error = %e, "failed to clean up backend resources");
    }

    // Release cluster allocations
    // TODO: track per-job allocations properly instead of fixed defaults
    {
        let mut cluster = ctx.cluster_state.write().await;
        let nodes_to_release: Vec<String> = cluster
            .allocations
            .iter()
            .filter(|(_, alloc)| alloc.jobs.contains(&name.to_string()))
            .map(|(node_name, _)| node_name.clone())
            .collect();

        for node in nodes_to_release {
            cluster.deallocate(&node, 1000, 1_000_000_000, 0, name);
        }
    }

    // Check if pods should be cleaned up based on TTL.
    if should_cleanup_pods(status) {
        info!(job = name, "pod TTL expired, cleaning up pods");
        if let Err(e) = ctx.backend.terminate(name, namespace).await {
            warn!(job = name, error = %e, "failed to delete expired pods");
        }
    }

    Ok(())
}

/// Remove allocations for jobs that no longer exist in the K8s API.
///
/// When a WrenJob is deleted directly (e.g. `kubectl delete`), the controller
/// never runs `handle_terminal` and the in-memory allocations are leaked. This
/// function detects and releases those stale allocations so the resources become
/// available for new jobs.
async fn cleanup_stale_allocations(namespace: &str, ctx: &ReconcilerContext) {
    let api: Api<WrenJob> = Api::namespaced(ctx.client.clone(), namespace);

    // Collect all unique job names referenced in allocations.
    let job_names: Vec<String> = {
        let cluster = ctx.cluster_state.read().await;
        collect_allocated_job_names(&cluster)
    };

    for job_name in &job_names {
        match api.get_opt(job_name).await {
            Ok(None) => {
                // Job no longer exists — release its allocations.
                info!(job = %job_name, "releasing stale allocations for deleted job");
                let mut cluster = ctx.cluster_state.write().await;
                let nodes_to_release: Vec<String> = cluster
                    .allocations
                    .iter()
                    .filter(|(_, alloc)| alloc.jobs.contains(job_name))
                    .map(|(node_name, _)| node_name.clone())
                    .collect();
                for node in nodes_to_release {
                    cluster.deallocate(&node, 1000, 1_000_000_000, 0, job_name);
                }
            }
            Ok(Some(_)) => {
                // Job still exists — nothing to do.
            }
            Err(e) => {
                warn!(job = %job_name, error = %e, "failed to check job existence for stale allocation cleanup");
            }
        }
    }
}

/// Helper to patch the WrenJob status subresource.
async fn update_status(
    name: &str,
    namespace: &str,
    state: JobState,
    message: Option<&str>,
    ctx: &ReconcilerContext,
) -> Result<(), WrenError> {
    let api: Api<WrenJob> = Api::namespaced(ctx.client.clone(), namespace);
    let status = serde_json::json!({
        "status": {
            "state": state,
            "message": message,
        }
    });
    api.patch_status(
        name,
        &PatchParams::apply("wren-controller"),
        &Patch::Merge(&status),
    )
    .await
    .map_err(WrenError::KubeError)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_core::{ClusterState, ContainerSpec, ExecutionBackendType, NodeAllocation};

    fn make_spec(
        nodes: u32,
        backend: ExecutionBackendType,
        container: Option<ContainerSpec>,
    ) -> wren_core::WrenJobSpec {
        wren_core::WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes,
            tasks_per_node: 1,
            backend,
            container,
            reaper: None,
            mpi: None,
            topology: None,
            dependencies: vec![],
        }
    }

    fn make_container_spec() -> ContainerSpec {
        ContainerSpec {
            image: "busybox".to_string(),
            command: vec![],
            args: vec![],
            resources: None,
            host_network: false,
            volume_mounts: vec![],
            env: vec![],
        }
    }

    // --- validate_job_spec tests ---

    #[test]
    fn test_validate_job_spec_valid() {
        let spec = make_spec(
            2,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        assert!(validate_job_spec(&spec).is_ok());
    }

    #[test]
    fn test_validate_job_spec_zero_nodes() {
        let spec = make_spec(
            0,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let err = validate_job_spec(&spec).unwrap_err();
        assert!(err.contains("nodes must be > 0"));
    }

    #[test]
    fn test_validate_job_spec_container_backend_no_spec() {
        let spec = make_spec(2, ExecutionBackendType::Container, None);
        let err = validate_job_spec(&spec).unwrap_err();
        assert!(err.contains("container spec required"));
    }

    #[test]
    fn test_validate_job_spec_reaper_backend_no_container_ok() {
        let spec = make_spec(2, ExecutionBackendType::Reaper, None);
        assert!(validate_job_spec(&spec).is_ok());
    }

    #[test]
    fn test_validate_job_spec_single_node() {
        let spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        assert!(validate_job_spec(&spec).is_ok());
    }

    // --- check_walltime_exceeded tests ---

    #[test]
    fn test_check_walltime_no_walltime() {
        assert!(check_walltime_exceeded(None, Some("2024-01-01T00:00:00Z")).is_none());
    }

    #[test]
    fn test_check_walltime_no_start_time() {
        assert!(check_walltime_exceeded(Some("1h"), None).is_none());
    }

    #[test]
    fn test_check_walltime_not_exceeded() {
        // Start time is now, walltime is 1h - should not be exceeded
        let now = chrono::Utc::now().to_rfc3339();
        assert!(check_walltime_exceeded(Some("1h"), Some(&now)).is_none());
    }

    #[test]
    fn test_check_walltime_exceeded() {
        // Start time is 2 hours ago, walltime is 1h - should be exceeded
        let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let result = check_walltime_exceeded(Some("1h"), Some(&two_hours_ago));
        assert!(result.is_some());
        assert!(result.unwrap().contains("walltime of"));
    }

    #[test]
    fn test_check_walltime_invalid_walltime_string() {
        let now = chrono::Utc::now().to_rfc3339();
        assert!(check_walltime_exceeded(Some("not-a-duration"), Some(&now)).is_none());
    }

    #[test]
    fn test_check_walltime_invalid_start_time() {
        assert!(check_walltime_exceeded(Some("1h"), Some("not-a-timestamp")).is_none());
    }

    #[test]
    fn test_check_walltime_both_none() {
        assert!(check_walltime_exceeded(None, None).is_none());
    }

    // --- should_cleanup_pods tests ---

    #[test]
    fn test_should_cleanup_no_times() {
        let status = WrenJobStatus::default();
        assert!(!should_cleanup_pods(&status));
    }

    #[test]
    fn test_should_cleanup_recent_completion() {
        let status = WrenJobStatus {
            completion_time: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        assert!(!should_cleanup_pods(&status));
    }

    #[test]
    fn test_should_cleanup_old_completion() {
        // 25 hours ago (TTL is 24h)
        let old = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        let status = WrenJobStatus {
            completion_time: Some(old),
            ..Default::default()
        };
        assert!(should_cleanup_pods(&status));
    }

    #[test]
    fn test_should_cleanup_falls_back_to_start_time() {
        let old = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        let status = WrenJobStatus {
            completion_time: None,
            start_time: Some(old),
            ..Default::default()
        };
        assert!(should_cleanup_pods(&status));
    }

    #[test]
    fn test_should_cleanup_invalid_time() {
        let status = WrenJobStatus {
            completion_time: Some("invalid".to_string()),
            ..Default::default()
        };
        assert!(!should_cleanup_pods(&status));
    }

    // --- collect_allocated_job_names tests ---

    #[test]
    fn test_collect_allocated_job_names_empty() {
        let cluster = ClusterState::default();
        let names = collect_allocated_job_names(&cluster);
        assert!(names.is_empty());
    }

    #[test]
    fn test_collect_allocated_job_names_with_allocations() {
        let mut cluster = ClusterState::default();
        cluster.allocations.insert(
            "node-0".to_string(),
            NodeAllocation {
                used_cpu_millis: 1000,
                used_memory_bytes: 1_000_000_000,
                used_gpus: 0,
                jobs: vec!["job-a".to_string(), "job-b".to_string()],
            },
        );
        cluster.allocations.insert(
            "node-1".to_string(),
            NodeAllocation {
                used_cpu_millis: 1000,
                used_memory_bytes: 1_000_000_000,
                used_gpus: 0,
                jobs: vec!["job-a".to_string()],
            },
        );
        let mut names = collect_allocated_job_names(&cluster);
        names.sort();
        assert_eq!(names, vec!["job-a", "job-b"]);
    }
}
