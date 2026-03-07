use futures::StreamExt;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Api, Client};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use wren_core::{ClusterState, WrenError, WrenJob};

mod container;
mod job_id;
mod leader_election;
#[allow(dead_code)]
mod metrics;
#[allow(dead_code)]
mod mpi;
mod node_watcher;
#[allow(dead_code)]
mod reaper;
mod reconciler;
mod user_identity;
#[allow(dead_code)]
mod reservation;
#[allow(dead_code)]
mod webhook;

use container::ContainerBackend;
use job_id::JobIdAllocator;
use metrics::Metrics;
use node_watcher::NodeWatcher;
use reconciler::ReconcilerContext;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    info!("starting wren controller");

    let client = Client::try_default().await?;
    let metrics = Metrics::new();

    // --- Leader election ---
    let leader_election_enabled = std::env::var("WREN_LEADER_ELECTION")
        .unwrap_or_else(|_| "true".to_string())
        .trim()
        .to_lowercase()
        != "false";

    if leader_election_enabled {
        let le_config = leader_election::LeaderElectionConfig::default();
        info!(
            identity = %le_config.identity,
            lease = %le_config.lease_name,
            namespace = %le_config.lease_namespace,
            "leader election enabled — waiting to acquire lease"
        );

        let mut is_leader = leader_election::run_leader_election(client.clone(), le_config).await?;

        // Block until we become the leader.
        while !*is_leader.borrow() {
            if is_leader.changed().await.is_err() {
                anyhow::bail!("leader election channel closed unexpectedly");
            }
        }
        info!("this instance is now the leader — starting controller");

        // Spawn a task that shuts down if leadership is lost.
        tokio::spawn(async move {
            loop {
                if is_leader.changed().await.is_err() {
                    warn!("leader election channel closed — shutting down");
                    std::process::exit(1);
                }
                if !*is_leader.borrow() {
                    warn!("leadership lost — shutting down gracefully");
                    std::process::exit(1);
                }
            }
        });
    } else {
        info!("leader election disabled (WREN_LEADER_ELECTION=false)");
    }

    // Shared cluster state
    let cluster_state = Arc::new(RwLock::new(ClusterState::new()));

    // Initial node sync
    let node_watcher = NodeWatcher::new(client.clone(), cluster_state.clone());
    if let Err(e) = node_watcher.sync_nodes().await {
        error!(error = %e, "failed initial node sync");
    }

    // Set up the execution backend
    let backend = Arc::new(ContainerBackend::new(client.clone()));

    // Job ID allocator — counter persisted in a ConfigMap
    let controller_namespace = std::env::var("WREN_NAMESPACE").unwrap_or_else(|_| "wren-system".to_string());
    let job_id_allocator = JobIdAllocator::new(client.clone(), &controller_namespace);

    // Build reconciler context
    let ctx = Arc::new(ReconcilerContext {
        client: client.clone(),
        cluster_state: cluster_state.clone(),
        backend,
        metrics: metrics.clone(),
        reservations: RwLock::new(reservation::ReservationManager::default()),
        job_id_allocator,
    });

    // Start metrics server in background
    let metrics_handle = tokio::spawn(metrics::serve_metrics(metrics, 8080));

    // Start periodic node sync in background
    let node_sync_client = client.clone();
    let node_sync_state = cluster_state.clone();
    let _node_sync_handle = tokio::spawn(async move {
        let watcher = NodeWatcher::new(node_sync_client, node_sync_state);
        loop {
            if let Err(e) = watcher.sync_nodes().await {
                error!(error = %e, "periodic node sync failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // Start the WrenJob controller
    let wrenjob_api: Api<WrenJob> = Api::all(client.clone());
    let pod_api: Api<k8s_openapi::api::core::v1::Pod> = Api::all(client.clone());

    info!("starting WrenJob controller");
    Controller::new(wrenjob_api, Config::default())
        // Watch pods with wren labels — triggers re-reconciliation of the owning
        // WrenJob when a pod phase changes (e.g. worker completes).
        .watches(
            pod_api,
            Config::default().labels("app.kubernetes.io/managed-by=wren"),
            |pod| {
                pod.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("wren.giar.dev/job-name"))
                    .map(|name| {
                        kube::runtime::reflector::ObjectRef::<WrenJob>::new(name)
                            .within(pod.metadata.namespace.as_deref().unwrap_or("default"))
                    })
            },
        )
        .shutdown_on_signal()
        .run(
            |job: Arc<WrenJob>, ctx: Arc<ReconcilerContext>| async move {
                reconciler::reconcile(&job, &ctx).await?;
                Ok(Action::requeue(std::time::Duration::from_secs(30)))
            },
            |_job: Arc<WrenJob>, error: &WrenError, _ctx: Arc<ReconcilerContext>| {
                error!(error = %error, "controller error policy triggered");
                Action::requeue(std::time::Duration::from_secs(5))
            },
            ctx,
        )
        .for_each(|result| async {
            match result {
                Ok((obj, _action)) => {
                    info!(
                        job = obj.name,
                        namespace = obj.namespace.as_deref().unwrap_or(""),
                        "reconciled"
                    );
                }
                Err(e) => {
                    error!(error = %e, "reconciliation failed");
                }
            }
        })
        .await;

    metrics_handle.abort();
    info!("wren controller shutting down");
    Ok(())
}
