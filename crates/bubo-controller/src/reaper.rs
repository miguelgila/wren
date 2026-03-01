use async_trait::async_trait;
use bubo_core::backend::{BackendJobStatus, ExecutionBackend, LaunchResult};
use bubo_core::{BuboError, BuboJobSpec, Placement};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info, warn};

use crate::mpi;

/// Default port on which Reaper agents listen.
const DEFAULT_REAPER_AGENT_PORT: u16 = 8443;

/// Request body sent to a Reaper agent when submitting a job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReaperJobRequest {
    /// Shell script to execute on bare metal.
    pub script: String,
    /// Environment variables for the job process.
    pub environment: HashMap<String, String>,
    /// Working directory for the job process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// Response returned by a Reaper agent after submitting a job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReaperJobResponse {
    /// Opaque job identifier assigned by the Reaper agent.
    pub job_id: String,
    /// Initial status reported at submission time.
    pub status: ReaperJobState,
}

/// Status of a job as reported by the Reaper agent's GET /api/v1/jobs/{id} endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReaperJobStatus {
    pub job_id: String,
    pub status: ReaperJobState,
    /// Exit code, populated when the job has finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Human-readable message (e.g. error description).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Possible states a Reaper-managed job can be in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReaperJobState {
    /// Job is queued / waiting to start.
    Pending,
    /// Job process is actively running.
    Running,
    /// Job completed successfully (exit code 0).
    Succeeded,
    /// Job exited with a non-zero exit code or was killed.
    Failed,
    /// Status could not be determined.
    Unknown,
}

/// Bare-metal execution backend that dispatches jobs to Reaper agents running on
/// each allocated node.
///
/// The Reaper agent API:
/// - `POST /api/v1/jobs`           — submit a job, returns [`ReaperJobResponse`]
/// - `GET  /api/v1/jobs/{job_id}`  — poll status, returns [`ReaperJobStatus`]
/// - `DELETE /api/v1/jobs/{job_id}`— terminate a running job
///
/// Node → agent URL mapping is derived from the Kubernetes node name combined with
/// `REAPER_AGENT_PORT` (default `8443`). The scheme is always `http` unless
/// `REAPER_AGENT_SCHEME` is set to `https`.
pub struct ReaperBackend {
    http: HttpClient,
    /// TCP port the Reaper agent listens on.
    agent_port: u16,
    /// URL scheme: "http" or "https".
    agent_scheme: String,
    /// Tracks the launcher node + Reaper job id for each bubo job name.
    /// Key: bubo job name, Value: (launcher_node, reaper_job_id).
    active_jobs: tokio::sync::Mutex<HashMap<String, (String, String)>>,
    /// All nodes participating in a job, used for broadcast terminate.
    /// Key: bubo job name, Value: list of node names.
    job_nodes: tokio::sync::Mutex<HashMap<String, Vec<String>>>,
}

impl ReaperBackend {
    /// Create a new `ReaperBackend`, reading configuration from environment variables.
    ///
    /// Environment variables:
    /// - `REAPER_AGENT_PORT`   — agent TCP port (default `8443`)
    /// - `REAPER_AGENT_SCHEME` — `http` or `https` (default `http`)
    pub fn new() -> Self {
        let agent_port = std::env::var("REAPER_AGENT_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(DEFAULT_REAPER_AGENT_PORT);

        let agent_scheme = std::env::var("REAPER_AGENT_SCHEME")
            .unwrap_or_else(|_| "http".to_string());

        Self {
            http: HttpClient::new(),
            agent_port,
            agent_scheme,
            active_jobs: tokio::sync::Mutex::new(HashMap::new()),
            job_nodes: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Build the base URL for the Reaper agent running on `node_name`.
    ///
    /// Kubernetes node names are used directly as hostnames, which works within
    /// a cluster where node names resolve via DNS or /etc/hosts.
    pub fn agent_url(&self, node_name: &str) -> String {
        format!("{}://{}:{}", self.agent_scheme, node_name, self.agent_port)
    }

    /// Submit a job to the Reaper agent on `node_name`.
    async fn submit_to_agent(
        &self,
        node_name: &str,
        request: &ReaperJobRequest,
    ) -> Result<ReaperJobResponse, BuboError> {
        let url = format!("{}/api/v1/jobs", self.agent_url(node_name));
        debug!(node = %node_name, url = %url, "submitting job to Reaper agent");

        let response = self
            .http
            .post(&url)
            .json(request)
            .send()
            .await
            .map_err(|e| BuboError::BackendError {
                message: format!("HTTP request to Reaper agent on {node_name} failed: {e}"),
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(BuboError::BackendError {
                message: format!(
                    "Reaper agent on {node_name} returned {status}: {body}"
                ),
            });
        }

        response.json::<ReaperJobResponse>().await.map_err(|e| {
            BuboError::BackendError {
                message: format!("failed to parse Reaper agent response from {node_name}: {e}"),
            }
        })
    }

    /// Poll the status of a job from the Reaper agent on `node_name`.
    async fn poll_agent_status(
        &self,
        node_name: &str,
        job_id: &str,
    ) -> Result<ReaperJobStatus, BuboError> {
        let url = format!("{}/api/v1/jobs/{}", self.agent_url(node_name), job_id);
        debug!(node = %node_name, job_id = %job_id, "polling job status from Reaper agent");

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BuboError::BackendError {
                message: format!("HTTP GET to Reaper agent on {node_name} failed: {e}"),
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(ReaperJobStatus {
                job_id: job_id.to_string(),
                status: ReaperJobState::Unknown,
                exit_code: None,
                message: Some("job not found on agent".to_string()),
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(BuboError::BackendError {
                message: format!(
                    "Reaper agent on {node_name} returned {status} for status poll: {body}"
                ),
            });
        }

        response.json::<ReaperJobStatus>().await.map_err(|e| {
            BuboError::BackendError {
                message: format!(
                    "failed to parse status response from Reaper agent on {node_name}: {e}"
                ),
            }
        })
    }

    /// Send a DELETE request to terminate a job on the Reaper agent.
    async fn terminate_on_agent(&self, node_name: &str, job_id: &str) {
        let url = format!("{}/api/v1/jobs/{}", self.agent_url(node_name), job_id);
        debug!(node = %node_name, job_id = %job_id, "terminating job on Reaper agent");

        match self.http.delete(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                debug!(node = %node_name, job_id = %job_id, "job terminated on Reaper agent");
            }
            Ok(resp) => {
                warn!(
                    node = %node_name,
                    job_id = %job_id,
                    status = %resp.status(),
                    "unexpected status when terminating job on Reaper agent"
                );
            }
            Err(e) => {
                warn!(
                    node = %node_name,
                    job_id = %job_id,
                    error = %e,
                    "failed to send terminate request to Reaper agent"
                );
            }
        }
    }

    /// Build the job request for a specific node, injecting MPI environment variables.
    fn build_job_request(
        &self,
        spec: &BuboJobSpec,
        placement: &Placement,
        rank: u32,
    ) -> Result<ReaperJobRequest, BuboError> {
        let reaper_spec = spec
            .reaper
            .as_ref()
            .ok_or_else(|| BuboError::ValidationError {
                reason: "reaper spec required for reaper backend".to_string(),
            })?;

        let hostfile_content = mpi::generate_hostfile(placement, spec.tasks_per_node);
        let total = mpi::total_ranks(spec);

        let mut environment = reaper_spec.environment.clone();
        environment.insert("BUBO_MPI_RANK".to_string(), rank.to_string());
        environment.insert("BUBO_TOTAL_RANKS".to_string(), total.to_string());
        environment.insert("BUBO_NUM_NODES".to_string(), spec.nodes.to_string());
        environment.insert("BUBO_HOSTFILE".to_string(), hostfile_content);

        // Expose MPI implementation so the script can load the right module
        if let Some(mpi_spec) = &spec.mpi {
            environment.insert(
                "BUBO_MPI_IMPL".to_string(),
                mpi_spec.implementation.clone(),
            );
            if let Some(ref iface) = mpi_spec.fabric_interface {
                environment.insert("BUBO_FABRIC_INTERFACE".to_string(), iface.clone());
            }
        }

        Ok(ReaperJobRequest {
            script: reaper_spec.script.clone(),
            environment,
            working_dir: reaper_spec.working_dir.clone(),
        })
    }
}

impl Default for ReaperBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionBackend for ReaperBackend {
    async fn launch(
        &self,
        job_name: &str,
        _namespace: &str,
        spec: &BuboJobSpec,
        placement: &Placement,
    ) -> Result<LaunchResult, BuboError> {
        info!(
            job = job_name,
            nodes = ?placement.nodes,
            "launching bare-metal MPI job via Reaper"
        );

        let mut resource_ids = Vec::new();
        let mut launcher_job_id: Option<String> = None;
        let launcher_node = placement
            .nodes
            .first()
            .cloned()
            .ok_or_else(|| BuboError::ValidationError {
                reason: "placement must contain at least one node".to_string(),
            })?;

        for (rank, node) in placement.nodes.iter().enumerate() {
            let request = self.build_job_request(spec, placement, rank as u32)?;
            let resp = self.submit_to_agent(node, &request).await?;

            debug!(
                job = job_name,
                node = %node,
                rank,
                reaper_job_id = %resp.job_id,
                "submitted job to Reaper agent"
            );

            let id_label = format!("{}:{}", node, resp.job_id);
            resource_ids.push(id_label);

            // Track the launcher node's job ID for status polling.
            if rank == 0 {
                launcher_job_id = Some(resp.job_id);
            }
        }

        // Persist mapping so status/terminate/cleanup can look it up.
        let launcher_job_id = launcher_job_id.expect("rank 0 always exists");
        {
            let mut active = self.active_jobs.lock().await;
            active.insert(
                job_name.to_string(),
                (launcher_node.clone(), launcher_job_id),
            );
        }
        {
            let mut nodes = self.job_nodes.lock().await;
            nodes.insert(job_name.to_string(), placement.nodes.clone());
        }

        Ok(LaunchResult {
            resource_ids,
            message: format!(
                "submitted job to {} Reaper agents; launcher on {}",
                placement.nodes.len(),
                launcher_node
            ),
        })
    }

    async fn status(
        &self,
        job_name: &str,
        _namespace: &str,
    ) -> Result<BackendJobStatus, BuboError> {
        let (launcher_node, job_id) = {
            let active = self.active_jobs.lock().await;
            match active.get(job_name) {
                Some(entry) => entry.clone(),
                None => return Ok(BackendJobStatus::NotFound),
            }
        };

        let status = self.poll_agent_status(&launcher_node, &job_id).await?;

        let backend_status = map_reaper_state(&status);
        debug!(
            job = job_name,
            reaper_state = ?status.status,
            "mapped Reaper status"
        );
        Ok(backend_status)
    }

    async fn terminate(
        &self,
        job_name: &str,
        _namespace: &str,
    ) -> Result<(), BuboError> {
        let nodes = {
            let nodes_map = self.job_nodes.lock().await;
            nodes_map.get(job_name).cloned().unwrap_or_default()
        };

        if nodes.is_empty() {
            debug!(job = job_name, "no Reaper nodes tracked, nothing to terminate");
            return Ok(());
        }

        // We don't persist per-node job IDs — Reaper agents accept the same
        // bubo job_name as a correlation key via DELETE /api/v1/jobs/{job_name}.
        // The launcher's job_id is reused on all agents because each agent was
        // given the same script; workers can be identified by the same job_name.
        let job_id = {
            let active = self.active_jobs.lock().await;
            active
                .get(job_name)
                .map(|(_, id)| id.clone())
                .unwrap_or_else(|| job_name.to_string())
        };

        for node in &nodes {
            self.terminate_on_agent(node, &job_id).await;
        }

        info!(job = job_name, nodes = nodes.len(), "sent terminate to all Reaper agents");
        Ok(())
    }

    async fn cleanup(
        &self,
        job_name: &str,
        namespace: &str,
    ) -> Result<(), BuboError> {
        // Bare-metal jobs have no extra Kubernetes resources to remove.
        // Terminate the processes and clear our local state.
        self.terminate(job_name, namespace).await?;

        {
            let mut active = self.active_jobs.lock().await;
            active.remove(job_name);
        }
        {
            let mut nodes = self.job_nodes.lock().await;
            nodes.remove(job_name);
        }

        debug!(job = job_name, "cleaned up Reaper job state");
        Ok(())
    }
}

/// Map a [`ReaperJobStatus`] to the generic [`BackendJobStatus`] understood by
/// the controller's reconciliation loop.
fn map_reaper_state(status: &ReaperJobStatus) -> BackendJobStatus {
    match status.status {
        ReaperJobState::Pending => BackendJobStatus::Launching { ready: 0, total: 1 },
        ReaperJobState::Running => BackendJobStatus::Running,
        ReaperJobState::Succeeded => BackendJobStatus::Succeeded,
        ReaperJobState::Failed => BackendJobStatus::Failed {
            message: status
                .message
                .clone()
                .unwrap_or_else(|| {
                    format!(
                        "job exited with code {}",
                        status.exit_code.unwrap_or(-1)
                    )
                }),
        },
        ReaperJobState::Unknown => BackendJobStatus::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubo_core::{ExecutionBackendType, BuboJobSpec};

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn make_placement(nodes: &[&str]) -> Placement {
        Placement {
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            score: 1.0,
        }
    }

    fn make_reaper_spec(nodes: u32, tasks_per_node: u32) -> BuboJobSpec {
        use bubo_core::crd::ReaperSpec;
        let mut env = HashMap::new();
        env.insert("SCRATCH".to_string(), "/scratch/project".to_string());

        BuboJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes,
            tasks_per_node,
            backend: ExecutionBackendType::Reaper,
            container: None,
            reaper: Some(ReaperSpec {
                script: "#!/bin/bash\nmpirun ./app".to_string(),
                environment: env,
                working_dir: Some("/scratch/project".to_string()),
            }),
            mpi: None,
            topology: None,
            dependencies: vec![],
        }
    }

    // -------------------------------------------------------------------------
    // Construction & configuration
    // -------------------------------------------------------------------------

    #[test]
    fn test_default_construction() {
        let backend = ReaperBackend::new();
        assert_eq!(backend.agent_port, DEFAULT_REAPER_AGENT_PORT);
        assert_eq!(backend.agent_scheme, "http");
    }

    #[test]
    fn test_construction_reads_env_vars() {
        // Use a sub-process approach isn't practical here; instead verify the
        // fallback path produces the expected defaults when env vars are absent.
        std::env::remove_var("REAPER_AGENT_PORT");
        std::env::remove_var("REAPER_AGENT_SCHEME");
        let backend = ReaperBackend::new();
        assert_eq!(backend.agent_port, 8443);
        assert_eq!(backend.agent_scheme, "http");
    }

    // -------------------------------------------------------------------------
    // Agent URL generation
    // -------------------------------------------------------------------------

    #[test]
    fn test_agent_url_default_scheme_and_port() {
        let backend = ReaperBackend::new();
        assert_eq!(
            backend.agent_url("node-42"),
            "http://node-42:8443"
        );
    }

    #[test]
    fn test_agent_url_custom_port() {
        let mut backend = ReaperBackend::new();
        backend.agent_port = 9000;
        assert_eq!(backend.agent_url("worker-1"), "http://worker-1:9000");
    }

    #[test]
    fn test_agent_url_https_scheme() {
        let mut backend = ReaperBackend::new();
        backend.agent_scheme = "https".to_string();
        assert_eq!(backend.agent_url("gpu-node"), "https://gpu-node:8443");
    }

    #[test]
    fn test_agent_url_preserves_fqdn() {
        let backend = ReaperBackend::new();
        assert_eq!(
            backend.agent_url("node-0.cluster.internal"),
            "http://node-0.cluster.internal:8443"
        );
    }

    // -------------------------------------------------------------------------
    // Request / response payload serialization
    // -------------------------------------------------------------------------

    #[test]
    fn test_reaper_job_request_serialization_roundtrip() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        env.insert("RANKS".to_string(), "8".to_string());

        let req = ReaperJobRequest {
            script: "#!/bin/bash\necho hello".to_string(),
            environment: env.clone(),
            working_dir: Some("/tmp/work".to_string()),
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: ReaperJobRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.script, req.script);
        assert_eq!(parsed.environment, env);
        assert_eq!(parsed.working_dir, Some("/tmp/work".to_string()));
    }

    #[test]
    fn test_reaper_job_request_omits_none_working_dir() {
        let req = ReaperJobRequest {
            script: "echo ok".to_string(),
            environment: HashMap::new(),
            working_dir: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(!json.contains("working_dir"), "None fields should be omitted");
    }

    #[test]
    fn test_reaper_job_response_roundtrip() {
        let resp = ReaperJobResponse {
            job_id: "abc-123".to_string(),
            status: ReaperJobState::Running,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: ReaperJobResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.job_id, "abc-123");
        assert_eq!(parsed.status, ReaperJobState::Running);
    }

    #[test]
    fn test_reaper_job_state_deserialization() {
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""pending""#).unwrap(),
            ReaperJobState::Pending
        );
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""running""#).unwrap(),
            ReaperJobState::Running
        );
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""succeeded""#).unwrap(),
            ReaperJobState::Succeeded
        );
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""failed""#).unwrap(),
            ReaperJobState::Failed
        );
        assert_eq!(
            serde_json::from_str::<ReaperJobState>(r#""unknown""#).unwrap(),
            ReaperJobState::Unknown
        );
    }

    #[test]
    fn test_reaper_job_status_with_exit_code() {
        let json = r#"{
            "job_id": "xyz",
            "status": "failed",
            "exit_code": 1,
            "message": "OOM"
        }"#;
        let status: ReaperJobStatus = serde_json::from_str(json).expect("deserialize");
        assert_eq!(status.job_id, "xyz");
        assert_eq!(status.status, ReaperJobState::Failed);
        assert_eq!(status.exit_code, Some(1));
        assert_eq!(status.message.as_deref(), Some("OOM"));
    }

    // -------------------------------------------------------------------------
    // Status mapping
    // -------------------------------------------------------------------------

    #[test]
    fn test_map_pending_to_launching() {
        let status = ReaperJobStatus {
            job_id: "j1".to_string(),
            status: ReaperJobState::Pending,
            exit_code: None,
            message: None,
        };
        assert_eq!(
            map_reaper_state(&status),
            BackendJobStatus::Launching { ready: 0, total: 1 }
        );
    }

    #[test]
    fn test_map_running() {
        let status = ReaperJobStatus {
            job_id: "j1".to_string(),
            status: ReaperJobState::Running,
            exit_code: None,
            message: None,
        };
        assert_eq!(map_reaper_state(&status), BackendJobStatus::Running);
    }

    #[test]
    fn test_map_succeeded() {
        let status = ReaperJobStatus {
            job_id: "j1".to_string(),
            status: ReaperJobState::Succeeded,
            exit_code: Some(0),
            message: None,
        };
        assert_eq!(map_reaper_state(&status), BackendJobStatus::Succeeded);
    }

    #[test]
    fn test_map_failed_with_message() {
        let status = ReaperJobStatus {
            job_id: "j1".to_string(),
            status: ReaperJobState::Failed,
            exit_code: Some(2),
            message: Some("segfault".to_string()),
        };
        assert_eq!(
            map_reaper_state(&status),
            BackendJobStatus::Failed {
                message: "segfault".to_string()
            }
        );
    }

    #[test]
    fn test_map_failed_without_message_uses_exit_code() {
        let status = ReaperJobStatus {
            job_id: "j1".to_string(),
            status: ReaperJobState::Failed,
            exit_code: Some(137),
            message: None,
        };
        match map_reaper_state(&status) {
            BackendJobStatus::Failed { message } => {
                assert!(message.contains("137"), "exit code should appear in message: {message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn test_map_failed_without_message_or_exit_code() {
        let status = ReaperJobStatus {
            job_id: "j1".to_string(),
            status: ReaperJobState::Failed,
            exit_code: None,
            message: None,
        };
        match map_reaper_state(&status) {
            BackendJobStatus::Failed { message } => {
                assert!(message.contains("-1"), "should fall back to -1: {message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn test_map_unknown_to_not_found() {
        let status = ReaperJobStatus {
            job_id: "j1".to_string(),
            status: ReaperJobState::Unknown,
            exit_code: None,
            message: None,
        };
        assert_eq!(map_reaper_state(&status), BackendJobStatus::NotFound);
    }

    // -------------------------------------------------------------------------
    // Job request building
    // -------------------------------------------------------------------------

    #[test]
    fn test_build_job_request_injects_mpi_env() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(4, 2);
        let placement = make_placement(&["n0", "n1", "n2", "n3"]);

        let req = backend.build_job_request(&spec, &placement, 0).unwrap();

        // MPI env vars injected
        assert_eq!(req.environment["BUBO_MPI_RANK"], "0");
        assert_eq!(req.environment["BUBO_TOTAL_RANKS"], "8"); // 4 * 2
        assert_eq!(req.environment["BUBO_NUM_NODES"], "4");
        assert!(req.environment.contains_key("BUBO_HOSTFILE"));
        // User-provided env preserved
        assert_eq!(req.environment["SCRATCH"], "/scratch/project");
    }

    #[test]
    fn test_build_job_request_rank_varies() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(2, 4);
        let placement = make_placement(&["n0", "n1"]);

        let req0 = backend.build_job_request(&spec, &placement, 0).unwrap();
        let req1 = backend.build_job_request(&spec, &placement, 1).unwrap();

        assert_eq!(req0.environment["BUBO_MPI_RANK"], "0");
        assert_eq!(req1.environment["BUBO_MPI_RANK"], "1");
        // Hostfile and total ranks are the same for all ranks
        assert_eq!(
            req0.environment["BUBO_HOSTFILE"],
            req1.environment["BUBO_HOSTFILE"]
        );
    }

    #[test]
    fn test_build_job_request_working_dir_forwarded() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = backend.build_job_request(&spec, &placement, 0).unwrap();
        assert_eq!(req.working_dir, Some("/scratch/project".to_string()));
    }

    #[test]
    fn test_build_job_request_error_without_reaper_spec() {
        let backend = ReaperBackend::new();
        let spec = BuboJobSpec {
            queue: "default".to_string(),
            priority: 50,
            walltime: None,
            nodes: 1,
            tasks_per_node: 1,
            backend: ExecutionBackendType::Reaper,
            container: None,
            reaper: None, // missing!
            mpi: None,
            topology: None,
            dependencies: vec![],
        };
        let placement = make_placement(&["n0"]);
        let result = backend.build_job_request(&spec, &placement, 0);
        assert!(result.is_err());
        match result.unwrap_err() {
            BuboError::ValidationError { reason } => {
                assert!(reason.contains("reaper spec"), "unexpected: {reason}");
            }
            e => panic!("expected ValidationError, got {e:?}"),
        }
    }

    #[test]
    fn test_build_job_request_hostfile_format() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(3, 4);
        let placement = make_placement(&["node-0", "node-1", "node-2"]);

        let req = backend.build_job_request(&spec, &placement, 0).unwrap();
        let hostfile = &req.environment["BUBO_HOSTFILE"];
        assert_eq!(
            hostfile,
            "node-0 slots=4\nnode-1 slots=4\nnode-2 slots=4"
        );
    }

    #[test]
    fn test_build_job_request_with_mpi_spec() {
        use bubo_core::MPISpec;

        let backend = ReaperBackend::new();
        let mut spec = make_reaper_spec(2, 1);
        spec.mpi = Some(MPISpec {
            implementation: "cray-mpich".to_string(),
            ssh_auth: false,
            fabric_interface: Some("hsn0".to_string()),
        });
        let placement = make_placement(&["node-0", "node-1"]);

        let req = backend.build_job_request(&spec, &placement, 0).unwrap();
        assert_eq!(req.environment["BUBO_MPI_IMPL"], "cray-mpich");
        assert_eq!(req.environment["BUBO_FABRIC_INTERFACE"], "hsn0");
    }
}
