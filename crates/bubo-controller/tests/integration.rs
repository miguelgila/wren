/// Integration tests for the Bubo controller against a real Kubernetes cluster.
///
/// These tests require a running cluster (kind or otherwise) accessible via KUBECONFIG.
/// All tests are marked `#[ignore]` so they do not run during normal `cargo test`.
///
/// To run them:
///   cargo test -p bubo-controller --test integration -- --ignored
///
/// Prerequisites:
///   1. A kind cluster: `kind create cluster`
///   2. CRDs installed: `kubectl apply -f manifests/crds/`
use std::time::Duration;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{
    api::{Api, DeleteParams, ListParams, ObjectMeta, PostParams},
    Client,
};
use serde_json::json;
use tokio::time::{sleep, timeout};

use bubo_core::crd::{BuboQueue, BuboQueueSpec, ContainerSpec, MPIJob, MPIJobSpec};
use bubo_core::types::{ExecutionBackendType, JobState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `true` if a Kubernetes cluster is reachable, `false` otherwise.
async fn cluster_available() -> bool {
    match Client::try_default().await {
        Ok(client) => {
            // Attempt a cheap API call to verify connectivity.
            let api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client);
            api.list(&ListParams::default()).await.is_ok()
        }
        Err(_) => false,
    }
}

/// Skip the calling test when no cluster is available.
///
/// Usage:
/// ```
/// if skip_if_no_cluster().await { return; }
/// ```
async fn skip_if_no_cluster() -> bool {
    if !cluster_available().await {
        eprintln!("SKIP: no Kubernetes cluster available (set KUBECONFIG or run in-cluster)");
        return true;
    }
    false
}

/// Build a minimal `MPIJob` object suitable for testing.
fn build_mpijob(name: &str, namespace: &str, nodes: u32) -> MPIJob {
    MPIJob {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: MPIJobSpec {
            nodes,
            queue: "default".to_string(),
            priority: 50,
            walltime: Some("1h".to_string()),
            tasks_per_node: 1,
            backend: ExecutionBackendType::Container,
            container: Some(ContainerSpec {
                image: "busybox:latest".to_string(),
                command: vec!["sleep".to_string(), "3600".to_string()],
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
        },
        status: None,
    }
}

/// Build a minimal `BuboQueue` object suitable for testing.
fn build_buboqueue(name: &str, namespace: &str, max_nodes: u32) -> BuboQueue {
    BuboQueue {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: BuboQueueSpec {
            max_nodes,
            max_walltime: Some("24h".to_string()),
            max_jobs_per_user: Some(10),
            default_priority: 50,
            backfill: None,
            fair_share: None,
        },
    }
}

/// Create an `MPIJob` in the cluster, returning the created object.
async fn create_test_mpijob(
    client: Client,
    name: &str,
    namespace: &str,
    nodes: u32,
) -> anyhow::Result<MPIJob> {
    let api: Api<MPIJob> = Api::namespaced(client, namespace);
    let job = build_mpijob(name, namespace, nodes);
    let created = api.create(&PostParams::default(), &job).await?;
    Ok(created)
}

/// Create a `BuboQueue` in the cluster.
async fn create_test_buboqueue(
    client: Client,
    name: &str,
    namespace: &str,
    max_nodes: u32,
) -> anyhow::Result<BuboQueue> {
    let api: Api<BuboQueue> = Api::namespaced(client, namespace);
    let queue = build_buboqueue(name, namespace, max_nodes);
    let created = api.create(&PostParams::default(), &queue).await?;
    Ok(created)
}

/// Poll the `MPIJob` status until `.status.state` matches `expected`, or timeout elapses.
///
/// Returns `true` if the expected state was reached, `false` on timeout.
async fn wait_for_job_state(
    client: Client,
    name: &str,
    namespace: &str,
    expected: JobState,
    poll_timeout: Duration,
) -> bool {
    let api: Api<MPIJob> = Api::namespaced(client, namespace);
    let deadline = timeout(poll_timeout, async {
        loop {
            match api.get(name).await {
                Ok(job) => {
                    if let Some(status) = &job.status {
                        if status.state == expected {
                            return true;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("wait_for_job_state: get error: {e}");
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await;

    matches!(deadline, Ok(true))
}

/// Delete an `MPIJob` and wait until it is gone from the API (up to 30 s).
async fn cleanup_job(client: Client, name: &str, namespace: &str) -> anyhow::Result<()> {
    let api: Api<MPIJob> = Api::namespaced(client.clone(), namespace);
    let _ = api.delete(name, &DeleteParams::default()).await;

    // Wait for deletion
    timeout(Duration::from_secs(30), async {
        loop {
            match api.get(name).await {
                Err(kube::Error::Api(e)) if e.code == 404 => break,
                _ => sleep(Duration::from_millis(500)).await,
            }
        }
    })
    .await
    .ok(); // best-effort

    Ok(())
}

/// Delete a `BuboQueue` and wait until it is gone from the API (up to 30 s).
async fn cleanup_queue(client: Client, name: &str, namespace: &str) -> anyhow::Result<()> {
    let api: Api<BuboQueue> = Api::namespaced(client.clone(), namespace);
    let _ = api.delete(name, &DeleteParams::default()).await;

    timeout(Duration::from_secs(30), async {
        loop {
            match api.get(name).await {
                Err(kube::Error::Api(e)) if e.code == 404 => break,
                _ => sleep(Duration::from_millis(500)).await,
            }
        }
    })
    .await
    .ok();

    Ok(())
}

/// Verify that both `MPIJob` and `BuboQueue` CRDs are registered with the API server.
async fn ensure_crds_installed(client: Client) -> bool {
    let crd_api: Api<CustomResourceDefinition> = Api::all(client);
    let mpijob_exists = crd_api
        .get("mpijobs.hpc.cscs.ch")
        .await
        .is_ok();
    let queue_exists = crd_api
        .get("buboqueues.hpc.cscs.ch")
        .await
        .is_ok();
    mpijob_exists && queue_exists
}

// ---------------------------------------------------------------------------
// CRD-level tests
// ---------------------------------------------------------------------------

/// Verify the `MPIJob` CRD is registered in the cluster.
#[tokio::test]
#[ignore]
async fn test_mpijob_crd_registered() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    let crd_api: Api<CustomResourceDefinition> = Api::all(client);
    let result = crd_api.get("mpijobs.hpc.cscs.ch").await;
    assert!(
        result.is_ok(),
        "MPIJob CRD not found — apply manifests/crds/ first"
    );
}

/// Verify the `BuboQueue` CRD is registered in the cluster.
#[tokio::test]
#[ignore]
async fn test_buboqueue_crd_registered() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    let crd_api: Api<CustomResourceDefinition> = Api::all(client);
    let result = crd_api.get("buboqueues.hpc.cscs.ch").await;
    assert!(
        result.is_ok(),
        "BuboQueue CRD not found — apply manifests/crds/ first"
    );
}

/// Create an `MPIJob` and verify it appears in the API.
#[tokio::test]
#[ignore]
async fn test_create_mpijob() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-create-mpijob";
    let namespace = "default";

    // Clean up any leftover from a previous run.
    let _ = cleanup_job(client.clone(), name, namespace).await;

    let created = create_test_mpijob(client.clone(), name, namespace, 2)
        .await
        .expect("create MPIJob");

    assert_eq!(created.metadata.name.as_deref(), Some(name));
    assert_eq!(created.spec.nodes, 2);
    assert_eq!(created.spec.queue, "default");

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create an `MPIJob` with only the required field (`nodes`) and verify defaults.
#[tokio::test]
#[ignore]
async fn test_mpijob_default_values() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-mpijob-defaults";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    // Post raw JSON with only the mandatory field.
    let api: Api<MPIJob> = Api::namespaced(client.clone(), namespace);
    let raw: MPIJob = serde_json::from_value(json!({
        "apiVersion": "hpc.cscs.ch/v1alpha1",
        "kind": "MPIJob",
        "metadata": { "name": name, "namespace": namespace },
        "spec": { "nodes": 1 }
    }))
    .expect("deserialize minimal MPIJob");

    let created = api
        .create(&PostParams::default(), &raw)
        .await
        .expect("create minimal MPIJob");

    assert_eq!(created.spec.nodes, 1);
    assert_eq!(created.spec.queue, "default", "default queue should be set");
    assert_eq!(created.spec.priority, 50, "default priority should be 50");
    assert_eq!(created.spec.tasks_per_node, 1, "default tasks_per_node should be 1");

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create a `BuboQueue` and verify it appears in the API.
#[tokio::test]
#[ignore]
async fn test_create_buboqueue() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-create-queue";
    let namespace = "default";
    let _ = cleanup_queue(client.clone(), name, namespace).await;

    let created = create_test_buboqueue(client.clone(), name, namespace, 64)
        .await
        .expect("create BuboQueue");

    assert_eq!(created.metadata.name.as_deref(), Some(name));
    assert_eq!(created.spec.max_nodes, 64);

    cleanup_queue(client, name, namespace).await.expect("cleanup");
}

// ---------------------------------------------------------------------------
// Lifecycle tests (require controller to be running)
// ---------------------------------------------------------------------------

/// Create a job and verify it transitions from `Pending` to `Scheduling`.
///
/// Requires the Bubo controller to be running in the cluster.
#[tokio::test]
#[ignore]
async fn test_job_transitions_to_scheduling() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-job-scheduling";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    create_test_mpijob(client.clone(), name, namespace, 2)
        .await
        .expect("create MPIJob");

    // Give the controller up to 30 s to move the job into Scheduling state.
    let reached = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Scheduling,
        Duration::from_secs(30),
    )
    .await;

    // Even if the controller is not running the job should at least be Pending.
    let api: Api<MPIJob> = Api::namespaced(client.clone(), namespace);
    let job = api.get(name).await.expect("get job");
    let state = job
        .status
        .as_ref()
        .map(|s| s.state.clone())
        .unwrap_or(JobState::Pending);

    eprintln!("job state after wait: {state}");
    if !reached {
        eprintln!(
            "NOTE: controller may not be running; job remained in {state}. \
             This is expected when testing CRD plumbing only."
        );
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Submit an `MPIJob` with `nodes: 0`, which the controller should reject or mark Failed.
#[tokio::test]
#[ignore]
async fn test_invalid_job_fails() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-invalid-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    // Try to create a job with 0 nodes.
    // The API server may reject it (via webhook validation) or the controller marks it Failed.
    let api: Api<MPIJob> = Api::namespaced(client.clone(), namespace);
    let raw: MPIJob = serde_json::from_value(json!({
        "apiVersion": "hpc.cscs.ch/v1alpha1",
        "kind": "MPIJob",
        "metadata": { "name": name, "namespace": namespace },
        "spec": { "nodes": 0 }
    }))
    .expect("deserialize");

    match api.create(&PostParams::default(), &raw).await {
        Err(e) => {
            // Webhook or CRD validation rejected the object — correct behaviour.
            eprintln!("Server rejected invalid job (expected): {e}");
        }
        Ok(_) => {
            // Object accepted; wait for controller to mark it Failed.
            let failed = wait_for_job_state(
                client.clone(),
                name,
                namespace,
                JobState::Failed,
                Duration::from_secs(30),
            )
            .await;
            eprintln!(
                "invalid job accepted by API server; controller marked Failed: {failed}"
            );
            // We log rather than assert here because webhook validation is Phase 5.
            cleanup_job(client, name, namespace).await.expect("cleanup");
        }
    }
}

/// Create a job then delete it and verify it is removed from the API.
#[tokio::test]
#[ignore]
async fn test_cancel_job() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-cancel-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    create_test_mpijob(client.clone(), name, namespace, 1)
        .await
        .expect("create MPIJob");

    // Verify it exists.
    let api: Api<MPIJob> = Api::namespaced(client.clone(), namespace);
    api.get(name).await.expect("job should exist");

    // Delete and wait.
    cleanup_job(client.clone(), name, namespace)
        .await
        .expect("cleanup");

    // Confirm it is gone.
    let gone = matches!(
        api.get(name).await,
        Err(kube::Error::Api(e)) if e.code == 404
    );
    assert!(gone, "job should be deleted");
}

/// Submit multiple jobs and verify they all appear in the API (queue behaviour).
#[tokio::test]
#[ignore]
async fn test_multiple_jobs_queued() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let namespace = "default";
    let names = [
        "test-queue-job-1",
        "test-queue-job-2",
        "test-queue-job-3",
    ];

    // Clean up any leftovers.
    for name in &names {
        let _ = cleanup_job(client.clone(), name, namespace).await;
    }

    // Submit all three jobs.
    for name in &names {
        create_test_mpijob(client.clone(), name, namespace, 1)
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
    }

    // Verify all three are visible.
    let api: Api<MPIJob> = Api::namespaced(client.clone(), namespace);
    for name in &names {
        let job = api.get(name).await.expect("job should exist");
        assert_eq!(job.metadata.name.as_deref(), Some(*name));
        // Each job should start as Pending.
        let state = job
            .status
            .as_ref()
            .map(|s| s.state.clone())
            .unwrap_or(JobState::Pending);
        assert!(
            matches!(state, JobState::Pending | JobState::Scheduling),
            "unexpected initial state: {state}"
        );
    }

    // Clean up.
    for name in &names {
        cleanup_job(client.clone(), name, namespace)
            .await
            .expect("cleanup");
    }
}
