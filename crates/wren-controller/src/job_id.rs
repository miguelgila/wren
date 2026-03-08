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

    #[test]
    fn test_configmap_name_is_stable() {
        assert_eq!(CONFIGMAP_NAME, "wren-job-id-counter");
    }

    #[test]
    fn test_counter_key_is_stable() {
        assert_eq!(COUNTER_KEY, "next_job_id");
    }
}
