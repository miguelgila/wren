use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::Client;
use std::collections::BTreeMap;
use tracing::{info, warn};
use wren_core::WrenError;

const CONFIGMAP_NAME: &str = "wren-job-id-counter";
const COUNTER_KEY: &str = "next_job_id";

/// Allocates sequential numeric job IDs, persisted in a ConfigMap.
///
/// With leader election active, only one controller instance writes at a time,
/// so simple read-modify-write on the ConfigMap is safe.
pub struct JobIdAllocator {
    api: Api<ConfigMap>,
}

impl JobIdAllocator {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
        }
    }

    /// Allocate the next job ID, persisting the updated counter.
    pub async fn allocate(&self) -> Result<u64, WrenError> {
        let current = self.read_counter().await?;
        let job_id = current;
        self.write_counter(current + 1).await?;
        info!(job_id, "allocated job ID");
        Ok(job_id)
    }

    /// Read the current counter value from the ConfigMap, creating it if needed.
    async fn read_counter(&self) -> Result<u64, WrenError> {
        match self
            .api
            .get_opt(CONFIGMAP_NAME)
            .await
            .map_err(WrenError::KubeError)?
        {
            Some(cm) => {
                let val = cm
                    .data
                    .as_ref()
                    .and_then(|d| d.get(COUNTER_KEY))
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1);
                Ok(val)
            }
            None => {
                // Create the ConfigMap with initial value
                self.create_configmap(1).await?;
                Ok(1)
            }
        }
    }

    /// Write the counter value back to the ConfigMap.
    async fn write_counter(&self, next: u64) -> Result<(), WrenError> {
        let patch = serde_json::json!({
            "data": {
                COUNTER_KEY: next.to_string()
            }
        });
        self.api
            .patch(
                CONFIGMAP_NAME,
                &PatchParams::apply("wren-controller"),
                &Patch::Merge(&patch),
            )
            .await
            .map_err(WrenError::KubeError)?;
        Ok(())
    }

    /// Create the counter ConfigMap for the first time.
    async fn create_configmap(&self, initial: u64) -> Result<(), WrenError> {
        let cm = ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some(CONFIGMAP_NAME.to_string()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([(
                COUNTER_KEY.to_string(),
                initial.to_string(),
            )])),
            ..Default::default()
        };
        match self.api.create(&PostParams::default(), &cm).await {
            Ok(_) => {
                info!("created job ID counter ConfigMap");
                Ok(())
            }
            Err(kube::Error::Api(resp)) if resp.code == 409 => {
                // Already exists (race with another startup) — that's fine
                warn!("job ID counter ConfigMap already exists (409)");
                Ok(())
            }
            Err(e) => Err(WrenError::KubeError(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_configmap_name_is_stable() {
        assert_eq!(CONFIGMAP_NAME, "wren-job-id-counter");
    }

    #[test]
    fn test_counter_key_is_stable() {
        assert_eq!(COUNTER_KEY, "next_job_id");
    }

    // -------------------------------------------------------------------------
    // Mock K8s client helpers
    // -------------------------------------------------------------------------

    /// Build a mock K8s client that routes responses based on HTTP method.
    /// GET returns the `get_response`, POST returns `post_response`,
    /// PATCH returns 200 with `{}`.
    #[allow(clippy::type_complexity)]
    fn make_allocator_client(
        get_status: u16,
        get_body: &str,
        post_status: u16,
        post_body: &str,
    ) -> (JobIdAllocator, Arc<Mutex<Vec<(String, String)>>>) {
        use bytes::Bytes;
        use http_body_util::Full;
        use tower::service_fn;

        let calls = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let calls_clone = calls.clone();
        let get_body = Bytes::from(get_body.to_owned());
        let post_body = Bytes::from(post_body.to_owned());
        let svc = service_fn(move |req: http::Request<_>| {
            let method = req.method().to_string();
            let uri = req.uri().path().to_string();
            calls_clone.lock().unwrap().push((method.clone(), uri));
            let get_body = get_body.clone();
            let post_body = post_body.clone();
            let get_status = get_status;
            let post_status = post_status;
            async move {
                let (status, body) = match method.as_str() {
                    "GET" => (get_status, get_body),
                    "POST" => (post_status, post_body),
                    _ => (200u16, Bytes::from("{}")), // PATCH
                };
                let resp = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Full::from(body))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        let client = Client::new(svc, "default");
        (JobIdAllocator::new(client, "default"), calls)
    }

    fn configmap_json(counter: u64) -> String {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "wren-job-id-counter",
                "namespace": "default"
            },
            "data": {
                "next_job_id": counter.to_string()
            }
        })
        .to_string()
    }

    fn configmap_json_bad_value() -> String {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "wren-job-id-counter",
                "namespace": "default"
            },
            "data": {
                "next_job_id": "not-a-number"
            }
        })
        .to_string()
    }

    fn not_found_json() -> String {
        serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": "configmaps \"wren-job-id-counter\" not found",
            "reason": "NotFound",
            "code": 404
        })
        .to_string()
    }

    fn created_json() -> String {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "wren-job-id-counter",
                "namespace": "default"
            },
            "data": {
                "next_job_id": "1"
            }
        })
        .to_string()
    }

    // -------------------------------------------------------------------------
    // Async tests for JobIdAllocator
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_allocate_returns_current_and_increments() {
        let (allocator, calls) =
            make_allocator_client(200, &configmap_json(42), 200, &created_json());
        let id = allocator.allocate().await.unwrap();
        assert_eq!(id, 42);
        // Should have done: GET (read_counter), PATCH (write_counter with 43)
        let calls = calls.lock().unwrap();
        assert!(calls.iter().any(|(m, _)| m == "GET"), "should read counter");
        assert!(
            calls.iter().any(|(m, _)| m == "PATCH"),
            "should write incremented counter"
        );
    }

    #[tokio::test]
    async fn test_allocate_creates_configmap_when_missing() {
        // GET returns 404 → create_configmap (POST) → returns 1
        // Then write_counter (PATCH) with 2
        let (allocator, calls) =
            make_allocator_client(404, &not_found_json(), 201, &created_json());
        let id = allocator.allocate().await.unwrap();
        assert_eq!(id, 1);
        let calls = calls.lock().unwrap();
        assert!(
            calls.iter().any(|(m, _)| m == "POST"),
            "should create ConfigMap"
        );
    }

    #[tokio::test]
    async fn test_read_counter_falls_back_on_bad_value() {
        let (allocator, _) =
            make_allocator_client(200, &configmap_json_bad_value(), 200, &created_json());
        // allocate should fall back to 1 when the value is not parseable
        let id = allocator.allocate().await.unwrap();
        assert_eq!(id, 1);
    }

    #[tokio::test]
    async fn test_read_counter_reads_existing_value() {
        let (allocator, _) = make_allocator_client(200, &configmap_json(100), 200, &created_json());
        let id = allocator.allocate().await.unwrap();
        assert_eq!(id, 100);
    }

    #[tokio::test]
    async fn test_create_configmap_tolerates_409() {
        // Simulate: GET=404 (not found) → POST=409 (already exists) → PATCH=200
        let conflict_body = serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": "configmaps \"wren-job-id-counter\" already exists",
            "reason": "AlreadyExists",
            "code": 409
        })
        .to_string();
        let (allocator, _) = make_allocator_client(404, &not_found_json(), 409, &conflict_body);
        let id = allocator.allocate().await.unwrap();
        assert_eq!(id, 1, "should still return 1 after tolerating 409");
    }
}
