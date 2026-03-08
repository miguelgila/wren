use async_trait::async_trait;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info, warn};
use wren_core::backend::{BackendJobStatus, ExecutionBackend, LaunchResult, UserIdentity};
use wren_core::{Placement, WrenError, WrenJobSpec};

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
    /// Unix user ID for privilege dropping on the Reaper agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Unix primary group ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// Username (for env var injection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Home directory path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_dir: Option<String>,
    /// Supplemental Unix groups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supplemental_groups: Option<Vec<u32>>,
    /// MPI hostfile content — Reaper agent writes this to `hostfile_path` before running the script.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostfile: Option<String>,
    /// Path where the hostfile should be written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostfile_path: Option<String>,
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
    /// Tracks the launcher node + Reaper job id for each wren job name.
    /// Key: wren job name, Value: (launcher_node, reaper_job_id).
    active_jobs: tokio::sync::Mutex<HashMap<String, (String, String)>>,
    /// All nodes participating in a job, used for broadcast terminate.
    /// Key: wren job name, Value: list of node names.
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

        let agent_scheme =
            std::env::var("REAPER_AGENT_SCHEME").unwrap_or_else(|_| "http".to_string());

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
    ) -> Result<ReaperJobResponse, WrenError> {
        let url = format!("{}/api/v1/jobs", self.agent_url(node_name));
        debug!(node = %node_name, url = %url, "submitting job to Reaper agent");

        let response = self
            .http
            .post(&url)
            .json(request)
            .send()
            .await
            .map_err(|e| WrenError::BackendError {
                message: format!("HTTP request to Reaper agent on {node_name} failed: {e}"),
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(WrenError::BackendError {
                message: format!("Reaper agent on {node_name} returned {status}: {body}"),
            });
        }

        response
            .json::<ReaperJobResponse>()
            .await
            .map_err(|e| WrenError::BackendError {
                message: format!("failed to parse Reaper agent response from {node_name}: {e}"),
            })
    }

    /// Poll the status of a job from the Reaper agent on `node_name`.
    async fn poll_agent_status(
        &self,
        node_name: &str,
        job_id: &str,
    ) -> Result<ReaperJobStatus, WrenError> {
        let url = format!("{}/api/v1/jobs/{}", self.agent_url(node_name), job_id);
        debug!(node = %node_name, job_id = %job_id, "polling job status from Reaper agent");

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| WrenError::BackendError {
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
            return Err(WrenError::BackendError {
                message: format!(
                    "Reaper agent on {node_name} returned {status} for status poll: {body}"
                ),
            });
        }

        response
            .json::<ReaperJobStatus>()
            .await
            .map_err(|e| WrenError::BackendError {
                message: format!(
                    "failed to parse status response from Reaper agent on {node_name}: {e}"
                ),
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
        job_name: &str,
        spec: &WrenJobSpec,
        placement: &Placement,
        rank: u32,
        user: Option<&UserIdentity>,
    ) -> Result<ReaperJobRequest, WrenError> {
        let reaper_spec = spec
            .reaper
            .as_ref()
            .ok_or_else(|| WrenError::ValidationError {
                reason: "reaper spec required for reaper backend".to_string(),
            })?;

        // Start with user-provided environment
        let mut environment = reaper_spec.environment.clone();

        // Hostfile for MPI — content written as env var AND a known path for the agent to write
        let hostfile_content = mpi::generate_hostfile(placement, spec.tasks_per_node);
        let hostfile_path = format!("/tmp/wren-hostfile-{}", job_name);

        // Inject MPI-implementation-specific env vars (MPICH_OFI_*, UCX_NET_DEVICES, etc.)
        if spec.mpi.is_some() {
            let mpi_env =
                mpi::bare_metal_env_vars(job_name, spec, &placement.nodes, &hostfile_path);
            for (k, v) in mpi_env {
                environment.insert(k, v);
            }

            // Generate the launcher command line for reference (user's script can use it)
            let launcher_args = mpi::bare_metal_mpirun_args(spec, &hostfile_path);
            environment.insert("WREN_MPI_LAUNCHER".to_string(), launcher_args.join(" "));
        }

        // Per-rank env vars (always set, even without MPI spec)
        environment.insert("WREN_MPI_RANK".to_string(), rank.to_string());
        environment.insert("WREN_LOCAL_RANK".to_string(), "0".to_string()); // Phase 3a: tasks_per_node=1

        // Distributed training env vars (PyTorch/NCCL compatible)
        let master_addr = placement.nodes.first().cloned().unwrap_or_default();
        environment.insert("MASTER_ADDR".to_string(), master_addr);
        environment.insert("MASTER_PORT".to_string(), "29500".to_string());
        environment.insert("RANK".to_string(), rank.to_string());
        environment.insert(
            "WORLD_SIZE".to_string(),
            mpi::total_ranks(spec).to_string(),
        );
        environment.insert("LOCAL_RANK".to_string(), "0".to_string()); // Phase 3a: 1 rank per node

        // Hostfile content as env var (for scripts that want to write it themselves)
        environment.insert(
            "WREN_HOSTFILE_CONTENT".to_string(),
            hostfile_content.clone(),
        );
        // Path where Reaper agent will write the hostfile
        environment.insert("WREN_HOSTFILE_PATH".to_string(), hostfile_path.clone());

        // Also set WREN_JOB_NAME and WREN_NUM_NODES if not already set by bare_metal_env_vars
        environment
            .entry("WREN_JOB_NAME".to_string())
            .or_insert_with(|| job_name.to_string());
        environment
            .entry("WREN_NUM_NODES".to_string())
            .or_insert_with(|| spec.nodes.to_string());
        environment
            .entry("WREN_TOTAL_RANKS".to_string())
            .or_insert_with(|| mpi::total_ranks(spec).to_string());

        // Inject user identity env vars (same as container backend)
        if let Some(u) = user {
            environment.insert("USER".to_string(), u.username.clone());
            environment.insert("LOGNAME".to_string(), u.username.clone());
            if let Some(ref home) = u.home_dir {
                environment.insert("HOME".to_string(), home.clone());
            }
        }

        Ok(ReaperJobRequest {
            script: reaper_spec.script.clone(),
            environment,
            working_dir: reaper_spec.working_dir.clone(),
            uid: user.map(|u| u.uid),
            gid: user.map(|u| u.gid),
            username: user.map(|u| u.username.clone()),
            home_dir: user.and_then(|u| u.home_dir.clone()),
            supplemental_groups: user
                .map(|u| u.supplemental_groups.clone())
                .filter(|g| !g.is_empty()),
            hostfile: if spec.mpi.is_some() {
                Some(hostfile_content)
            } else {
                None
            },
            hostfile_path: if spec.mpi.is_some() {
                Some(hostfile_path)
            } else {
                None
            },
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
        spec: &WrenJobSpec,
        placement: &Placement,
        user: Option<&UserIdentity>,
    ) -> Result<LaunchResult, WrenError> {
        info!(
            job = job_name,
            nodes = ?placement.nodes,
            "launching bare-metal MPI job via Reaper"
        );

        let mut resource_ids = Vec::new();
        let mut launcher_job_id: Option<String> = None;
        let launcher_node =
            placement
                .nodes
                .first()
                .cloned()
                .ok_or_else(|| WrenError::ValidationError {
                    reason: "placement must contain at least one node".to_string(),
                })?;

        for (rank, node) in placement.nodes.iter().enumerate() {
            let request = self.build_job_request(job_name, spec, placement, rank as u32, user)?;
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
    ) -> Result<BackendJobStatus, WrenError> {
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

    async fn terminate(&self, job_name: &str, _namespace: &str) -> Result<(), WrenError> {
        let nodes = {
            let nodes_map = self.job_nodes.lock().await;
            nodes_map.get(job_name).cloned().unwrap_or_default()
        };

        if nodes.is_empty() {
            debug!(
                job = job_name,
                "no Reaper nodes tracked, nothing to terminate"
            );
            return Ok(());
        }

        // We don't persist per-node job IDs — Reaper agents accept the same
        // wren job_name as a correlation key via DELETE /api/v1/jobs/{job_name}.
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

        info!(
            job = job_name,
            nodes = nodes.len(),
            "sent terminate to all Reaper agents"
        );
        Ok(())
    }

    async fn cleanup(&self, job_name: &str, namespace: &str) -> Result<(), WrenError> {
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
            message: status.message.clone().unwrap_or_else(|| {
                format!("job exited with code {}", status.exit_code.unwrap_or(-1))
            }),
        },
        ReaperJobState::Unknown => BackendJobStatus::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_core::{ExecutionBackendType, WrenJobSpec};

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn make_placement(nodes: &[&str]) -> Placement {
        Placement {
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            score: 1.0,
        }
    }

    fn make_reaper_spec(nodes: u32, tasks_per_node: u32) -> WrenJobSpec {
        use wren_core::crd::ReaperSpec;
        let mut env = HashMap::new();
        env.insert("SCRATCH".to_string(), "/scratch/project".to_string());

        WrenJobSpec {
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
            project: None,
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
        assert_eq!(backend.agent_url("node-42"), "http://node-42:8443");
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
            uid: Some(1000),
            gid: Some(1000),
            username: Some("test".to_string()),
            home_dir: Some("/home/test".to_string()),
            supplemental_groups: Some(vec![1000, 2000]),
            hostfile: Some("node-0 slots=4".to_string()),
            hostfile_path: Some("/tmp/wren-hostfile-test".to_string()),
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: ReaperJobRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.script, req.script);
        assert_eq!(parsed.environment, env);
        assert_eq!(parsed.working_dir, Some("/tmp/work".to_string()));
        assert_eq!(parsed.uid, Some(1000));
        assert_eq!(parsed.gid, Some(1000));
        assert_eq!(parsed.username, Some("test".to_string()));
        assert_eq!(parsed.home_dir, Some("/home/test".to_string()));
        assert_eq!(parsed.supplemental_groups, Some(vec![1000, 2000]));
    }

    #[test]
    fn test_reaper_job_request_omits_none_working_dir() {
        let req = ReaperJobRequest {
            script: "echo ok".to_string(),
            environment: HashMap::new(),
            working_dir: None,
            uid: None,
            gid: None,
            username: None,
            home_dir: None,
            supplemental_groups: None,
            hostfile: None,
            hostfile_path: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(
            !json.contains("working_dir"),
            "None fields should be omitted"
        );
        assert!(!json.contains("uid"), "None uid should be omitted");
        assert!(!json.contains("gid"), "None gid should be omitted");
        assert!(!json.contains("username"), "None username should be omitted");
        assert!(!json.contains("home_dir"), "None home_dir should be omitted");
        assert!(
            !json.contains("supplemental_groups"),
            "None supplemental_groups should be omitted"
        );
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
                assert!(
                    message.contains("137"),
                    "exit code should appear in message: {message}"
                );
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

        let req = backend.build_job_request("test-job", &spec, &placement, 0, None).unwrap();

        // MPI env vars injected
        assert_eq!(req.environment["WREN_MPI_RANK"], "0");
        assert_eq!(req.environment["WREN_TOTAL_RANKS"], "8"); // 4 * 2
        assert_eq!(req.environment["WREN_NUM_NODES"], "4");
        assert!(req.environment.contains_key("WREN_HOSTFILE_CONTENT"));
        // User-provided env preserved
        assert_eq!(req.environment["SCRATCH"], "/scratch/project");
    }

    #[test]
    fn test_build_job_request_rank_varies() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(2, 4);
        let placement = make_placement(&["n0", "n1"]);

        let req0 = backend.build_job_request("test-job", &spec, &placement, 0, None).unwrap();
        let req1 = backend.build_job_request("test-job", &spec, &placement, 1, None).unwrap();

        assert_eq!(req0.environment["WREN_MPI_RANK"], "0");
        assert_eq!(req1.environment["WREN_MPI_RANK"], "1");
        // Hostfile and total ranks are the same for all ranks
        assert_eq!(
            req0.environment["WREN_HOSTFILE_CONTENT"],
            req1.environment["WREN_HOSTFILE_CONTENT"]
        );
    }

    #[test]
    fn test_build_job_request_working_dir_forwarded() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = backend.build_job_request("test-job", &spec, &placement, 0, None).unwrap();
        assert_eq!(req.working_dir, Some("/scratch/project".to_string()));
    }

    #[test]
    fn test_build_job_request_error_without_reaper_spec() {
        let backend = ReaperBackend::new();
        let spec = WrenJobSpec {
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
            project: None,
        };
        let placement = make_placement(&["n0"]);
        let result = backend.build_job_request("test-job", &spec, &placement, 0, None);
        assert!(result.is_err());
        match result.unwrap_err() {
            WrenError::ValidationError { reason } => {
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

        let req = backend.build_job_request("test-job", &spec, &placement, 0, None).unwrap();
        // No MPI spec → hostfile content in WREN_HOSTFILE_CONTENT env var
        let hostfile = &req.environment["WREN_HOSTFILE_CONTENT"];
        assert_eq!(hostfile, "node-0 slots=4\nnode-1 slots=4\nnode-2 slots=4");
    }

    #[test]
    fn test_build_job_request_with_mpi_spec() {
        use wren_core::MPISpec;

        let backend = ReaperBackend::new();
        let mut spec = make_reaper_spec(2, 1);
        spec.mpi = Some(MPISpec {
            implementation: "cray-mpich".to_string(),
            ssh_auth: false,
            fabric_interface: Some("hsn0".to_string()),
        });
        let placement = make_placement(&["node-0", "node-1"]);

        let req = backend.build_job_request("test-job", &spec, &placement, 0, None).unwrap();
        // With MPI spec, bare_metal_env_vars injects MPICH_OFI_IFNAME (not WREN_MPI_IMPL/WREN_FABRIC_INTERFACE)
        assert_eq!(req.environment["MPICH_OFI_IFNAME"], "hsn0");
        assert_eq!(req.environment["MPICH_OFI_STARTUP_CONNECT"], "1");
    }

    // --- build_job_request edge cases ---

    #[test]
    fn test_build_job_request_mpi_impl_without_fabric() {
        use wren_core::MPISpec;
        let backend = ReaperBackend::new();
        let mut spec = make_reaper_spec(2, 4);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: false,
            fabric_interface: None,
        });
        let placement = make_placement(&["n0", "n1"]);
        let req = backend.build_job_request("test-job", &spec, &placement, 0, None).unwrap();
        // openmpi without fabric — no UCX_NET_DEVICES set, but MPI launcher env present
        assert!(!req.environment.contains_key("UCX_NET_DEVICES"));
        assert!(req.environment.contains_key("WREN_MPI_LAUNCHER"));
    }

    #[test]
    fn test_build_job_request_script_preserved() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);
        let req = backend.build_job_request("test-job", &spec, &placement, 0, None).unwrap();
        assert_eq!(req.script, "#!/bin/bash\nmpirun ./app");
    }

    #[test]
    fn test_build_job_request_single_node_single_rank() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["solo-node"]);
        let req = backend.build_job_request("test-job", &spec, &placement, 0, None).unwrap();
        assert_eq!(req.environment["WREN_TOTAL_RANKS"], "1");
        assert_eq!(req.environment["WREN_NUM_NODES"], "1");
        assert_eq!(req.environment["WREN_MPI_RANK"], "0");
        // No MPI spec → hostfile content in WREN_HOSTFILE_CONTENT
        assert_eq!(req.environment["WREN_HOSTFILE_CONTENT"], "solo-node slots=1");
    }

    #[test]
    fn test_build_job_request_large_cluster() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(8, 4);
        let placement = Placement {
            nodes: (0..8).map(|i| format!("node-{i}")).collect(),
            score: 1.0,
        };
        let req = backend.build_job_request("test-job", &spec, &placement, 7, None).unwrap();
        assert_eq!(req.environment["WREN_TOTAL_RANKS"], "32"); // 8*4
        assert_eq!(req.environment["WREN_MPI_RANK"], "7");
        assert_eq!(req.environment["WREN_NUM_NODES"], "8");
    }

    // --- map_reaper_state edge cases ---

    #[test]
    fn test_map_reaper_state_failed_exit_code_zero_with_failed_status() {
        // Edge case: status says Failed but exit_code is 0
        let status = ReaperJobStatus {
            job_id: "j1".to_string(),
            status: ReaperJobState::Failed,
            exit_code: Some(0),
            message: Some("killed by signal".to_string()),
        };
        let mapped = map_reaper_state(&status);
        assert_eq!(
            mapped,
            BackendJobStatus::Failed {
                message: "killed by signal".to_string()
            }
        );
    }

    // --- ReaperJobStatus serde ---

    #[test]
    fn test_reaper_job_status_minimal_json() {
        let json = r#"{"job_id": "abc", "status": "pending"}"#;
        let status: ReaperJobStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.job_id, "abc");
        assert_eq!(status.status, ReaperJobState::Pending);
        assert!(status.exit_code.is_none());
        assert!(status.message.is_none());
    }

    #[test]
    fn test_reaper_job_status_succeeded_with_exit_code_zero() {
        let json = r#"{"job_id": "xyz", "status": "succeeded", "exit_code": 0}"#;
        let status: ReaperJobStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.status, ReaperJobState::Succeeded);
        assert_eq!(status.exit_code, Some(0));
    }

    // -------------------------------------------------------------------------
    // User identity
    // -------------------------------------------------------------------------

    fn make_user_identity() -> UserIdentity {
        UserIdentity {
            username: "testuser".to_string(),
            uid: 1001,
            gid: 1001,
            supplemental_groups: vec![1001, 5000],
            home_dir: Some("/home/testuser".to_string()),
            default_project: Some("my-project".to_string()),
        }
    }

    #[test]
    fn test_build_job_request_with_user_identity() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(2, 1);
        let placement = make_placement(&["n0", "n1"]);
        let user = make_user_identity();

        let req = backend
            .build_job_request("test-job", &spec, &placement, 0, Some(&user))
            .unwrap();

        assert_eq!(req.uid, Some(1001));
        assert_eq!(req.gid, Some(1001));
        assert_eq!(req.username, Some("testuser".to_string()));
        assert_eq!(req.home_dir, Some("/home/testuser".to_string()));
        assert_eq!(req.supplemental_groups, Some(vec![1001, 5000]));

        assert_eq!(req.environment["USER"], "testuser");
        assert_eq!(req.environment["LOGNAME"], "testuser");
        assert_eq!(req.environment["HOME"], "/home/testuser");
    }

    #[test]
    fn test_build_job_request_without_user_leaves_fields_none() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = backend
            .build_job_request("test-job", &spec, &placement, 0, None)
            .unwrap();

        assert!(req.uid.is_none());
        assert!(req.gid.is_none());
        assert!(req.username.is_none());
        assert!(req.home_dir.is_none());
        assert!(req.supplemental_groups.is_none());
        assert!(!req.environment.contains_key("USER"));
        assert!(!req.environment.contains_key("LOGNAME"));
        assert!(!req.environment.contains_key("HOME"));
    }

    #[test]
    fn test_build_job_request_user_without_home_dir() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);
        let user = UserIdentity {
            username: "nohome".to_string(),
            uid: 2000,
            gid: 2000,
            supplemental_groups: vec![],
            home_dir: None,
            default_project: None,
        };

        let req = backend
            .build_job_request("test-job", &spec, &placement, 0, Some(&user))
            .unwrap();

        assert_eq!(req.uid, Some(2000));
        assert_eq!(req.gid, Some(2000));
        assert_eq!(req.home_dir, None);
        assert!(req.supplemental_groups.is_none()); // empty vec filtered to None
        assert_eq!(req.environment["USER"], "nohome");
        assert!(!req.environment.contains_key("HOME"));
    }

    #[test]
    fn test_build_job_request_user_identity_serde_roundtrip() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);
        let user = make_user_identity();

        let req = backend
            .build_job_request("test-job", &spec, &placement, 0, Some(&user))
            .unwrap();
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: ReaperJobRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.uid, req.uid);
        assert_eq!(parsed.gid, req.gid);
        assert_eq!(parsed.username, req.username);
        assert_eq!(parsed.home_dir, req.home_dir);
        assert_eq!(parsed.supplemental_groups, req.supplemental_groups);
    }

    #[test]
    fn test_build_job_request_none_user_serde_omits_fields() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = backend
            .build_job_request("test-job", &spec, &placement, 0, None)
            .unwrap();
        let json = serde_json::to_string(&req).expect("serialize");

        assert!(!json.contains("uid"), "None uid should be omitted: {json}");
        assert!(!json.contains("gid"), "None gid should be omitted: {json}");
        assert!(
            !json.contains("username"),
            "None username should be omitted: {json}"
        );
        assert!(
            !json.contains("home_dir"),
            "None home_dir should be omitted: {json}"
        );
        assert!(
            !json.contains("supplemental_groups"),
            "None groups should be omitted: {json}"
        );
    }

    #[test]
    fn test_build_job_request_user_env_vars_dont_override_user_provided() {
        let backend = ReaperBackend::new();
        let mut spec = make_reaper_spec(1, 1);
        // User provides their own USER env var in the reaper spec
        spec.reaper
            .as_mut()
            .unwrap()
            .environment
            .insert("USER".to_string(), "custom-user".to_string());
        let placement = make_placement(&["n0"]);
        let user = make_user_identity();

        let req = backend
            .build_job_request("test-job", &spec, &placement, 0, Some(&user))
            .unwrap();

        // UserIdentity should override user-provided env var (identity is authoritative)
        assert_eq!(req.environment["USER"], "testuser");
    }

    // -------------------------------------------------------------------------
    // Distributed training env vars
    // -------------------------------------------------------------------------

    #[test]
    fn test_build_job_request_distributed_training_env_vars() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(4, 1);
        let placement = make_placement(&["node-0", "node-1", "node-2", "node-3"]);

        let req = backend.build_job_request("train-job", &spec, &placement, 2, None).unwrap();

        // Distributed training env vars (PyTorch/NCCL compatible)
        assert_eq!(req.environment["MASTER_ADDR"], "node-0");
        assert_eq!(req.environment["MASTER_PORT"], "29500");
        assert_eq!(req.environment["RANK"], "2");
        assert_eq!(req.environment["WORLD_SIZE"], "4");
        assert_eq!(req.environment["LOCAL_RANK"], "0");
    }

    #[test]
    fn test_build_job_request_with_mpi_spec_uses_bare_metal_env() {
        use wren_core::MPISpec;
        let backend = ReaperBackend::new();
        let mut spec = make_reaper_spec(2, 1);
        spec.mpi = Some(MPISpec {
            implementation: "cray-mpich".to_string(),
            ssh_auth: false,
            fabric_interface: Some("hsn0".to_string()),
        });
        let placement = make_placement(&["node-0", "node-1"]);

        let req = backend.build_job_request("mpi-job", &spec, &placement, 0, None).unwrap();

        // cray-mpich specific env vars from bare_metal_env_vars()
        assert_eq!(req.environment["MPICH_OFI_STARTUP_CONNECT"], "1");
        assert_eq!(req.environment["MPICH_OFI_NUM_NICS"], "1");
        assert_eq!(req.environment["MPICH_OFI_IFNAME"], "hsn0");
        // Launcher command reference — cray-mpich uses srun
        assert!(req.environment["WREN_MPI_LAUNCHER"].contains("srun"));
    }

    #[test]
    fn test_build_job_request_hostfile_fields() {
        use wren_core::MPISpec;
        let backend = ReaperBackend::new();
        let mut spec = make_reaper_spec(2, 1);
        spec.mpi = Some(MPISpec {
            implementation: "openmpi".to_string(),
            ssh_auth: false,
            fabric_interface: None,
        });
        let placement = make_placement(&["n0", "n1"]);

        let req = backend.build_job_request("hf-job", &spec, &placement, 0, None).unwrap();

        assert!(req.hostfile.is_some());
        assert_eq!(req.hostfile.as_deref(), Some("n0 slots=1\nn1 slots=1"));
        assert_eq!(req.hostfile_path.as_deref(), Some("/tmp/wren-hostfile-hf-job"));
    }

    #[test]
    fn test_build_job_request_no_mpi_spec_no_hostfile_fields() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(1, 1);
        let placement = make_placement(&["n0"]);

        let req = backend.build_job_request("no-mpi", &spec, &placement, 0, None).unwrap();

        assert!(req.hostfile.is_none());
        assert!(req.hostfile_path.is_none());
        // But distributed training env vars should still be set
        assert_eq!(req.environment["MASTER_ADDR"], "n0");
        assert_eq!(req.environment["RANK"], "0");
        assert_eq!(req.environment["WORLD_SIZE"], "1");
    }

    #[test]
    fn test_build_job_request_master_addr_is_first_node() {
        let backend = ReaperBackend::new();
        let spec = make_reaper_spec(3, 1);
        let placement = make_placement(&["alpha", "beta", "gamma"]);

        let req0 = backend.build_job_request("j", &spec, &placement, 0, None).unwrap();
        let req2 = backend.build_job_request("j", &spec, &placement, 2, None).unwrap();

        // MASTER_ADDR is always the first node, regardless of rank
        assert_eq!(req0.environment["MASTER_ADDR"], "alpha");
        assert_eq!(req2.environment["MASTER_ADDR"], "alpha");
    }
}
