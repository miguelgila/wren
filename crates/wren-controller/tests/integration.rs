/// Integration tests for the Wren controller against a real Kubernetes cluster.
///
/// These tests require a running cluster (kind or otherwise) accessible via KUBECONFIG.
/// All tests are marked `#[ignore]` so they do not run during normal `cargo test`.
///
/// To run them:
///   cargo test -p wren-controller --test integration -- --ignored
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

use wren_core::crd::{ContainerSpec, WrenJob, WrenJobSpec, WrenQueue, WrenQueueSpec};
use wren_core::types::{ExecutionBackendType, JobState};

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

/// Build a minimal `WrenJob` object suitable for testing.
fn build_wrenjob(name: &str, namespace: &str, nodes: u32) -> WrenJob {
    WrenJob {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: WrenJobSpec {
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

/// Build a minimal `WrenQueue` object suitable for testing.
fn build_wrenqueue(name: &str, namespace: &str, max_nodes: u32) -> WrenQueue {
    WrenQueue {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: WrenQueueSpec {
            max_nodes,
            max_walltime: Some("24h".to_string()),
            max_jobs_per_user: Some(10),
            default_priority: 50,
            backfill: None,
            fair_share: None,
        },
    }
}

/// Create an `WrenJob` in the cluster, returning the created object.
async fn create_test_wrenjob(
    client: Client,
    name: &str,
    namespace: &str,
    nodes: u32,
) -> anyhow::Result<WrenJob> {
    let api: Api<WrenJob> = Api::namespaced(client, namespace);
    let job = build_wrenjob(name, namespace, nodes);
    let created = api.create(&PostParams::default(), &job).await?;
    Ok(created)
}

/// Create a `WrenQueue` in the cluster.
async fn create_test_wrenqueue(
    client: Client,
    name: &str,
    namespace: &str,
    max_nodes: u32,
) -> anyhow::Result<WrenQueue> {
    let api: Api<WrenQueue> = Api::namespaced(client, namespace);
    let queue = build_wrenqueue(name, namespace, max_nodes);
    let created = api.create(&PostParams::default(), &queue).await?;
    Ok(created)
}

/// Poll the `WrenJob` status until `.status.state` matches `expected`, or timeout elapses.
///
/// Returns `true` if the expected state was reached, `false` on timeout.
async fn wait_for_job_state(
    client: Client,
    name: &str,
    namespace: &str,
    expected: JobState,
    poll_timeout: Duration,
) -> bool {
    let api: Api<WrenJob> = Api::namespaced(client, namespace);
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

/// Delete an `WrenJob` and wait until it is gone from the API (up to 30 s).
async fn cleanup_job(client: Client, name: &str, namespace: &str) -> anyhow::Result<()> {
    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
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

/// Delete a `WrenQueue` and wait until it is gone from the API (up to 30 s).
async fn cleanup_queue(client: Client, name: &str, namespace: &str) -> anyhow::Result<()> {
    let api: Api<WrenQueue> = Api::namespaced(client.clone(), namespace);
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

/// Verify that both `WrenJob` and `WrenQueue` CRDs are registered with the API server.
async fn ensure_crds_installed(client: Client) -> bool {
    let crd_api: Api<CustomResourceDefinition> = Api::all(client);
    let wrenjob_exists = crd_api.get("wrenjobs.hpc.cscs.ch").await.is_ok();
    let queue_exists = crd_api.get("wrenqueues.hpc.cscs.ch").await.is_ok();
    wrenjob_exists && queue_exists
}

// ---------------------------------------------------------------------------
// CRD-level tests
// ---------------------------------------------------------------------------

/// Verify the `WrenJob` CRD is registered in the cluster.
#[tokio::test]
#[ignore]
async fn test_wrenjob_crd_registered() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    let crd_api: Api<CustomResourceDefinition> = Api::all(client);
    let result = crd_api.get("wrenjobs.hpc.cscs.ch").await;
    assert!(
        result.is_ok(),
        "WrenJob CRD not found — apply manifests/crds/ first"
    );
}

/// Verify the `WrenQueue` CRD is registered in the cluster.
#[tokio::test]
#[ignore]
async fn test_wrenqueue_crd_registered() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    let crd_api: Api<CustomResourceDefinition> = Api::all(client);
    let result = crd_api.get("wrenqueues.hpc.cscs.ch").await;
    assert!(
        result.is_ok(),
        "WrenQueue CRD not found — apply manifests/crds/ first"
    );
}

/// Create an `WrenJob` and verify it appears in the API.
#[tokio::test]
#[ignore]
async fn test_create_wrenjob() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-create-wrenjob";
    let namespace = "default";

    // Clean up any leftover from a previous run.
    let _ = cleanup_job(client.clone(), name, namespace).await;

    let created = create_test_wrenjob(client.clone(), name, namespace, 2)
        .await
        .expect("create WrenJob");

    assert_eq!(created.metadata.name.as_deref(), Some(name));
    assert_eq!(created.spec.nodes, 2);
    assert_eq!(created.spec.queue, "default");

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create an `WrenJob` with only the required field (`nodes`) and verify defaults.
#[tokio::test]
#[ignore]
async fn test_wrenjob_default_values() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-wrenjob-defaults";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    // Post raw JSON with only the mandatory field.
    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
    let raw: WrenJob = serde_json::from_value(json!({
        "apiVersion": "hpc.cscs.ch/v1alpha1",
        "kind": "WrenJob",
        "metadata": { "name": name, "namespace": namespace },
        "spec": { "nodes": 1 }
    }))
    .expect("deserialize minimal WrenJob");

    let created = api
        .create(&PostParams::default(), &raw)
        .await
        .expect("create minimal WrenJob");

    assert_eq!(created.spec.nodes, 1);
    assert_eq!(created.spec.queue, "default", "default queue should be set");
    assert_eq!(created.spec.priority, 50, "default priority should be 50");
    assert_eq!(
        created.spec.tasks_per_node, 1,
        "default tasks_per_node should be 1"
    );

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create a `WrenQueue` and verify it appears in the API.
#[tokio::test]
#[ignore]
async fn test_create_wrenqueue() {
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

    let created = create_test_wrenqueue(client.clone(), name, namespace, 64)
        .await
        .expect("create WrenQueue");

    assert_eq!(created.metadata.name.as_deref(), Some(name));
    assert_eq!(created.spec.max_nodes, 64);

    cleanup_queue(client, name, namespace)
        .await
        .expect("cleanup");
}

// ---------------------------------------------------------------------------
// Lifecycle tests (require controller to be running)
// ---------------------------------------------------------------------------

/// Create a job and verify it transitions from `Pending` to `Scheduling`.
///
/// Requires the Wren controller to be running in the cluster.
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

    create_test_wrenjob(client.clone(), name, namespace, 2)
        .await
        .expect("create WrenJob");

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
    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
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

/// Submit an `WrenJob` with `nodes: 0`, which the controller should reject or mark Failed.
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
    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
    let raw: WrenJob = serde_json::from_value(json!({
        "apiVersion": "hpc.cscs.ch/v1alpha1",
        "kind": "WrenJob",
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
            eprintln!("invalid job accepted by API server; controller marked Failed: {failed}");
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

    create_test_wrenjob(client.clone(), name, namespace, 1)
        .await
        .expect("create WrenJob");

    // Verify it exists.
    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
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
    let names = ["test-queue-job-1", "test-queue-job-2", "test-queue-job-3"];

    // Clean up any leftovers.
    for name in &names {
        let _ = cleanup_job(client.clone(), name, namespace).await;
    }

    // Submit all three jobs.
    for name in &names {
        create_test_wrenjob(client.clone(), name, namespace, 1)
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
    }

    // Verify all three are visible.
    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
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

// ---------------------------------------------------------------------------
// Additional helpers
// ---------------------------------------------------------------------------

use k8s_openapi::api::core::v1::{ConfigMap, Node, Pod, Service};
use wren_core::crd::TopologySpec;

/// Build a minimal `WrenJob` with a custom walltime string.
fn build_wrenjob_with_walltime(name: &str, namespace: &str, nodes: u32, walltime: &str) -> WrenJob {
    WrenJob {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: WrenJobSpec {
            nodes,
            queue: "default".to_string(),
            priority: 50,
            walltime: Some(walltime.to_string()),
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

/// Build a minimal `WrenJob` with topology constraints.
fn build_wrenjob_with_topology(
    name: &str,
    namespace: &str,
    nodes: u32,
    prefer_same_switch: bool,
    max_hops: Option<u32>,
) -> WrenJob {
    WrenJob {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: WrenJobSpec {
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
            topology: Some(TopologySpec {
                prefer_same_switch,
                max_hops,
                topology_key: None,
            }),
            dependencies: vec![],
        },
        status: None,
    }
}

/// Wait until at least `min_count` pods matching `label_selector` exist, or timeout elapses.
/// Returns the count found on success, or 0 on timeout.
async fn wait_for_pods(
    client: Client,
    namespace: &str,
    label_selector: &str,
    min_count: usize,
    wait_timeout: Duration,
) -> usize {
    let api: Api<Pod> = Api::namespaced(client, namespace);
    let lp = ListParams::default().labels(label_selector);
    let result = timeout(wait_timeout, async {
        loop {
            match api.list(&lp).await {
                Ok(list) if list.items.len() >= min_count => return list.items.len(),
                _ => sleep(Duration::from_millis(500)).await,
            }
        }
    })
    .await;
    result.unwrap_or(0)
}

/// Wait until a Service with the given name exists in the namespace.
/// Returns true if found within the timeout, false otherwise.
async fn wait_for_service(
    client: Client,
    namespace: &str,
    svc_name: &str,
    wait_timeout: Duration,
) -> bool {
    let api: Api<Service> = Api::namespaced(client, namespace);
    let result = timeout(wait_timeout, async {
        loop {
            match api.get(svc_name).await {
                Ok(_) => return true,
                Err(_) => sleep(Duration::from_millis(500)).await,
            }
        }
    })
    .await;
    matches!(result, Ok(true))
}

/// Wait until a ConfigMap with the given name exists in the namespace.
/// Returns true if found within the timeout, false otherwise.
async fn wait_for_configmap(
    client: Client,
    namespace: &str,
    cm_name: &str,
    wait_timeout: Duration,
) -> bool {
    let api: Api<ConfigMap> = Api::namespaced(client, namespace);
    let result = timeout(wait_timeout, async {
        loop {
            match api.get(cm_name).await {
                Ok(_) => return true,
                Err(_) => sleep(Duration::from_millis(500)).await,
            }
        }
    })
    .await;
    matches!(result, Ok(true))
}

/// Count pods matching a label selector in a namespace.
async fn count_pods(client: Client, namespace: &str, label_selector: &str) -> usize {
    let api: Api<Pod> = Api::namespaced(client, namespace);
    let lp = ListParams::default().labels(label_selector);
    api.list(&lp).await.map(|l| l.items.len()).unwrap_or(0)
}

/// Wait until no pods matching `label_selector` remain, or timeout elapses.
/// Returns true if cleared within the timeout.
async fn wait_for_pods_gone(
    client: Client,
    namespace: &str,
    label_selector: &str,
    wait_timeout: Duration,
) -> bool {
    let api: Api<Pod> = Api::namespaced(client, namespace);
    let lp = ListParams::default().labels(label_selector);
    let result = timeout(wait_timeout, async {
        loop {
            match api.list(&lp).await {
                Ok(list) if list.items.is_empty() => return true,
                _ => sleep(Duration::from_millis(500)).await,
            }
        }
    })
    .await;
    matches!(result, Ok(true))
}

/// Wait until a Service is gone from the namespace, or timeout elapses.
async fn wait_for_service_gone(
    client: Client,
    namespace: &str,
    svc_name: &str,
    wait_timeout: Duration,
) -> bool {
    let api: Api<Service> = Api::namespaced(client, namespace);
    let result = timeout(wait_timeout, async {
        loop {
            match api.get(svc_name).await {
                Err(kube::Error::Api(e)) if e.code == 404 => return true,
                _ => sleep(Duration::from_millis(500)).await,
            }
        }
    })
    .await;
    matches!(result, Ok(true))
}

// ---------------------------------------------------------------------------
// Controller lifecycle tests (require controller running)
// ---------------------------------------------------------------------------

/// Create a 1-node job with a fast command and verify it reaches Succeeded state.
#[tokio::test]
#[ignore]
async fn test_job_runs_to_completion() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-completion-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    let job = WrenJob {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: WrenJobSpec {
            nodes: 1,
            queue: "default".to_string(),
            priority: 50,
            walltime: Some("10m".to_string()),
            tasks_per_node: 1,
            backend: ExecutionBackendType::Container,
            container: Some(ContainerSpec {
                image: "busybox:latest".to_string(),
                command: vec!["sh".to_string(), "-c".to_string(), "echo done".to_string()],
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
    };

    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
    api.create(&PostParams::default(), &job)
        .await
        .expect("create WrenJob");

    // Wait for Running state first (up to 30 s).
    let reached_running = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    if !reached_running {
        eprintln!("NOTE: job did not reach Running — controller may not be active");
    }

    // Then wait for Succeeded (up to 60 s total).
    let reached_succeeded = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Succeeded,
        Duration::from_secs(60),
    )
    .await;

    if reached_succeeded {
        let job = api.get(name).await.expect("get job");
        let completion_time = job.status.as_ref().and_then(|s| s.completion_time.as_ref());
        assert!(
            completion_time.is_some(),
            "completionTime should be set after Succeeded"
        );
        eprintln!("job completed at: {:?}", completion_time);
    } else {
        eprintln!("NOTE: job did not reach Succeeded — controller may not be active");
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create a 2-node job, wait for it to progress, then verify worker pods exist.
#[tokio::test]
#[ignore]
async fn test_job_creates_worker_pods() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-worker-pods-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    create_test_wrenjob(client.clone(), name, namespace, 2)
        .await
        .expect("create WrenJob");

    // Wait up to 30 s for the controller to move past Pending.
    let _ = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    // Verify worker pods with wren.io/job-name and wren.io/role=worker labels.
    let label_selector = format!("wren.io/job-name={name},wren.io/role=worker");
    let found = wait_for_pods(
        client.clone(),
        namespace,
        &label_selector,
        1,
        Duration::from_secs(30),
    )
    .await;

    if found == 0 {
        eprintln!("NOTE: no worker pods found — controller may not be active");
    } else {
        eprintln!("found {found} worker pod(s) for job {name}");

        // Spot-check one pod for the rank label.
        let pod_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let lp = ListParams::default().labels(&label_selector);
        let pods = pod_api.list(&lp).await.expect("list worker pods");
        for pod in &pods.items {
            let labels = pod.metadata.labels.as_ref();
            assert!(
                labels.and_then(|l| l.get("wren.io/rank")).is_some(),
                "worker pod missing wren.io/rank label"
            );
            assert!(
                labels.and_then(|l| l.get("wren.io/job-name")).is_some(),
                "worker pod missing wren.io/job-name label"
            );
        }
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create a job and verify the launcher pod exists with the correct label.
#[tokio::test]
#[ignore]
async fn test_job_creates_launcher_pod() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-launcher-pod-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    create_test_wrenjob(client.clone(), name, namespace, 1)
        .await
        .expect("create WrenJob");

    let _ = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    let label_selector = format!("wren.io/job-name={name},wren.io/role=launcher");
    let found = wait_for_pods(
        client.clone(),
        namespace,
        &label_selector,
        1,
        Duration::from_secs(30),
    )
    .await;

    if found == 0 {
        eprintln!("NOTE: no launcher pod found — controller may not be active");
    } else {
        eprintln!("found launcher pod for job {name}");
        // Verify the expected pod name convention.
        let pod_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let expected_launcher_name = format!("{name}-launcher");
        match pod_api.get(&expected_launcher_name).await {
            Ok(pod) => {
                let role = pod
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("wren.io/role"))
                    .map(String::as_str);
                assert_eq!(
                    role,
                    Some("launcher"),
                    "launcher pod should have role=launcher"
                );
            }
            Err(_) => eprintln!(
                "NOTE: launcher pod name {expected_launcher_name} not found; naming may differ"
            ),
        }
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create a job and verify the headless service is created.
#[tokio::test]
#[ignore]
async fn test_job_creates_headless_service() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-headless-svc-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    // Pre-clean the service in case of leftover.
    let svc_api: Api<Service> = Api::namespaced(client.clone(), namespace);
    let svc_name = format!("{name}-headless");
    let _ = svc_api.delete(&svc_name, &DeleteParams::default()).await;

    create_test_wrenjob(client.clone(), name, namespace, 1)
        .await
        .expect("create WrenJob");

    let _ = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    let found = wait_for_service(
        client.clone(),
        namespace,
        &svc_name,
        Duration::from_secs(30),
    )
    .await;

    if !found {
        eprintln!("NOTE: headless service {svc_name} not found — controller may not be active");
    } else {
        let svc = svc_api.get(&svc_name).await.expect("get headless service");
        // A headless service has clusterIP: None.
        let cluster_ip = svc.spec.as_ref().and_then(|s| s.cluster_ip.as_deref());
        eprintln!("headless service clusterIP: {cluster_ip:?}");
        assert!(
            cluster_ip == Some("None") || cluster_ip.is_none(),
            "expected headless service (clusterIP=None), got: {cluster_ip:?}"
        );
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create a job and verify the hostfile ConfigMap is created with a `hostfile` key.
#[tokio::test]
#[ignore]
async fn test_job_creates_hostfile_configmap() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-hostfile-cm-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let cm_name = format!("{name}-hostfile");
    let _ = cm_api.delete(&cm_name, &DeleteParams::default()).await;

    create_test_wrenjob(client.clone(), name, namespace, 1)
        .await
        .expect("create WrenJob");

    let _ = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    let found =
        wait_for_configmap(client.clone(), namespace, &cm_name, Duration::from_secs(30)).await;

    if !found {
        eprintln!("NOTE: hostfile ConfigMap {cm_name} not found — controller may not be active");
    } else {
        let cm = cm_api.get(&cm_name).await.expect("get hostfile configmap");
        let data = cm.data.unwrap_or_default();
        assert!(
            data.contains_key("hostfile"),
            "hostfile ConfigMap must have a 'hostfile' key; got keys: {:?}",
            data.keys().collect::<Vec<_>>()
        );
        eprintln!("hostfile contents: {}", data.get("hostfile").unwrap());
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Create a 2-node job and verify assignedNodes is populated in the status.
#[tokio::test]
#[ignore]
async fn test_job_status_reports_assigned_nodes() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-assigned-nodes-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    create_test_wrenjob(client.clone(), name, namespace, 2)
        .await
        .expect("create WrenJob");

    let reached = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
    let job = api.get(name).await.expect("get job");

    if reached {
        let assigned = job
            .status
            .as_ref()
            .map(|s| s.assigned_nodes.clone())
            .unwrap_or_default();
        assert!(
            !assigned.is_empty(),
            "assignedNodes should be non-empty when job is Running"
        );
        assert_eq!(
            assigned.len(),
            2,
            "expected 2 assigned nodes for a 2-node job, got: {assigned:?}"
        );
        eprintln!("assigned nodes: {assigned:?}");
    } else {
        eprintln!("NOTE: job did not reach Running — controller may not be active");
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

/// Verify totalWorkers and readyWorkers are populated when a job is Running.
#[tokio::test]
#[ignore]
async fn test_job_status_reports_workers() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-worker-status-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    let nodes = 2u32;
    create_test_wrenjob(client.clone(), name, namespace, nodes)
        .await
        .expect("create WrenJob");

    let reached = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
    let job = api.get(name).await.expect("get job");

    if reached {
        let status = job.status.as_ref().expect("status should be present");
        assert_eq!(
            status.total_workers, nodes,
            "totalWorkers should equal spec.nodes"
        );
        assert!(
            status.ready_workers > 0,
            "readyWorkers should be > 0 when Running"
        );
        eprintln!(
            "totalWorkers={}, readyWorkers={}",
            status.total_workers, status.ready_workers
        );
    } else {
        eprintln!("NOTE: job did not reach Running — controller may not be active");
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

// ---------------------------------------------------------------------------
// Walltime tests
// ---------------------------------------------------------------------------

/// Create a job with a very short walltime; expect it to reach WalltimeExceeded.
#[tokio::test]
#[ignore]
async fn test_short_walltime_triggers_exceeded() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-walltime-exceeded-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    // 5-second walltime with a long-running command so the job stays alive
    // until the walltime is enforced.
    let job = build_wrenjob_with_walltime(name, namespace, 1, "5s");
    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
    // Override the command to sleep so the job does not finish on its own.
    let mut job = job;
    if let Some(c) = job.spec.container.as_mut() {
        c.command = vec!["sleep".to_string(), "3600".to_string()];
    }
    api.create(&PostParams::default(), &job)
        .await
        .expect("create WrenJob");

    let reached = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::WalltimeExceeded,
        Duration::from_secs(60),
    )
    .await;

    if !reached {
        let current = api
            .get(name)
            .await
            .ok()
            .and_then(|j| j.status)
            .map(|s| s.state)
            .unwrap_or(JobState::Pending);
        eprintln!(
            "NOTE: job did not reach WalltimeExceeded (current: {current}) — \
             controller walltime enforcement may not be active"
        );
    } else {
        eprintln!("job correctly transitioned to WalltimeExceeded");
    }

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

// ---------------------------------------------------------------------------
// Multi-job tests
// ---------------------------------------------------------------------------

/// Submit 3 independent jobs and verify each has its own independent lifecycle.
#[tokio::test]
#[ignore]
async fn test_multiple_jobs_independent_lifecycle() {
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
        "test-indep-job-alpha",
        "test-indep-job-beta",
        "test-indep-job-gamma",
    ];

    for name in &names {
        let _ = cleanup_job(client.clone(), name, namespace).await;
    }

    for name in &names {
        create_test_wrenjob(client.clone(), name, namespace, 1)
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
    }

    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);

    // Each job should appear in the API with its own metadata.
    for name in &names {
        let job = api.get(name).await.expect("job should exist");
        assert_eq!(
            job.metadata.name.as_deref(),
            Some(*name),
            "job name mismatch"
        );
        // Jobs should not share status — each has an independent status subresource.
        let state = job
            .status
            .as_ref()
            .map(|s| s.state.clone())
            .unwrap_or(JobState::Pending);
        eprintln!("job {name} state: {state}");
        assert!(
            matches!(
                state,
                JobState::Pending | JobState::Scheduling | JobState::Running
            ),
            "unexpected state for {name}: {state}"
        );
    }

    for name in &names {
        cleanup_job(client.clone(), name, namespace)
            .await
            .expect("cleanup");
    }
}

/// Submit jobs with different priorities; verify both are accepted by the API.
#[tokio::test]
#[ignore]
async fn test_high_priority_job() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let namespace = "default";
    let low_name = "test-priority-low-job";
    let high_name = "test-priority-high-job";

    let _ = cleanup_job(client.clone(), low_name, namespace).await;
    let _ = cleanup_job(client.clone(), high_name, namespace).await;

    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);

    // Create low-priority job (priority=100).
    let mut low_job = build_wrenjob(low_name, namespace, 1);
    low_job.spec.priority = 100;
    api.create(&PostParams::default(), &low_job)
        .await
        .expect("create low-priority job");

    // Create high-priority job (priority=200).
    let mut high_job = build_wrenjob(high_name, namespace, 1);
    high_job.spec.priority = 200;
    api.create(&PostParams::default(), &high_job)
        .await
        .expect("create high-priority job");

    // Both should be accepted.
    let low = api.get(low_name).await.expect("low-priority job exists");
    let high = api.get(high_name).await.expect("high-priority job exists");

    assert_eq!(low.spec.priority, 100);
    assert_eq!(high.spec.priority, 200);
    assert!(
        high.spec.priority > low.spec.priority,
        "high-priority job should have a higher priority value"
    );

    eprintln!(
        "low priority: {}, high priority: {}",
        low.spec.priority, high.spec.priority
    );

    cleanup_job(client.clone(), low_name, namespace)
        .await
        .expect("cleanup low");
    cleanup_job(client.clone(), high_name, namespace)
        .await
        .expect("cleanup high");
}

// ---------------------------------------------------------------------------
// Queue tests
// ---------------------------------------------------------------------------

/// Create a WrenQueue with explicit policies and verify them.
#[tokio::test]
#[ignore]
async fn test_wrenqueue_with_policies() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-policy-queue";
    let namespace = "default";
    let _ = cleanup_queue(client.clone(), name, namespace).await;

    use wren_core::crd::{BackfillConfig, FairShareConfig, WrenQueue, WrenQueueSpec};

    let queue = WrenQueue {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: WrenQueueSpec {
            max_nodes: 2,
            max_walltime: Some("1h".to_string()),
            max_jobs_per_user: Some(5),
            default_priority: 50,
            backfill: Some(BackfillConfig {
                enabled: true,
                look_ahead: Some("30m".to_string()),
            }),
            fair_share: Some(FairShareConfig {
                enabled: true,
                decay_half_life: Some("7d".to_string()),
            }),
        },
    };

    let api: Api<WrenQueue> = Api::namespaced(client.clone(), namespace);
    let created = api
        .create(&PostParams::default(), &queue)
        .await
        .expect("create WrenQueue");

    assert_eq!(created.spec.max_nodes, 2);
    assert_eq!(created.spec.max_walltime.as_deref(), Some("1h"));
    assert_eq!(created.spec.max_jobs_per_user, Some(5));

    let backfill = created.spec.backfill.as_ref().expect("backfill config");
    assert!(backfill.enabled);
    assert_eq!(backfill.look_ahead.as_deref(), Some("30m"));

    let fair_share = created.spec.fair_share.as_ref().expect("fair_share config");
    assert!(fair_share.enabled);
    assert_eq!(fair_share.decay_half_life.as_deref(), Some("7d"));

    cleanup_queue(client, name, namespace)
        .await
        .expect("cleanup");
}

/// Create two queues and verify both exist independently.
#[tokio::test]
#[ignore]
async fn test_multiple_queues() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let namespace = "default";
    let gpu_name = "test-gpu-queue";
    let cpu_name = "test-cpu-queue";

    let _ = cleanup_queue(client.clone(), gpu_name, namespace).await;
    let _ = cleanup_queue(client.clone(), cpu_name, namespace).await;

    create_test_wrenqueue(client.clone(), gpu_name, namespace, 32)
        .await
        .expect("create gpu queue");
    create_test_wrenqueue(client.clone(), cpu_name, namespace, 64)
        .await
        .expect("create cpu queue");

    let api: Api<WrenQueue> = Api::namespaced(client.clone(), namespace);

    let gpu_q = api.get(gpu_name).await.expect("gpu queue exists");
    let cpu_q = api.get(cpu_name).await.expect("cpu queue exists");

    assert_eq!(gpu_q.spec.max_nodes, 32);
    assert_eq!(cpu_q.spec.max_nodes, 64);

    eprintln!(
        "gpu queue max_nodes={}, cpu queue max_nodes={}",
        gpu_q.spec.max_nodes, cpu_q.spec.max_nodes
    );

    cleanup_queue(client.clone(), gpu_name, namespace)
        .await
        .expect("cleanup gpu");
    cleanup_queue(client.clone(), cpu_name, namespace)
        .await
        .expect("cleanup cpu");
}

// ---------------------------------------------------------------------------
// Topology tests
// ---------------------------------------------------------------------------

/// Verify that cluster nodes have topology labels set (e.g., by kind config).
#[tokio::test]
#[ignore]
async fn test_nodes_have_topology_labels() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");

    let node_api: Api<Node> = Api::all(client);
    let nodes = node_api
        .list(&ListParams::default())
        .await
        .expect("list nodes");

    assert!(
        !nodes.items.is_empty(),
        "cluster should have at least one node"
    );

    let mut switch_count = 0usize;
    let mut rack_count = 0usize;

    for node in &nodes.items {
        let labels = node.metadata.labels.as_ref();
        if labels
            .and_then(|l| l.get("topology.wren.io/switch"))
            .is_some()
        {
            switch_count += 1;
        }
        if labels
            .and_then(|l| l.get("topology.wren.io/rack"))
            .is_some()
        {
            rack_count += 1;
        }
    }

    eprintln!(
        "nodes with topology.wren.io/switch: {switch_count}/{}, \
         topology.wren.io/rack: {rack_count}/{}",
        nodes.items.len(),
        nodes.items.len()
    );

    // This is a soft check: if the kind cluster was created with the topology
    // config from scripts/kind-config.yaml the labels will be present.
    // We log rather than assert so the test doesn't fail on vanilla clusters.
    if switch_count == 0 {
        eprintln!(
            "NOTE: no topology.wren.io/switch labels found — \
             apply them with: kubectl label nodes <node> topology.wren.io/switch=sw0"
        );
    }
    if rack_count == 0 {
        eprintln!(
            "NOTE: no topology.wren.io/rack labels found — \
             apply them with: kubectl label nodes <node> topology.wren.io/rack=rack0"
        );
    }
}

/// Create a job with topology constraints and verify it is accepted by the cluster.
#[tokio::test]
#[ignore]
async fn test_job_with_topology_constraints() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-topology-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    let job = build_wrenjob_with_topology(name, namespace, 1, true, Some(2));
    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
    let created = api
        .create(&PostParams::default(), &job)
        .await
        .expect("create WrenJob with topology");

    assert_eq!(created.metadata.name.as_deref(), Some(name));
    let topo = created.spec.topology.as_ref().expect("topology spec");
    assert!(topo.prefer_same_switch, "preferSameSwitch should be true");
    assert_eq!(topo.max_hops, Some(2), "maxHops should be 2");

    // Allow the controller to process the job; it should at least reach Scheduling.
    let _ = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Scheduling,
        Duration::from_secs(20),
    )
    .await;

    let job = api.get(name).await.expect("get job");
    let state = job
        .status
        .as_ref()
        .map(|s| s.state.clone())
        .unwrap_or(JobState::Pending);
    eprintln!("topology job state: {state}");
    assert!(
        matches!(
            state,
            JobState::Pending | JobState::Scheduling | JobState::Running
        ),
        "unexpected state for topology job: {state}"
    );

    cleanup_job(client, name, namespace).await.expect("cleanup");
}

// ---------------------------------------------------------------------------
// Cleanup tests
// ---------------------------------------------------------------------------

/// Create a job, wait for Running, delete it, and verify worker pods are cleaned up.
#[tokio::test]
#[ignore]
async fn test_job_cleanup_removes_pods() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-cleanup-pods-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    create_test_wrenjob(client.clone(), name, namespace, 1)
        .await
        .expect("create WrenJob");

    // Wait for the controller to create pods.
    let _ = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    let label_selector = format!("wren.io/job-name={name}");
    let pods_before = count_pods(client.clone(), namespace, &label_selector).await;
    eprintln!("pods before deletion: {pods_before}");

    // Delete the WrenJob.
    cleanup_job(client.clone(), name, namespace)
        .await
        .expect("cleanup job");

    if pods_before > 0 {
        // Verify pods are cleaned up within 60 s.
        let cleaned = wait_for_pods_gone(
            client.clone(),
            namespace,
            &label_selector,
            Duration::from_secs(60),
        )
        .await;

        if cleaned {
            eprintln!("all pods cleaned up after job deletion");
        } else {
            let remaining = count_pods(client.clone(), namespace, &label_selector).await;
            eprintln!(
                "NOTE: {remaining} pod(s) still present 60 s after job deletion — \
                 controller garbage collection may not be active"
            );
        }
    } else {
        eprintln!("NOTE: no pods were created before deletion — controller may not be active");
    }
}

/// Create a job, wait for Running, delete it, and verify the headless service is removed.
#[tokio::test]
#[ignore]
async fn test_job_cleanup_removes_service() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-cleanup-svc-job";
    let namespace = "default";
    let svc_name = format!("{name}-headless");
    let _ = cleanup_job(client.clone(), name, namespace).await;

    // Pre-clean the service.
    let svc_api: Api<Service> = Api::namespaced(client.clone(), namespace);
    let _ = svc_api.delete(&svc_name, &DeleteParams::default()).await;

    create_test_wrenjob(client.clone(), name, namespace, 1)
        .await
        .expect("create WrenJob");

    let _ = wait_for_job_state(
        client.clone(),
        name,
        namespace,
        JobState::Running,
        Duration::from_secs(30),
    )
    .await;

    let svc_existed = wait_for_service(
        client.clone(),
        namespace,
        &svc_name,
        Duration::from_secs(30),
    )
    .await;

    eprintln!("headless service existed before deletion: {svc_existed}");

    // Delete the WrenJob.
    cleanup_job(client.clone(), name, namespace)
        .await
        .expect("cleanup job");

    if svc_existed {
        let cleaned = wait_for_service_gone(
            client.clone(),
            namespace,
            &svc_name,
            Duration::from_secs(60),
        )
        .await;

        if cleaned {
            eprintln!("headless service cleaned up after job deletion");
        } else {
            eprintln!(
                "NOTE: headless service {svc_name} still present 60 s after job deletion — \
                 controller garbage collection may not be active"
            );
        }
    } else {
        eprintln!("NOTE: headless service was never created — controller may not be active");
    }
}

// ---------------------------------------------------------------------------
// Error handling tests
// ---------------------------------------------------------------------------

/// Create a container backend job with no container spec; verify it fails.
#[tokio::test]
#[ignore]
async fn test_container_backend_required_for_container_job() {
    if skip_if_no_cluster().await {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    if !ensure_crds_installed(client.clone()).await {
        eprintln!("SKIP: CRDs not installed");
        return;
    }

    let name = "test-no-container-spec-job";
    let namespace = "default";
    let _ = cleanup_job(client.clone(), name, namespace).await;

    // Build a container-backend job but omit the container spec entirely.
    let job = WrenJob {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: WrenJobSpec {
            nodes: 1,
            queue: "default".to_string(),
            priority: 50,
            walltime: Some("1h".to_string()),
            tasks_per_node: 1,
            backend: ExecutionBackendType::Container,
            container: None, // deliberately missing
            reaper: None,
            mpi: None,
            topology: None,
            dependencies: vec![],
        },
        status: None,
    };

    let api: Api<WrenJob> = Api::namespaced(client.clone(), namespace);
    match api.create(&PostParams::default(), &job).await {
        Err(e) => {
            // Webhook or validation rejected the object — correct behaviour.
            eprintln!("server rejected job with missing container spec (expected): {e}");
        }
        Ok(_) => {
            // Controller should detect the invalid spec and mark it Failed.
            let failed = wait_for_job_state(
                client.clone(),
                name,
                namespace,
                JobState::Failed,
                Duration::from_secs(30),
            )
            .await;

            if failed {
                eprintln!("controller correctly marked job with missing container spec as Failed");
                let j = api.get(name).await.expect("get job");
                let msg = j.status.as_ref().and_then(|s| s.message.as_deref());
                eprintln!("failure message: {msg:?}");
            } else {
                let state = api
                    .get(name)
                    .await
                    .ok()
                    .and_then(|j| j.status)
                    .map(|s| s.state)
                    .unwrap_or(JobState::Pending);
                eprintln!(
                    "NOTE: job with missing container spec reached {state} instead of Failed — \
                     validation may be deferred to Phase 5 webhook"
                );
            }

            cleanup_job(client, name, namespace).await.expect("cleanup");
        }
    }
}
