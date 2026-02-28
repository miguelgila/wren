use bubo_core::{BuboError, ClusterState, MPIJob};
use futures::StreamExt;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Api, Client};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

mod container;
mod metrics;
mod mpi;
mod node_watcher;
mod reaper;
mod reconciler;
mod reservation;

use container::ContainerBackend;
use metrics::Metrics;
use node_watcher::NodeWatcher;
use reconciler::ReconcilerContext;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    info!("starting bubo controller");

    let client = Client::try_default().await?;
    let metrics = Metrics::new();

    // Shared cluster state
    let cluster_state = Arc::new(RwLock::new(ClusterState::new()));

    // Initial node sync
    let node_watcher = NodeWatcher::new(client.clone(), cluster_state.clone());
    if let Err(e) = node_watcher.sync_nodes().await {
        error!(error = %e, "failed initial node sync");
    }

    // Set up the execution backend
    let backend = Arc::new(ContainerBackend::new(client.clone()));

    // Build reconciler context
    let ctx = Arc::new(ReconcilerContext {
        client: client.clone(),
        cluster_state: cluster_state.clone(),
        backend,
        metrics: metrics.clone(),
        reservations: RwLock::new(reservation::ReservationManager::default()),
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

    // Start the MPIJob controller
    let mpijob_api: Api<MPIJob> = Api::all(client.clone());

    info!("starting MPIJob controller");
    Controller::new(mpijob_api, Config::default())
        .shutdown_on_signal()
        .run(
            |job: Arc<MPIJob>, ctx: Arc<ReconcilerContext>| async move {
                reconciler::reconcile(&job, &ctx).await?;
                Ok(Action::requeue(std::time::Duration::from_secs(30)))
            },
            |_job: Arc<MPIJob>, error: &BuboError, _ctx: Arc<ReconcilerContext>| {
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
    info!("bubo controller shutting down");
    Ok(())
}
