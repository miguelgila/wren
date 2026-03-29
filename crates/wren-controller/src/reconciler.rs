use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use wren_core::backend::{BackendJobStatus, ExecutionBackend};
use wren_core::{
    ClusterState, ExecutionBackendType, JobState, UserIdentity, WalltimeDuration, WrenError,
    WrenJob, WrenJobStatus,
};
use wren_scheduler::gang::GangScheduler;

use crate::job_id::JobIdAllocator;
use crate::metrics::Metrics;
use crate::reservation::ReservationManager;

/// Context shared across reconciliation calls.
pub struct ReconcilerContext {
    pub client: Client,
    pub cluster_state: Arc<RwLock<ClusterState>>,
    pub container_backend: Arc<dyn ExecutionBackend>,
    pub reaper_backend: Arc<dyn ExecutionBackend>,
    pub metrics: Metrics,
    pub reservations: RwLock<ReservationManager>,
    pub job_id_allocator: JobIdAllocator,
}

impl ReconcilerContext {
    /// Select the appropriate backend based on the job's backend type.
    pub fn backend_for(&self, backend_type: &ExecutionBackendType) -> &Arc<dyn ExecutionBackend> {
        match backend_type {
            ExecutionBackendType::Container => &self.container_backend,
            ExecutionBackendType::Reaper => &self.reaper_backend,
        }
    }
}

/// Reconcile a single WrenJob. Called by the controller whenever the resource changes.
pub async fn reconcile(job: &WrenJob, ctx: &ReconcilerContext) -> Result<(), WrenError> {
    let name = job.metadata.name.as_deref().unwrap_or("unknown");
    let namespace = job.metadata.namespace.as_deref().unwrap_or("default");

    let status = job.status.as_ref().cloned().unwrap_or_default();
    let spec = &job.spec;

    // Resolve user identity from wren.giar.dev/user annotation
    let user = crate::user_identity::resolve_user_identity(
        &ctx.client,
        job.metadata.annotations.as_ref(),
    )
    .await
    .unwrap_or_else(|e| {
        warn!(job = name, error = %e, "failed to resolve user identity, continuing without");
        None
    });

    info!(
        job = name,
        namespace,
        state = %status.state,
        user = user.as_ref().map(|u| u.username.as_str()).unwrap_or("<none>"),
        "reconciling WrenJob"
    );

    let backend = ctx.backend_for(&spec.backend);

    match status.state {
        JobState::Pending => handle_pending(name, namespace, spec, ctx).await,
        JobState::Scheduling => {
            handle_scheduling(name, namespace, spec, user.as_ref(), backend, ctx).await
        }
        JobState::Running => handle_running(name, namespace, spec, &status, backend, ctx).await,
        JobState::Succeeded
        | JobState::Failed
        | JobState::Cancelled
        | JobState::WalltimeExceeded => {
            // Terminal states — clean up resources, preserve pods for log TTL
            handle_terminal(name, namespace, &status, backend, ctx).await
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

    // Allocate a sequential job ID
    let job_id = ctx.job_id_allocator.allocate().await?;
    info!(job = name, job_id, "assigned job ID");

    // Transition to Scheduling with the assigned job ID
    let api: Api<WrenJob> = Api::namespaced(ctx.client.clone(), namespace);
    let status = serde_json::json!({
        "status": {
            "jobId": job_id,
            "state": "Scheduling",
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
        .record_job_state_change(&spec.queue, "Pending", "Scheduling");
    Ok(())
}

async fn handle_scheduling(
    name: &str,
    namespace: &str,
    spec: &wren_core::WrenJobSpec,
    user: Option<&UserIdentity>,
    backend: &Arc<dyn ExecutionBackend>,
    ctx: &ReconcilerContext,
) -> Result<(), WrenError> {
    // Security: every job MUST have a resolved, non-root user identity.
    // This prevents anonymous or root execution on both container and bare-metal backends.
    if user.is_none() {
        let reason = "no valid WrenUser identity resolved — every job requires a \
                       wren.giar.dev/user annotation pointing to an existing WrenUser with non-root uid";
        warn!(job = name, "rejecting job: {}", reason);
        update_status(name, namespace, JobState::Failed, Some(reason), ctx).await?;
        return Ok(());
    }

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
            match backend
                .launch(name, namespace, spec, &placement, user)
                .await
            {
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
    backend: &Arc<dyn ExecutionBackend>,
    ctx: &ReconcilerContext,
) -> Result<(), WrenError> {
    // Check backend status
    let backend_status = backend.status(name, namespace).await?;

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
                backend.terminate(name, namespace).await?;
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
    backend: &Arc<dyn ExecutionBackend>,
    ctx: &ReconcilerContext,
) -> Result<(), WrenError> {
    // Ensure backend resources (services, configmaps) are cleaned up.
    // Pods are intentionally kept for log retrieval.
    if let Err(e) = backend.cleanup(name, namespace).await {
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
        if let Err(e) = backend.terminate(name, namespace).await {
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
    use async_trait::async_trait;
    use bytes::Bytes;
    use http_body_util::Full;
    use kube::api::ObjectMeta;
    use std::sync::Mutex;
    use tower::service_fn;
    use wren_core::backend::{BackendJobStatus, ExecutionBackend, LaunchResult};
    use wren_core::{
        ClusterState, ContainerSpec, ExecutionBackendType, NodeAllocation, NodeResources,
    };

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
            project: None,
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

    #[test]
    fn test_collect_allocated_job_names_single_job_single_node() {
        let mut cluster = ClusterState::default();
        cluster.allocations.insert(
            "node-0".to_string(),
            NodeAllocation {
                used_cpu_millis: 2000,
                used_memory_bytes: 2_000_000_000,
                used_gpus: 4,
                jobs: vec!["job-gpu".to_string()],
            },
        );
        let names = collect_allocated_job_names(&cluster);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], "job-gpu");
    }

    #[test]
    fn test_collect_allocated_job_names_node_with_empty_job_list() {
        let mut cluster = ClusterState::default();
        cluster.allocations.insert(
            "node-0".to_string(),
            NodeAllocation {
                used_cpu_millis: 0,
                used_memory_bytes: 0,
                used_gpus: 0,
                jobs: vec![],
            },
        );
        let names = collect_allocated_job_names(&cluster);
        assert!(names.is_empty());
    }

    #[test]
    fn test_collect_allocated_job_names_deduplicates_same_job_across_nodes() {
        // A multi-node gang job appears in allocations on every node it uses —
        // the result must contain it only once.
        let mut cluster = ClusterState::default();
        for node in ["node-0", "node-1", "node-2", "node-3"] {
            cluster.allocations.insert(
                node.to_string(),
                NodeAllocation {
                    used_cpu_millis: 1000,
                    used_memory_bytes: 1_000_000_000,
                    used_gpus: 0,
                    jobs: vec!["gang-job".to_string()],
                },
            );
        }
        let names = collect_allocated_job_names(&cluster);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], "gang-job");
    }

    // --- validate_job_spec additional edge-case tests ---

    #[test]
    fn test_validate_job_spec_reaper_backend_with_reaper_spec_ok() {
        // Providing a ReaperSpec is optional for the reaper backend but should
        // not cause validation to fail when it is present.
        use wren_core::ReaperSpec;
        let mut spec = make_spec(4, ExecutionBackendType::Reaper, None);
        spec.reaper = Some(ReaperSpec {
            script: Some("#!/bin/bash\n./app".to_string()),
            ..Default::default()
        });
        assert!(validate_job_spec(&spec).is_ok());
    }

    #[test]
    fn test_validate_job_spec_container_backend_with_both_specs_ok() {
        // Having both a container spec and a reaper spec set is unusual but the
        // validator only enforces that the container spec is present for the
        // Container backend — extra fields are not rejected.
        use wren_core::ReaperSpec;
        let mut spec = make_spec(
            2,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        spec.reaper = Some(ReaperSpec::default());
        assert!(validate_job_spec(&spec).is_ok());
    }

    #[test]
    fn test_validate_job_spec_large_node_count_ok() {
        // The validator has no upper-bound check; large node counts are allowed
        // and will fail later at scheduling time if the cluster is too small.
        let spec = make_spec(
            1024,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        assert!(validate_job_spec(&spec).is_ok());
    }

    #[test]
    fn test_validate_job_spec_error_message_mentions_container_spec() {
        // Verify the error text is actionable — it tells the user what is missing.
        let spec = make_spec(1, ExecutionBackendType::Container, None);
        let err = validate_job_spec(&spec).unwrap_err();
        assert!(
            err.contains("container spec"),
            "expected error to mention 'container spec', got: {err}"
        );
    }

    #[test]
    fn test_validate_job_spec_error_message_mentions_nodes() {
        // Verify the zero-nodes error is actionable.
        let spec = make_spec(0, ExecutionBackendType::Reaper, None);
        let err = validate_job_spec(&spec).unwrap_err();
        assert!(
            err.contains("nodes"),
            "expected error to mention 'nodes', got: {err}"
        );
    }

    // --- check_walltime_exceeded additional format/boundary tests ---

    #[test]
    fn test_check_walltime_exceeded_seconds_format() {
        // "3600" (bare integer) is a valid walltime meaning 3600 seconds.
        // A job started 2 hours ago has exceeded a 3600-second walltime.
        let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let result = check_walltime_exceeded(Some("3600"), Some(&two_hours_ago));
        assert!(
            result.is_some(),
            "bare-seconds walltime should be recognised"
        );
    }

    #[test]
    fn test_check_walltime_not_exceeded_seconds_format() {
        // A job started just now has not exceeded a 1-hour walltime expressed
        // as bare seconds.
        let now = chrono::Utc::now().to_rfc3339();
        assert!(check_walltime_exceeded(Some("3600"), Some(&now)).is_none());
    }

    #[test]
    fn test_check_walltime_exceeded_days_format() {
        // "1d" = 86400 s. A job started 2 days ago should be exceeded.
        let two_days_ago = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
        let result = check_walltime_exceeded(Some("1d"), Some(&two_days_ago));
        assert!(result.is_some(), "days walltime should be recognised");
    }

    #[test]
    fn test_check_walltime_exceeded_mixed_format() {
        // "1h30m" = 5400 s. A job started 2 hours ago should be exceeded.
        let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let result = check_walltime_exceeded(Some("1h30m"), Some(&two_hours_ago));
        assert!(result.is_some());
    }

    #[test]
    fn test_check_walltime_not_exceeded_mixed_format() {
        // "2h30m" = 9000 s. A job started 1 hour ago should NOT be exceeded.
        let one_hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert!(check_walltime_exceeded(Some("2h30m"), Some(&one_hour_ago)).is_none());
    }

    #[test]
    fn test_check_walltime_exceeded_message_contains_duration() {
        // The returned message must include the walltime string so operators can
        // understand why the job was terminated.
        let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let msg = check_walltime_exceeded(Some("1h"), Some(&two_hours_ago)).unwrap();
        assert!(
            msg.contains("walltime"),
            "message should mention 'walltime', got: {msg}"
        );
    }

    // --- should_cleanup_pods additional boundary tests ---

    #[test]
    fn test_should_cleanup_completion_time_takes_precedence_over_start_time() {
        // completion_time is recent (should NOT trigger cleanup) but start_time
        // is old. completion_time must win — pods should be kept.
        let old = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        let recent = chrono::Utc::now().to_rfc3339();
        let status = WrenJobStatus {
            completion_time: Some(recent),
            start_time: Some(old),
            ..Default::default()
        };
        assert!(
            !should_cleanup_pods(&status),
            "recent completion_time should prevent cleanup even if start_time is old"
        );
    }

    #[test]
    fn test_should_cleanup_pods_just_under_ttl() {
        // 23 hours — within the 24-hour TTL — must not trigger cleanup.
        let just_under = (chrono::Utc::now() - chrono::Duration::hours(23)).to_rfc3339();
        let status = WrenJobStatus {
            completion_time: Some(just_under),
            ..Default::default()
        };
        assert!(!should_cleanup_pods(&status));
    }

    #[test]
    fn test_should_cleanup_pods_well_past_ttl() {
        // 48 hours — well past the 24-hour TTL — must trigger cleanup.
        let two_days_ago = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        let status = WrenJobStatus {
            completion_time: Some(two_days_ago),
            ..Default::default()
        };
        assert!(should_cleanup_pods(&status));
    }

    // =========================================================================
    // Async tests for reconcile(), handle_pending(), handle_scheduling(),
    // handle_running(), handle_terminal(), cleanup_stale_allocations(),
    // update_status()
    // =========================================================================

    // --- Mock infrastructure ---

    /// A configurable mock ExecutionBackend for unit testing the reconciler.
    struct MockBackend {
        launch_result: Mutex<Option<Result<LaunchResult, WrenError>>>,
        status_result: Mutex<BackendJobStatus>,
        terminate_called: Mutex<bool>,
        cleanup_called: Mutex<bool>,
    }

    impl MockBackend {
        /// Construct a mock that always succeeds on launch and returns `status`
        /// from `status()`. Returns an `Arc<dyn ExecutionBackend>` so it can be
        /// passed directly to `make_ctx` without extra casts.
        fn new_ok(status: BackendJobStatus) -> Arc<dyn ExecutionBackend> {
            Arc::new(Self {
                launch_result: Mutex::new(Some(Ok(LaunchResult {
                    resource_ids: vec!["pod-0".to_string()],
                    message: "launched".to_string(),
                }))),
                status_result: Mutex::new(status),
                terminate_called: Mutex::new(false),
                cleanup_called: Mutex::new(false),
            })
        }

        /// Construct a mock whose `launch()` returns the given error.
        fn new_launch_err(err: WrenError) -> Arc<dyn ExecutionBackend> {
            Arc::new(Self {
                launch_result: Mutex::new(Some(Err(err))),
                status_result: Mutex::new(BackendJobStatus::Running),
                terminate_called: Mutex::new(false),
                cleanup_called: Mutex::new(false),
            })
        }
    }

    // Helper accessors via raw pointer — the Arc vtable pointer points to the
    // concrete type, so this is safe as long as every `Arc<dyn ExecutionBackend>`
    // in these tests is backed by a `MockBackend` (which they all are).
    fn terminate_called(b: &Arc<dyn ExecutionBackend>) -> bool {
        let raw: *const MockBackend = Arc::as_ptr(b) as *const MockBackend;
        unsafe { *(*raw).terminate_called.lock().unwrap() }
    }

    fn cleanup_called(b: &Arc<dyn ExecutionBackend>) -> bool {
        let raw: *const MockBackend = Arc::as_ptr(b) as *const MockBackend;
        unsafe { *(*raw).cleanup_called.lock().unwrap() }
    }

    #[async_trait]
    impl ExecutionBackend for MockBackend {
        async fn launch(
            &self,
            _job_name: &str,
            _namespace: &str,
            _spec: &wren_core::WrenJobSpec,
            _placement: &wren_core::Placement,
            _user: Option<&UserIdentity>,
        ) -> Result<LaunchResult, WrenError> {
            let mut guard = self.launch_result.lock().unwrap();
            guard.take().unwrap_or(Ok(LaunchResult {
                resource_ids: vec![],
                message: "default".to_string(),
            }))
        }

        async fn status(
            &self,
            _job_name: &str,
            _namespace: &str,
        ) -> Result<BackendJobStatus, WrenError> {
            Ok(self.status_result.lock().unwrap().clone())
        }

        async fn terminate(&self, _job_name: &str, _namespace: &str) -> Result<(), WrenError> {
            *self.terminate_called.lock().unwrap() = true;
            Ok(())
        }

        async fn cleanup(&self, _job_name: &str, _namespace: &str) -> Result<(), WrenError> {
            *self.cleanup_called.lock().unwrap() = true;
            Ok(())
        }
    }

    /// Route a mock request to a JSON body appropriate for its resource type.
    fn route_ok_response(method: &str, path: &str) -> (u16, Bytes) {
        // WrenUser lookups → 404 (no user by default)
        if path.contains("wrenusers") {
            return (
                404,
                Bytes::from(
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}"#,
                ),
            );
        }
        // ConfigMap operations (job-id counter)
        if path.contains("configmaps") {
            return (
                200,
                Bytes::from(
                    r#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"wren-job-id-counter","namespace":"default"},"data":{"next_job_id":"1"}}"#,
                ),
            );
        }
        // WrenJob status patches and gets
        if path.contains("wrenjobs") {
            return (
                200,
                Bytes::from(
                    r#"{"apiVersion":"wren.giar.dev/v1alpha1","kind":"WrenJob","metadata":{"name":"test","namespace":"default"},"spec":{"queue":"default","priority":50,"nodes":1,"tasksPerNode":1,"backend":"container","container":{"image":"busybox","command":[],"args":[],"hostNetwork":false,"volumeMounts":[],"env":[]},"dependencies":[]}}"#,
                ),
            );
        }
        let _ = method; // suppress unused warning
        (200, Bytes::from(r#"{"metadata":{}}"#))
    }

    /// Build a kube::Client that returns valid responses for all common operations.
    fn make_ok_client() -> Client {
        let svc = service_fn(move |req: http::Request<_>| {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            async move {
                let (status, body) = route_ok_response(&method, &path);
                let resp = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Full::from(body))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        Client::new(svc, "default")
    }

    /// Build a kube::Client that returns 404 for GET requests on the given
    /// WrenJob name and valid responses for everything else.
    fn make_job_not_found_client(job_name: &str) -> Client {
        let target = job_name.to_string();
        let svc = service_fn(move |req: http::Request<_>| {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let is_not_found =
                method == "GET" && path.contains("wrenjobs") && path.contains(target.as_str());
            async move {
                let (status, body) = if is_not_found {
                    (
                        404u16,
                        Bytes::from(
                            r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}"#,
                        ),
                    )
                } else {
                    route_ok_response(&method, &path)
                };
                let resp = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Full::from(body))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        Client::new(svc, "default")
    }

    /// Build a cluster state with `n` nodes, each with 4 CPUs and 4 GB RAM.
    fn make_cluster_with_nodes(n: usize) -> ClusterState {
        let mut cluster = ClusterState::default();
        for i in 0..n {
            cluster.nodes.push(NodeResources {
                name: format!("node-{}", i),
                allocatable_cpu_millis: 4000,
                allocatable_memory_bytes: 4_000_000_000,
                allocatable_gpus: 0,
                labels: std::collections::HashMap::new(),
                switch_group: None,
                rack: None,
            });
        }
        cluster
    }

    /// Build a test UserIdentity for scheduling tests.
    fn make_test_user() -> UserIdentity {
        UserIdentity {
            username: "testuser".to_string(),
            uid: 1001,
            gid: 1001,
            supplemental_groups: vec![],
            home_dir: None,
            default_project: None,
        }
    }

    /// Build a ReconcilerContext with the given client and backends.
    fn make_ctx(
        client: Client,
        container_backend: Arc<dyn ExecutionBackend>,
        reaper_backend: Arc<dyn ExecutionBackend>,
        cluster: ClusterState,
    ) -> ReconcilerContext {
        ReconcilerContext {
            job_id_allocator: JobIdAllocator::new(client.clone(), "default"),
            client,
            cluster_state: Arc::new(RwLock::new(cluster)),
            container_backend,
            reaper_backend,
            metrics: Metrics::new(),
            reservations: RwLock::new(ReservationManager::default()),
        }
    }

    /// Build a minimal WrenJob object for testing.
    fn make_wrenjob(name: &str, state: JobState, spec: wren_core::WrenJobSpec) -> WrenJob {
        WrenJob {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some("default".to_string()),
                annotations: None,
                ..Default::default()
            },
            spec,
            status: Some(WrenJobStatus {
                state,
                ..Default::default()
            }),
        }
    }

    // --- handle_pending tests ---

    #[tokio::test]
    async fn test_handle_pending_invalid_job_zero_nodes() {
        // A job with 0 nodes must transition directly to Failed (no Err return).
        let spec = make_spec(
            0,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let result = handle_pending("job-zero", "default", &spec, &ctx).await;
        assert!(result.is_ok(), "handle_pending should not return Err");
        assert!(!cleanup_called(&backend));
    }

    #[tokio::test]
    async fn test_handle_pending_invalid_job_no_container_spec() {
        // Container backend without container spec -> Failed.
        let spec = make_spec(2, ExecutionBackendType::Container, None);
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let result = handle_pending("job-nospec", "default", &spec, &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_pending_valid_job_transitions_to_scheduling() {
        // A valid container job must get through validate_job_spec and allocate
        // a job ID without returning an error.
        let spec = make_spec(
            2,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let result = handle_pending("job-valid", "default", &spec, &ctx).await;
        assert!(
            result.is_ok(),
            "valid pending job should not error: {:?}",
            result
        );
    }

    // --- handle_scheduling tests ---

    #[tokio::test]
    async fn test_handle_scheduling_successful_placement() {
        // 2-node job on a 2-node cluster -> launch succeeds.
        let spec = make_spec(
            2,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let user = make_test_user();
        let result =
            handle_scheduling("job-ok", "default", &spec, Some(&user), &backend, &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_scheduling_no_feasible_placement() {
        // Requesting 5 nodes on a 2-node cluster -- no placement found, stays
        // in Scheduling (function returns Ok without error).
        let spec = make_spec(
            5,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let user = make_test_user();
        let result = handle_scheduling(
            "job-no-nodes",
            "default",
            &spec,
            Some(&user),
            &backend,
            &ctx,
        )
        .await;
        assert!(result.is_ok(), "no-placement should not be an error");
    }

    #[tokio::test]
    async fn test_handle_scheduling_no_user_fails() {
        // Any backend without user identity -> must transition to Failed (Ok(())).
        let spec = make_spec(1, ExecutionBackendType::Reaper, None);
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let result =
            handle_scheduling("job-reaper-nouser", "default", &spec, None, &backend, &ctx).await;
        assert!(result.is_ok(), "rejection should be Ok(()), not Err");
        assert!(!cleanup_called(&backend));
        assert!(!terminate_called(&backend));
    }

    #[tokio::test]
    async fn test_handle_scheduling_container_no_user_fails() {
        // Container backend without user identity -> must also be rejected.
        let spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let result = handle_scheduling(
            "job-container-nouser",
            "default",
            &spec,
            None,
            &backend,
            &ctx,
        )
        .await;
        assert!(result.is_ok(), "rejection should be Ok(()), not Err");
        assert!(!cleanup_called(&backend));
        assert!(!terminate_called(&backend));
    }

    #[tokio::test]
    async fn test_handle_scheduling_reaper_with_user_ok() {
        // Reaper backend with a valid user identity proceeds to launch.
        let spec = make_spec(1, ExecutionBackendType::Reaper, None);
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let user = make_test_user();
        let result = handle_scheduling(
            "job-reaper-user",
            "default",
            &spec,
            Some(&user),
            &backend,
            &ctx,
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_scheduling_launch_failure_releases_allocations() {
        // When backend.launch() returns an error, the scheduler must release
        // the cluster allocations and transition the job to Failed.
        let spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_launch_err(WrenError::ValidationError {
            reason: "simulated launch failure".to_string(),
        });
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let user = make_test_user();
        let result = handle_scheduling(
            "job-fail-launch",
            "default",
            &spec,
            Some(&user),
            &backend,
            &ctx,
        )
        .await;
        assert!(
            result.is_ok(),
            "launch failure should be handled, not propagated"
        );
        // Allocations should have been released
        let cluster = ctx.cluster_state.read().await;
        for node in &cluster.nodes {
            let alloc = cluster.allocations.get(&node.name);
            let job_count = alloc.map(|a| a.jobs.len()).unwrap_or(0);
            assert_eq!(
                job_count, 0,
                "allocations for node {} should be released after launch failure",
                node.name
            );
        }
    }

    // --- handle_running tests ---

    #[tokio::test]
    async fn test_handle_running_succeeded() {
        // Backend reports Succeeded -> job should transition (no error).
        let spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Succeeded);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(1),
        );
        let status = WrenJobStatus {
            start_time: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let result = handle_running("job-done", "default", &spec, &status, &backend, &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_running_failed() {
        // Backend reports Failed -> job transitions to Failed.
        let spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Failed {
            message: "OOM killed".to_string(),
        });
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(1),
        );
        let status = WrenJobStatus {
            start_time: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let result = handle_running("job-oom", "default", &spec, &status, &backend, &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_running_walltime_exceeded() {
        // Running job whose start_time is in the past and walltime has expired
        // -> terminate must be called and status updated.
        let mut spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        spec.walltime = Some("1s".to_string());
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(1),
        );
        let old_start = (chrono::Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        let status = WrenJobStatus {
            start_time: Some(old_start),
            ..Default::default()
        };
        let result =
            handle_running("job-walltime", "default", &spec, &status, &backend, &ctx).await;
        assert!(result.is_ok());
        assert!(
            terminate_called(&backend),
            "terminate must be called when walltime is exceeded"
        );
    }

    #[tokio::test]
    async fn test_handle_running_launching_updates_ready_workers() {
        // Backend reports Launching -> status patch for readyWorkers (no error).
        let spec = make_spec(
            2,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Launching { ready: 1, total: 2 });
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let status = WrenJobStatus::default();
        let result =
            handle_running("job-launching", "default", &spec, &status, &backend, &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_running_not_found() {
        // Backend reports NotFound -> job must be transitioned to Failed.
        let spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::NotFound);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(1),
        );
        let status = WrenJobStatus::default();
        let result =
            handle_running("job-notfound", "default", &spec, &status, &backend, &ctx).await;
        assert!(result.is_ok());
    }

    // --- handle_terminal tests ---

    #[tokio::test]
    async fn test_handle_terminal_cleans_up_resources() {
        // Terminal state must call cleanup() on the backend.
        let backend = MockBackend::new_ok(BackendJobStatus::Succeeded);
        let mut cluster = make_cluster_with_nodes(2);
        cluster.allocate("node-0", 1000, 1_000_000_000, 0, "job-term");
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            cluster,
        );
        let status = WrenJobStatus {
            completion_time: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let result = handle_terminal("job-term", "default", &status, &backend, &ctx).await;
        assert!(result.is_ok());
        assert!(
            cleanup_called(&backend),
            "cleanup() must be called in terminal state"
        );
        let cluster = ctx.cluster_state.read().await;
        let alloc = cluster.allocations.get("node-0");
        let has_job = alloc
            .map(|a| a.jobs.contains(&"job-term".to_string()))
            .unwrap_or(false);
        assert!(
            !has_job,
            "job allocation should be released after terminal cleanup"
        );
    }

    #[tokio::test]
    async fn test_handle_terminal_ttl_expired_calls_terminate() {
        // When TTL has expired, terminate() must be called to delete pods.
        let backend = MockBackend::new_ok(BackendJobStatus::Succeeded);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(1),
        );
        let old = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        let status = WrenJobStatus {
            completion_time: Some(old),
            ..Default::default()
        };
        let result = handle_terminal("job-ttl", "default", &status, &backend, &ctx).await;
        assert!(result.is_ok());
        assert!(
            terminate_called(&backend),
            "terminate() must be called when pod TTL has expired"
        );
    }

    #[tokio::test]
    async fn test_handle_terminal_no_ttl_does_not_call_terminate() {
        // When TTL has NOT expired, terminate() must NOT be called.
        let backend = MockBackend::new_ok(BackendJobStatus::Succeeded);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(1),
        );
        let status = WrenJobStatus {
            completion_time: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let result = handle_terminal("job-no-ttl", "default", &status, &backend, &ctx).await;
        assert!(result.is_ok());
        assert!(
            !terminate_called(&backend),
            "terminate() must NOT be called when TTL has not expired"
        );
    }

    // --- reconcile() dispatch tests ---

    #[tokio::test]
    async fn test_reconcile_dispatches_pending_state() {
        // A Pending job goes through handle_pending (validation + ID allocation).
        let spec = make_spec(
            2,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(2),
        );
        let job = make_wrenjob("job-pending", JobState::Pending, spec);
        let result = reconcile(&job, &ctx).await;
        assert!(
            result.is_ok(),
            "reconcile of Pending job should not error: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_reconcile_dispatches_running_state() {
        // A Running job goes through handle_running (status check).
        let spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Succeeded);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(1),
        );
        let job = make_wrenjob("job-running", JobState::Running, spec);
        let result = reconcile(&job, &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_reconcile_dispatches_terminal_state() {
        // A Succeeded job goes through handle_terminal (cleanup).
        let spec = make_spec(
            1,
            ExecutionBackendType::Container,
            Some(make_container_spec()),
        );
        let backend = MockBackend::new_ok(BackendJobStatus::Succeeded);
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            make_cluster_with_nodes(1),
        );
        let job = make_wrenjob("job-succeeded", JobState::Succeeded, spec);
        let result = reconcile(&job, &ctx).await;
        assert!(result.is_ok());
        assert!(
            cleanup_called(&backend),
            "cleanup should be called for terminal state"
        );
    }

    // --- cleanup_stale_allocations tests ---

    #[tokio::test]
    async fn test_cleanup_stale_allocations_removes_deleted_jobs() {
        // When the K8s API returns 404 for a job that has allocations,
        // cleanup_stale_allocations must release those allocations.
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let mut cluster = make_cluster_with_nodes(2);
        cluster.allocate("node-0", 1000, 1_000_000_000, 0, "ghost-job");
        let ctx = make_ctx(
            make_job_not_found_client("ghost-job"),
            Arc::clone(&backend),
            Arc::clone(&backend),
            cluster,
        );
        cleanup_stale_allocations("default", &ctx).await;
        let cluster = ctx.cluster_state.read().await;
        let alloc = cluster.allocations.get("node-0");
        let has_job = alloc
            .map(|a| a.jobs.contains(&"ghost-job".to_string()))
            .unwrap_or(false);
        assert!(
            !has_job,
            "ghost-job allocation should be removed when job no longer exists in API"
        );
    }

    #[tokio::test]
    async fn test_cleanup_stale_allocations_keeps_existing_jobs() {
        // When the K8s API returns 200 for a job with allocations,
        // cleanup_stale_allocations must leave those allocations intact.
        let backend = MockBackend::new_ok(BackendJobStatus::Running);
        let mut cluster = make_cluster_with_nodes(2);
        cluster.allocate("node-0", 1000, 1_000_000_000, 0, "live-job");
        let ctx = make_ctx(
            make_ok_client(),
            Arc::clone(&backend),
            Arc::clone(&backend),
            cluster,
        );
        cleanup_stale_allocations("default", &ctx).await;
        let cluster = ctx.cluster_state.read().await;
        let alloc = cluster.allocations.get("node-0");
        let has_job = alloc
            .map(|a| a.jobs.contains(&"live-job".to_string()))
            .unwrap_or(false);
        assert!(
            has_job,
            "live-job allocation must be preserved when job still exists"
        );
    }
}
