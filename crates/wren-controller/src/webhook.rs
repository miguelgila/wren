use axum::{http::StatusCode, routing::get, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use wren_core::{ExecutionBackendType, WalltimeDuration, WrenJobSpec, WrenQueueSpec};

/// Admission review request/response types (simplified, matching K8s API).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionReview<T> {
    pub api_version: String,
    pub kind: String,
    pub request: AdmissionRequest<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionRequest<T> {
    pub uid: String,
    pub object: T,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionResponse {
    pub api_version: String,
    pub kind: String,
    pub response: AdmissionResponseBody,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionResponseBody {
    pub uid: String,
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<AdmissionStatus>,
}

#[derive(Serialize, Deserialize)]
pub struct AdmissionStatus {
    pub code: u16,
    pub message: String,
}

/// Validate an WrenJobSpec. Returns Ok(()) or Err with a list of descriptive messages.
pub fn validate_wrenjob(spec: &WrenJobSpec) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    // Rule 1: nodes must be > 0
    if spec.nodes == 0 {
        errors.push("nodes must be greater than 0".to_string());
    }

    // Rule 2: tasks_per_node must be > 0
    if spec.tasks_per_node == 0 {
        errors.push("tasksPerNode must be greater than 0".to_string());
    }

    // Rule 3: if walltime is set, it must parse successfully
    if let Some(wt) = &spec.walltime {
        if WalltimeDuration::parse(wt).is_err() {
            errors.push(format!(
                "walltime '{}' is invalid; use formats like '4h', '30m', '1d', '2h30m'",
                wt
            ));
        }
    }

    // Rule 4: container backend requires a container spec with non-empty image
    if spec.backend == ExecutionBackendType::Container {
        match &spec.container {
            None => errors
                .push("backend 'container' requires a container spec to be provided".to_string()),
            Some(c) if c.image.trim().is_empty() => {
                errors.push("container.image must not be empty".to_string())
            }
            _ => {}
        }
    }

    // Rule 5: reaper backend requires a reaper spec with non-empty script
    if spec.backend == ExecutionBackendType::Reaper {
        match &spec.reaper {
            None => {
                errors.push("backend 'reaper' requires a reaper spec to be provided".to_string())
            }
            Some(r) if r.script.trim().is_empty() => {
                errors.push("reaper.script must not be empty".to_string())
            }
            _ => {}
        }
    }

    // Rule 6: priority must be in [-10000, 10000]
    if spec.priority < -10000 || spec.priority > 10000 {
        errors.push(format!(
            "priority {} is out of range; must be between -10000 and 10000",
            spec.priority
        ));
    }

    // Rule 7: queue must be non-empty
    if spec.queue.trim().is_empty() {
        errors.push("queue must not be empty".to_string());
    }

    // Rule 8: dependency job names must be non-empty
    for (i, dep) in spec.dependencies.iter().enumerate() {
        if dep.job.trim().is_empty() {
            errors.push(format!("dependencies[{}].job must not be empty", i));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate a WrenQueueSpec. Returns Ok(()) or Err with a list of descriptive messages.
pub fn validate_wrenqueue(spec: &WrenQueueSpec) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    // Rule 1: max_nodes must be > 0
    if spec.max_nodes == 0 {
        errors.push("maxNodes must be greater than 0".to_string());
    }

    // Rule 2: if max_walltime is set, it must parse successfully
    if let Some(wt) = &spec.max_walltime {
        if WalltimeDuration::parse(wt).is_err() {
            errors.push(format!(
                "maxWalltime '{}' is invalid; use formats like '4h', '30m', '1d', '2h30m'",
                wt
            ));
        }
    }

    // Rule 3: default_priority must be in [-10000, 10000]
    if spec.default_priority < -10000 || spec.default_priority > 10000 {
        errors.push(format!(
            "defaultPriority {} is out of range; must be between -10000 and 10000",
            spec.default_priority
        ));
    }

    // Rule 4: if max_jobs_per_user is set, it must be > 0
    if let Some(max) = spec.max_jobs_per_user {
        if max == 0 {
            errors.push("maxJobsPerUser must be greater than 0 if set".to_string());
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn admission_allowed(uid: String) -> AdmissionResponse {
    AdmissionResponse {
        api_version: "admission.k8s.io/v1".to_string(),
        kind: "AdmissionReview".to_string(),
        response: AdmissionResponseBody {
            uid,
            allowed: true,
            status: None,
        },
    }
}

fn admission_denied(uid: String, errors: Vec<String>) -> AdmissionResponse {
    let message = errors.join("; ");
    AdmissionResponse {
        api_version: "admission.k8s.io/v1".to_string(),
        kind: "AdmissionReview".to_string(),
        response: AdmissionResponseBody {
            uid,
            allowed: false,
            status: Some(AdmissionStatus { code: 422, message }),
        },
    }
}

async fn validate_wrenjob_handler(
    Json(review): Json<AdmissionReview<WrenJobSpec>>,
) -> (StatusCode, Json<AdmissionResponse>) {
    let uid = review.request.uid.clone();
    let spec = &review.request.object;

    match validate_wrenjob(spec) {
        Ok(()) => {
            info!(uid, "WrenJob admission: allowed");
            (StatusCode::OK, Json(admission_allowed(uid)))
        }
        Err(errors) => {
            warn!(uid, ?errors, "WrenJob admission: denied");
            (StatusCode::OK, Json(admission_denied(uid, errors)))
        }
    }
}

async fn validate_wrenqueue_handler(
    Json(review): Json<AdmissionReview<WrenQueueSpec>>,
) -> (StatusCode, Json<AdmissionResponse>) {
    let uid = review.request.uid.clone();
    let spec = &review.request.object;

    match validate_wrenqueue(spec) {
        Ok(()) => {
            info!(uid, "WrenQueue admission: allowed");
            (StatusCode::OK, Json(admission_allowed(uid)))
        }
        Err(errors) => {
            warn!(uid, ?errors, "WrenQueue admission: denied");
            (StatusCode::OK, Json(admission_denied(uid, errors)))
        }
    }
}

async fn health_handler() -> &'static str {
    "ok"
}

/// Build an axum Router for the webhook endpoints.
pub fn webhook_router() -> Router {
    Router::new()
        .route("/validate/wrenjob", post(validate_wrenjob_handler))
        .route("/validate/wrenqueue", post(validate_wrenqueue_handler))
        .route("/healthz", get(health_handler))
}

/// Start the webhook HTTPS server (plain HTTP — TLS termination handled by cert-manager sidecar or
/// an ingress controller in the cluster; the Helm chart wires this up).
pub async fn serve_webhooks(port: u16) -> anyhow::Result<()> {
    let app = webhook_router();

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!(port, "webhook server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as AxumStatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use wren_core::{ContainerSpec, DependencyType, JobDependency, ReaperSpec};

    fn container_spec(image: &str) -> ContainerSpec {
        ContainerSpec {
            image: image.to_string(),
            command: vec![],
            args: vec![],
            resources: None,
            host_network: false,
            volume_mounts: vec![],
            env: vec![],
        }
    }

    fn valid_wrenjob_spec() -> WrenJobSpec {
        WrenJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes: 4,
            tasks_per_node: 1,
            backend: ExecutionBackendType::Container,
            container: Some(container_spec("pytorch:latest")),
            reaper: None,
            mpi: None,
            topology: None,
            dependencies: vec![],
        }
    }

    fn valid_wrenqueue_spec() -> WrenQueueSpec {
        WrenQueueSpec {
            max_nodes: 128,
            max_walltime: None,
            max_jobs_per_user: None,
            default_priority: 50,
            backfill: None,
            fair_share: None,
        }
    }

    // --- WrenJob tests ---

    #[test]
    fn test_wrenjob_valid_container() {
        assert!(validate_wrenjob(&valid_wrenjob_spec()).is_ok());
    }

    #[test]
    fn test_wrenjob_nodes_zero() {
        let mut spec = valid_wrenjob_spec();
        spec.nodes = 0;
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.contains("nodes must be greater than 0")));
    }

    #[test]
    fn test_wrenjob_tasks_per_node_zero() {
        let mut spec = valid_wrenjob_spec();
        spec.tasks_per_node = 0;
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("tasksPerNode")));
    }

    #[test]
    fn test_wrenjob_invalid_walltime() {
        let mut spec = valid_wrenjob_spec();
        spec.walltime = Some("4x".to_string());
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("walltime")));
    }

    #[test]
    fn test_wrenjob_valid_walltime() {
        let mut spec = valid_wrenjob_spec();
        spec.walltime = Some("2h30m".to_string());
        assert!(validate_wrenjob(&spec).is_ok());
    }

    #[test]
    fn test_wrenjob_container_backend_missing_spec() {
        let mut spec = valid_wrenjob_spec();
        spec.backend = ExecutionBackendType::Container;
        spec.container = None;
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("container spec")));
    }

    #[test]
    fn test_wrenjob_container_backend_empty_image() {
        let mut spec = valid_wrenjob_spec();
        spec.container = Some(container_spec("  "));
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("container.image")));
    }

    #[test]
    fn test_wrenjob_reaper_backend_valid() {
        let spec = WrenJobSpec {
            backend: ExecutionBackendType::Reaper,
            container: None,
            reaper: Some(ReaperSpec {
                script: "#!/bin/bash\nsrun ./app".to_string(),
                environment: Default::default(),
                working_dir: None,
            }),
            ..valid_wrenjob_spec()
        };
        assert!(validate_wrenjob(&spec).is_ok());
    }

    #[test]
    fn test_wrenjob_reaper_backend_missing_spec() {
        let spec = WrenJobSpec {
            backend: ExecutionBackendType::Reaper,
            container: None,
            reaper: None,
            ..valid_wrenjob_spec()
        };
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("reaper spec")));
    }

    #[test]
    fn test_wrenjob_reaper_backend_empty_script() {
        let spec = WrenJobSpec {
            backend: ExecutionBackendType::Reaper,
            container: None,
            reaper: Some(ReaperSpec {
                script: "   ".to_string(),
                environment: Default::default(),
                working_dir: None,
            }),
            ..valid_wrenjob_spec()
        };
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("reaper.script")));
    }

    #[test]
    fn test_wrenjob_priority_out_of_range_high() {
        let mut spec = valid_wrenjob_spec();
        spec.priority = 10001;
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("priority")));
    }

    #[test]
    fn test_wrenjob_priority_out_of_range_low() {
        let mut spec = valid_wrenjob_spec();
        spec.priority = -10001;
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("priority")));
    }

    #[test]
    fn test_wrenjob_priority_boundary_values_valid() {
        let mut spec = valid_wrenjob_spec();
        spec.priority = -10000;
        assert!(validate_wrenjob(&spec).is_ok());
        spec.priority = 10000;
        assert!(validate_wrenjob(&spec).is_ok());
    }

    #[test]
    fn test_wrenjob_empty_queue() {
        let mut spec = valid_wrenjob_spec();
        spec.queue = "  ".to_string();
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("queue")));
    }

    #[test]
    fn test_wrenjob_empty_dependency_job_name() {
        let mut spec = valid_wrenjob_spec();
        spec.dependencies = vec![JobDependency {
            dep_type: DependencyType::AfterOk,
            job: "".to_string(),
        }];
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("dependencies[0].job")));
    }

    #[test]
    fn test_wrenjob_valid_dependency() {
        let mut spec = valid_wrenjob_spec();
        spec.dependencies = vec![JobDependency {
            dep_type: DependencyType::AfterOk,
            job: "previous-sim".to_string(),
        }];
        assert!(validate_wrenjob(&spec).is_ok());
    }

    #[test]
    fn test_wrenjob_multiple_errors_collected() {
        let spec = WrenJobSpec {
            queue: "".to_string(),
            priority: 99999,
            walltime: Some("bad".to_string()),
            nodes: 0,
            tasks_per_node: 0,
            backend: ExecutionBackendType::Container,
            container: None,
            reaper: None,
            mpi: None,
            topology: None,
            dependencies: vec![],
        };
        let errs = validate_wrenjob(&spec).unwrap_err();
        assert!(errs.len() >= 4, "expected multiple errors, got: {:?}", errs);
    }

    // --- WrenQueue tests ---

    #[test]
    fn test_wrenqueue_valid() {
        assert!(validate_wrenqueue(&valid_wrenqueue_spec()).is_ok());
    }

    #[test]
    fn test_wrenqueue_max_nodes_zero() {
        let mut spec = valid_wrenqueue_spec();
        spec.max_nodes = 0;
        let errs = validate_wrenqueue(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("maxNodes")));
    }

    #[test]
    fn test_wrenqueue_invalid_max_walltime() {
        let mut spec = valid_wrenqueue_spec();
        spec.max_walltime = Some("notavalid".to_string());
        let errs = validate_wrenqueue(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("maxWalltime")));
    }

    #[test]
    fn test_wrenqueue_valid_max_walltime() {
        let mut spec = valid_wrenqueue_spec();
        spec.max_walltime = Some("24h".to_string());
        assert!(validate_wrenqueue(&spec).is_ok());
    }

    #[test]
    fn test_wrenqueue_default_priority_out_of_range() {
        let mut spec = valid_wrenqueue_spec();
        spec.default_priority = -20000;
        let errs = validate_wrenqueue(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("defaultPriority")));
    }

    #[test]
    fn test_wrenqueue_max_jobs_per_user_zero() {
        let mut spec = valid_wrenqueue_spec();
        spec.max_jobs_per_user = Some(0);
        let errs = validate_wrenqueue(&spec).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("maxJobsPerUser")));
    }

    #[test]
    fn test_wrenqueue_max_jobs_per_user_valid() {
        let mut spec = valid_wrenqueue_spec();
        spec.max_jobs_per_user = Some(10);
        assert!(validate_wrenqueue(&spec).is_ok());
    }

    #[test]
    fn test_wrenqueue_max_jobs_per_user_none_is_valid() {
        let mut spec = valid_wrenqueue_spec();
        spec.max_jobs_per_user = None;
        assert!(validate_wrenqueue(&spec).is_ok());
    }

    #[test]
    fn test_wrenqueue_priority_boundary_values_valid() {
        let mut spec = valid_wrenqueue_spec();
        spec.default_priority = 10000;
        assert!(validate_wrenqueue(&spec).is_ok());
        spec.default_priority = -10000;
        assert!(validate_wrenqueue(&spec).is_ok());
    }

    // --- admission response helpers ---

    #[test]
    fn test_admission_allowed_response() {
        let resp = admission_allowed("uid-123".to_string());
        assert_eq!(resp.api_version, "admission.k8s.io/v1");
        assert_eq!(resp.kind, "AdmissionReview");
        assert_eq!(resp.response.uid, "uid-123");
        assert!(resp.response.allowed);
        assert!(resp.response.status.is_none());
    }

    #[test]
    fn test_admission_denied_response() {
        let errors = vec![
            "nodes must be > 0".to_string(),
            "queue is empty".to_string(),
        ];
        let resp = admission_denied("uid-456".to_string(), errors);
        assert_eq!(resp.response.uid, "uid-456");
        assert!(!resp.response.allowed);
        let status = resp.response.status.unwrap();
        assert_eq!(status.code, 422);
        assert!(status.message.contains("nodes must be > 0"));
        assert!(status.message.contains("queue is empty"));
        assert!(status.message.contains("; "));
    }

    #[test]
    fn test_admission_denied_single_error() {
        let errors = vec!["single error".to_string()];
        let resp = admission_denied("uid-789".to_string(), errors);
        assert!(!resp.response.allowed);
        let status = resp.response.status.unwrap();
        assert_eq!(status.message, "single error");
    }

    // --- AdmissionResponse serialization ---

    #[test]
    fn test_admission_response_serialization() {
        let resp = admission_allowed("uid-ser".to_string());
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"apiVersion\":\"admission.k8s.io/v1\""));
        assert!(json.contains("\"allowed\":true"));
        assert!(json.contains("\"uid\":\"uid-ser\""));
    }

    #[test]
    fn test_admission_denied_serialization_includes_status() {
        let resp = admission_denied("uid-denied".to_string(), vec!["bad".to_string()]);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"allowed\":false"));
        assert!(json.contains("\"code\":422"));
        assert!(json.contains("bad"));
    }

    // --- handler / router tests ---

    #[tokio::test]
    async fn test_validate_wrenjob_handler_valid() {
        let app = webhook_router();

        let body = serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid-1",
                "object": {
                    "nodes": 4,
                    "queue": "default",
                    "tasksPerNode": 1,
                    "backend": "container",
                    "container": {
                        "image": "pytorch:latest",
                        "command": [],
                        "args": [],
                        "hostNetwork": false,
                        "volumeMounts": [],
                        "env": []
                    },
                    "priority": 50,
                    "dependencies": []
                }
            }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/validate/wrenjob")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), AxumStatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let resp: AdmissionResponse = serde_json::from_slice(&body_bytes).unwrap();
        assert!(resp.response.allowed);
        assert_eq!(resp.response.uid, "test-uid-1");
    }

    #[tokio::test]
    async fn test_validate_wrenjob_handler_invalid() {
        let app = webhook_router();

        let body = serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid-2",
                "object": {
                    "nodes": 0,
                    "queue": "default",
                    "tasksPerNode": 1,
                    "backend": "container",
                    "container": {
                        "image": "pytorch:latest",
                        "command": [],
                        "args": [],
                        "hostNetwork": false,
                        "volumeMounts": [],
                        "env": []
                    },
                    "priority": 50,
                    "dependencies": []
                }
            }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/validate/wrenjob")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), AxumStatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let resp: AdmissionResponse = serde_json::from_slice(&body_bytes).unwrap();
        assert!(!resp.response.allowed);
        assert!(resp.response.status.unwrap().message.contains("nodes"));
    }

    #[tokio::test]
    async fn test_validate_wrenqueue_handler_valid() {
        let app = webhook_router();

        let body = serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid-3",
                "object": {
                    "maxNodes": 128,
                    "defaultPriority": 50
                }
            }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/validate/wrenqueue")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), AxumStatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let resp: AdmissionResponse = serde_json::from_slice(&body_bytes).unwrap();
        assert!(resp.response.allowed);
        assert_eq!(resp.response.uid, "test-uid-3");
    }

    #[tokio::test]
    async fn test_validate_wrenqueue_handler_invalid() {
        let app = webhook_router();

        let body = serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid-4",
                "object": {
                    "maxNodes": 0,
                    "defaultPriority": 50
                }
            }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/validate/wrenqueue")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), AxumStatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let resp: AdmissionResponse = serde_json::from_slice(&body_bytes).unwrap();
        assert!(!resp.response.allowed);
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let app = webhook_router();

        let request = Request::builder()
            .method("GET")
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), AxumStatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body_bytes[..], b"ok");
    }
}
