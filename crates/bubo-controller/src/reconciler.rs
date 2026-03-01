use bubo_core::backend::{BackendJobStatus, ExecutionBackend};
use bubo_core::{
    BuboError, BuboJob, BuboJobStatus, ClusterState, JobState, WalltimeDuration,
};
use bubo_scheduler::gang::GangScheduler;
use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

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

/// Reconcile a single BuboJob. Called by the controller whenever the resource changes.
pub async fn reconcile(job: &BuboJob, ctx: &ReconcilerContext) -> Result<(), BuboError> {
    let name = job.metadata.name.as_deref().unwrap_or("unknown");
    let namespace = job
        .metadata
        .namespace
        .as_deref()
        .unwrap_or("default");

    let status = job.status.as_ref().cloned().unwrap_or_default();
    let spec = &job.spec;

    info!(
        job = name,
        namespace,
        state = %status.state,
        "reconciling BuboJob"
    );

    match status.state {
        JobState::Pending => handle_pending(name, namespace, spec, ctx).await,
        JobState::Scheduling => handle_scheduling(name, namespace, spec, ctx).await,
        JobState::Running => handle_running(name, namespace, spec, &status, ctx).await,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled | JobState::WalltimeExceeded => {
            // Terminal states — clean up resources, preserve pods for log TTL
            handle_terminal(name, namespace, &status, ctx).await
        }
    }
}

async fn handle_pending(
    name: &str,
    namespace: &str,
    spec: &bubo_core::BuboJobSpec,
    ctx: &ReconcilerContext,
) -> Result<(), BuboError> {
    // Validate the job spec
    if spec.nodes == 0 {
        update_status(
            name,
            namespace,
            JobState::Failed,
            Some("nodes must be > 0"),
            ctx,
        )
        .await?;
        return Ok(());
    }

    if spec.backend == bubo_core::ExecutionBackendType::Container && spec.container.is_none() {
        update_status(
            name,
            namespace,
            JobState::Failed,
            Some("container spec required for container backend"),
            ctx,
        )
        .await?;
        return Ok(());
    }

    // Transition to Scheduling
    update_status(name, namespace, JobState::Scheduling, None, ctx).await?;
    ctx.metrics.record_job_state_change(&spec.queue, "Pending", "Scheduling");
    Ok(())
}

async fn handle_scheduling(
    name: &str,
    namespace: &str,
    spec: &bubo_core::BuboJobSpec,
    ctx: &ReconcilerContext,
) -> Result<(), BuboError> {
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
            ctx.metrics.record_scheduling_latency("success", scheduling_secs);
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
                    let api: Api<BuboJob> = Api::namespaced(ctx.client.clone(), namespace);
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
                        &PatchParams::apply("bubo-controller"),
                        &Patch::Merge(&status),
                    )
                    .await
                    .map_err(BuboError::KubeError)?;

                    ctx.metrics.record_job_state_change(&spec.queue, "Scheduling", "Running");
                    ctx.metrics.scheduling_attempts.with_label_values(&["success"]).inc();
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
                    ctx.metrics.scheduling_attempts.with_label_values(&["failed"]).inc();
                }
            }
        }
        Err(BuboError::NoFeasiblePlacement { .. }) => {
            let scheduling_secs = scheduling_start.elapsed().as_secs_f64();
            ctx.metrics.record_scheduling_latency("no_placement", scheduling_secs);
            // Stay in Scheduling state — will retry on next reconcile
            info!(job = name, "no feasible placement, will retry");
            ctx.metrics.scheduling_attempts.with_label_values(&["no_placement"]).inc();
        }
        Err(e) => {
            let scheduling_secs = scheduling_start.elapsed().as_secs_f64();
            ctx.metrics.record_scheduling_latency("error", scheduling_secs);
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
    spec: &bubo_core::BuboJobSpec,
    job_status: &BuboJobStatus,
    ctx: &ReconcilerContext,
) -> Result<(), BuboError> {
    // Check backend status
    let backend_status = ctx.backend.status(name, namespace).await?;

    match backend_status {
        BackendJobStatus::Succeeded => {
            info!(job = name, "job completed successfully");
            let api: Api<BuboJob> = Api::namespaced(ctx.client.clone(), namespace);
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
                &PatchParams::apply("bubo-controller"),
                &Patch::Merge(&status),
            )
            .await
            .map_err(BuboError::KubeError)?;
            ctx.metrics.record_job_state_change(&spec.queue, "Running", "Succeeded");
        }
        BackendJobStatus::Failed { message } => {
            warn!(job = name, reason = %message, "job failed");
            update_status(
                name,
                namespace,
                JobState::Failed,
                Some(&message),
                ctx,
            )
            .await?;
            ctx.metrics.record_job_state_change(&spec.queue, "Running", "Failed");
        }
        BackendJobStatus::Running => {
            // Check walltime
            if let Some(ref wt_str) = spec.walltime {
                if let Ok(wt) = WalltimeDuration::parse(wt_str) {
                    if let Some(ref start_str) = job_status.start_time {
                        if let Ok(start) = chrono::DateTime::parse_from_rfc3339(start_str) {
                            let elapsed = chrono::Utc::now() - start.with_timezone(&chrono::Utc);
                            if elapsed.num_seconds() as u64 > wt.seconds {
                                warn!(
                                    job = name,
                                    walltime = %wt,
                                    elapsed_secs = elapsed.num_seconds(),
                                    "walltime exceeded, terminating"
                                );
                                ctx.backend.terminate(name, namespace).await?;
                                update_status(
                                    name,
                                    namespace,
                                    JobState::WalltimeExceeded,
                                    Some(&format!("walltime of {} exceeded", wt)),
                                    ctx,
                                )
                                .await?;
                                ctx.metrics.record_job_state_change(&spec.queue, "Running", "WalltimeExceeded");
                            }
                        }
                    }
                }
            }
        }
        BackendJobStatus::Launching { ready, total } => {
            // Update ready worker count
            let api: Api<BuboJob> = Api::namespaced(ctx.client.clone(), namespace);
            let status = serde_json::json!({
                "status": {
                    "readyWorkers": ready,
                    "totalWorkers": total,
                }
            });
            api.patch_status(
                name,
                &PatchParams::apply("bubo-controller"),
                &Patch::Merge(&status),
            )
            .await
            .map_err(BuboError::KubeError)?;
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

/// How long completed pods are preserved for log retrieval before deletion.
const POD_TTL_AFTER_FINISHED: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60); // 24 hours

async fn handle_terminal(
    name: &str,
    namespace: &str,
    status: &BuboJobStatus,
    ctx: &ReconcilerContext,
) -> Result<(), BuboError> {
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
    // Use completion_time if available, otherwise start_time.
    let finished_at = status
        .completion_time
        .as_deref()
        .or(status.start_time.as_deref());

    if let Some(time_str) = finished_at {
        if let Ok(finished) = chrono::DateTime::parse_from_rfc3339(time_str) {
            let elapsed = chrono::Utc::now() - finished.with_timezone(&chrono::Utc);
            if elapsed.num_seconds() as u64 > POD_TTL_AFTER_FINISHED.as_secs() {
                info!(job = name, "pod TTL expired, cleaning up pods");
                if let Err(e) = ctx.backend.terminate(name, namespace).await {
                    warn!(job = name, error = %e, "failed to delete expired pods");
                }
            }
        }
    }

    Ok(())
}

/// Helper to patch the BuboJob status subresource.
async fn update_status(
    name: &str,
    namespace: &str,
    state: JobState,
    message: Option<&str>,
    ctx: &ReconcilerContext,
) -> Result<(), BuboError> {
    let api: Api<BuboJob> = Api::namespaced(ctx.client.clone(), namespace);
    let status = serde_json::json!({
        "status": {
            "state": state,
            "message": message,
        }
    });
    api.patch_status(
        name,
        &PatchParams::apply("bubo-controller"),
        &Patch::Merge(&status),
    )
    .await
    .map_err(BuboError::KubeError)?;
    Ok(())
}
