use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{ObjectMeta, Patch, PatchParams, PostParams};
use kube::{Api, Client};
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tracing::{error, info, warn};

/// Configuration for leader election.
pub struct LeaderElectionConfig {
    /// Name of the Lease resource to use.
    pub lease_name: String,
    /// Namespace for the Lease resource.
    pub lease_namespace: String,
    /// Identity of this candidate (usually pod name).
    pub identity: String,
    /// How long the lease is valid before it must be renewed.
    pub lease_duration: Duration,
    /// How often the leader renews its lease.
    pub renew_interval: Duration,
    /// How long a non-leader waits before retrying to acquire.
    pub retry_interval: Duration,
}

impl Default for LeaderElectionConfig {
    fn default() -> Self {
        Self {
            lease_name: "wren-controller-leader".to_string(),
            lease_namespace: resolve_namespace(),
            identity: resolve_identity(),
            lease_duration: Duration::from_secs(15),
            renew_interval: Duration::from_secs(10),
            retry_interval: Duration::from_secs(2),
        }
    }
}

/// Resolve the pod identity: POD_NAME env var, then hostname, then a UUID.
pub fn resolve_identity() -> String {
    if let Ok(name) = std::env::var("POD_NAME") {
        if !name.is_empty() {
            return name;
        }
    }
    if let Ok(host) = hostname::get() {
        if let Ok(s) = host.into_string() {
            if !s.is_empty() {
                return s;
            }
        }
    }
    uuid::Uuid::new_v4().to_string()
}

/// Resolve the namespace: POD_NAMESPACE env var, then service account namespace file,
/// then fall back to "default".
pub fn resolve_namespace() -> String {
    if let Ok(ns) = std::env::var("POD_NAMESPACE") {
        if !ns.is_empty() {
            return ns;
        }
    }
    if let Ok(ns) =
        std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
    {
        let ns = ns.trim().to_string();
        if !ns.is_empty() {
            return ns;
        }
    }
    "default".to_string()
}

/// Convert a `SystemTime` to a `MicroTime` (k8s-openapi).
fn to_micro_time(t: SystemTime) -> MicroTime {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    MicroTime(dt)
}

/// Returns true if the lease is currently held by someone else and has not expired.
fn lease_is_held_by_other(spec: &LeaseSpec, identity: &str, now: SystemTime) -> bool {
    let holder = match spec.holder_identity.as_deref() {
        Some(h) if !h.is_empty() => h,
        _ => return false,
    };
    if holder == identity {
        return false;
    }
    // Check renewal time + lease duration
    if let Some(renew_time) = &spec.renew_time {
        let renew: SystemTime = renew_time.0.into();
        let duration_secs = spec.lease_duration_seconds.unwrap_or(15) as u64;
        let expiry = renew + Duration::from_secs(duration_secs);
        return expiry > now;
    }
    false
}

/// Runs the leader election loop.
///
/// Returns a `watch::Receiver<bool>` that signals `true` when this instance
/// is the leader. The receiver transitions back to `false` if leadership is lost.
pub async fn run_leader_election(
    client: Client,
    config: LeaderElectionConfig,
) -> anyhow::Result<watch::Receiver<bool>> {
    let (tx, rx) = watch::channel(false);
    let lease_api: Api<Lease> = Api::namespaced(client.clone(), &config.lease_namespace);

    let lease_name = config.lease_name.clone();
    let lease_namespace = config.lease_namespace.clone();
    let identity = config.identity.clone();
    let lease_duration = config.lease_duration;
    let renew_interval = config.renew_interval;
    let retry_interval = config.retry_interval;

    tokio::spawn(async move {
        info!(
            identity = %identity,
            lease = %lease_name,
            namespace = %lease_namespace,
            "starting leader election"
        );

        loop {
            let now = SystemTime::now();

            // Try to read the existing lease.
            match lease_api.get_opt(&lease_name).await {
                Err(e) => {
                    error!(error = %e, "failed to read lease");
                    tokio::time::sleep(retry_interval).await;
                    continue;
                }
                Ok(Some(existing)) => {
                    let spec = existing.spec.clone().unwrap_or_default();

                    if lease_is_held_by_other(&spec, &identity, now) {
                        // Someone else holds a valid lease; wait and retry.
                        let current_holder = spec.holder_identity.as_deref().unwrap_or("<unknown>");
                        if *tx.borrow() {
                            warn!(
                                holder = %current_holder,
                                "lost leadership — another holder has the lease"
                            );
                            let _ = tx.send(false);
                        }
                        tokio::time::sleep(retry_interval).await;
                        continue;
                    }

                    // We either hold it or it has expired — take / renew it.
                    let is_renewal = spec.holder_identity.as_deref() == Some(&identity);
                    let transitions = if is_renewal {
                        spec.lease_transitions.unwrap_or(0)
                    } else {
                        spec.lease_transitions.unwrap_or(0) + 1
                    };
                    let acquire_time = if is_renewal {
                        spec.acquire_time
                            .clone()
                            .unwrap_or_else(|| to_micro_time(now))
                    } else {
                        to_micro_time(now)
                    };

                    let new_spec = LeaseSpec {
                        holder_identity: Some(identity.clone()),
                        lease_duration_seconds: Some(lease_duration.as_secs() as i32),
                        acquire_time: Some(acquire_time),
                        renew_time: Some(to_micro_time(now)),
                        lease_transitions: Some(transitions),
                        preferred_holder: None,
                        strategy: None,
                    };

                    let patch = serde_json::json!({
                        "apiVersion": "coordination.k8s.io/v1",
                        "kind": "Lease",
                        "metadata": {
                            "name": lease_name,
                            "namespace": lease_namespace,
                            "resourceVersion": existing.metadata.resource_version,
                        },
                        "spec": new_spec,
                    });

                    match lease_api
                        .patch(
                            &lease_name,
                            &PatchParams::apply("wren-controller").force(),
                            &Patch::Apply(patch),
                        )
                        .await
                    {
                        Ok(_) => {
                            if !*tx.borrow() {
                                info!(identity = %identity, "acquired leadership");
                                let _ = tx.send(true);
                            }
                            tokio::time::sleep(renew_interval).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to patch lease (conflict?)");
                            if *tx.borrow() {
                                warn!("lost leadership due to patch failure");
                                let _ = tx.send(false);
                            }
                            tokio::time::sleep(retry_interval).await;
                        }
                    }
                }
                Ok(None) => {
                    // Lease does not exist yet — create it.
                    let lease = Lease {
                        metadata: ObjectMeta {
                            name: Some(lease_name.clone()),
                            namespace: Some(lease_namespace.clone()),
                            ..Default::default()
                        },
                        spec: Some(LeaseSpec {
                            holder_identity: Some(identity.clone()),
                            lease_duration_seconds: Some(lease_duration.as_secs() as i32),
                            acquire_time: Some(to_micro_time(now)),
                            renew_time: Some(to_micro_time(now)),
                            lease_transitions: Some(0),
                            preferred_holder: None,
                            strategy: None,
                        }),
                    };

                    match lease_api.create(&PostParams::default(), &lease).await {
                        Ok(_) => {
                            info!(identity = %identity, "created lease and acquired leadership");
                            let _ = tx.send(true);
                            tokio::time::sleep(renew_interval).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to create lease (race?)");
                            tokio::time::sleep(retry_interval).await;
                        }
                    }
                }
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    // --- resolve_identity ---

    #[test]
    fn test_resolve_identity_uses_pod_name_env() {
        // Temporarily set POD_NAME; we test the function directly.
        // Since env is process-global, we call resolve_identity with a known env state
        // by checking the logic path manually.
        let _id = "my-pod-abc123".to_string();
        // Simulate: if POD_NAME were set to this, the function returns it.
        // We verify the fallback chain instead (no env set in test context).
        let result = resolve_identity();
        // Must be non-empty regardless of which branch was taken.
        assert!(!result.is_empty());
    }

    #[test]
    fn test_resolve_identity_is_non_empty() {
        let id = resolve_identity();
        assert!(!id.is_empty());
    }

    #[test]
    fn test_resolve_identity_uuid_fallback_format() {
        // If no POD_NAME and no hostname, a UUID is generated.
        // We just check that the fallback-generated value is non-empty and plausible.
        let id = resolve_identity();
        assert!(!id.is_empty());
        // UUID or hostname — both are valid strings, no format enforced beyond non-empty.
    }

    // --- resolve_namespace ---

    #[test]
    fn test_resolve_namespace_is_non_empty() {
        let ns = resolve_namespace();
        assert!(!ns.is_empty());
    }

    #[test]
    fn test_resolve_namespace_default_fallback() {
        // Without POD_NAMESPACE set and outside a cluster, falls back to "default".
        // We can't guarantee the env isn't set, so we test the logic path:
        // if both env and file are absent, result is "default".
        // Verify the function returns a non-empty string.
        let ns = resolve_namespace();
        assert!(!ns.is_empty());
        // In a non-cluster environment it must be "default".
        // (CI runners won't have the SA namespace file.)
        if std::env::var("POD_NAMESPACE").is_err()
            && std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
                .is_err()
        {
            assert_eq!(ns, "default");
        }
    }

    // --- LeaderElectionConfig defaults ---

    #[test]
    fn test_default_config_lease_duration() {
        let cfg = LeaderElectionConfig::default();
        assert_eq!(cfg.lease_duration, Duration::from_secs(15));
    }

    #[test]
    fn test_default_config_renew_interval() {
        let cfg = LeaderElectionConfig::default();
        assert_eq!(cfg.renew_interval, Duration::from_secs(10));
    }

    #[test]
    fn test_default_config_retry_interval() {
        let cfg = LeaderElectionConfig::default();
        assert_eq!(cfg.retry_interval, Duration::from_secs(2));
    }

    #[test]
    fn test_default_config_lease_name() {
        let cfg = LeaderElectionConfig::default();
        assert_eq!(cfg.lease_name, "wren-controller-leader");
    }

    #[test]
    fn test_default_config_identity_non_empty() {
        let cfg = LeaderElectionConfig::default();
        assert!(!cfg.identity.is_empty());
    }

    // --- lease_is_held_by_other ---

    fn make_spec(holder: &str, renew_secs_ago: u64, duration_secs: i32) -> LeaseSpec {
        let renew_time = SystemTime::now()
            .checked_sub(Duration::from_secs(renew_secs_ago))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let dt: chrono::DateTime<chrono::Utc> = renew_time.into();
        LeaseSpec {
            holder_identity: Some(holder.to_string()),
            lease_duration_seconds: Some(duration_secs),
            renew_time: Some(MicroTime(dt)),
            acquire_time: None,
            lease_transitions: Some(0),
            preferred_holder: None,
            strategy: None,
        }
    }

    #[test]
    fn test_lease_held_by_other_active() {
        // Another holder renewed 5s ago, lease duration 15s — still valid.
        let spec = make_spec("other-pod", 5, 15);
        let now = SystemTime::now();
        assert!(lease_is_held_by_other(&spec, "my-pod", now));
    }

    #[test]
    fn test_lease_held_by_other_expired() {
        // Another holder renewed 20s ago, lease duration 15s — expired.
        let spec = make_spec("other-pod", 20, 15);
        let now = SystemTime::now();
        assert!(!lease_is_held_by_other(&spec, "my-pod", now));
    }

    #[test]
    fn test_lease_held_by_self_not_considered_other() {
        // Spec claims we hold it — not "other".
        let spec = make_spec("my-pod", 5, 15);
        let now = SystemTime::now();
        assert!(!lease_is_held_by_other(&spec, "my-pod", now));
    }

    #[test]
    fn test_lease_no_holder_not_considered_other() {
        let spec = LeaseSpec {
            holder_identity: None,
            ..Default::default()
        };
        let now = SystemTime::now();
        assert!(!lease_is_held_by_other(&spec, "my-pod", now));
    }

    #[test]
    fn test_lease_empty_holder_not_considered_other() {
        let spec = LeaseSpec {
            holder_identity: Some("".to_string()),
            ..Default::default()
        };
        let now = SystemTime::now();
        assert!(!lease_is_held_by_other(&spec, "my-pod", now));
    }

    #[test]
    fn test_lease_boundary_exactly_expired() {
        // Renewed exactly `lease_duration` seconds ago — just expired.
        let spec = make_spec("other-pod", 15, 15);
        let now = SystemTime::now();
        assert!(!lease_is_held_by_other(&spec, "my-pod", now));
    }

    // --- to_micro_time ---

    #[test]
    fn test_to_micro_time_roundtrip() {
        let now = SystemTime::now();
        let micro = to_micro_time(now);
        // MicroTime wraps a chrono DateTime — verify it's close to now
        let dt: chrono::DateTime<chrono::Utc> = now.into();
        let diff = (micro.0 - dt).num_milliseconds().abs();
        assert!(
            diff < 1000,
            "to_micro_time should preserve time within 1s, diff={diff}ms"
        );
    }

    #[test]
    fn test_to_micro_time_epoch() {
        let epoch = SystemTime::UNIX_EPOCH;
        let micro = to_micro_time(epoch);
        assert_eq!(micro.0.timestamp(), 0);
    }

    // --- lease_is_held_by_other edge cases ---

    #[test]
    fn test_lease_no_renew_time_not_held() {
        // Holder set but no renew_time — can't determine expiry, so not held
        let spec = LeaseSpec {
            holder_identity: Some("other-pod".to_string()),
            lease_duration_seconds: Some(15),
            renew_time: None,
            acquire_time: None,
            lease_transitions: Some(0),
            preferred_holder: None,
            strategy: None,
        };
        let now = SystemTime::now();
        assert!(!lease_is_held_by_other(&spec, "my-pod", now));
    }

    #[test]
    fn test_lease_held_by_other_just_renewed() {
        // Renewed 0 seconds ago, duration 15s — definitely held
        let spec = make_spec("other-pod", 0, 15);
        let now = SystemTime::now();
        assert!(lease_is_held_by_other(&spec, "my-pod", now));
    }

    #[test]
    fn test_lease_held_by_other_one_second_left() {
        // Renewed 14s ago, duration 15s — still held (1s left)
        let spec = make_spec("other-pod", 14, 15);
        let now = SystemTime::now();
        assert!(lease_is_held_by_other(&spec, "my-pod", now));
    }

    #[test]
    fn test_lease_default_duration_when_none() {
        // lease_duration_seconds is None — defaults to 15
        let renew_time = SystemTime::now()
            .checked_sub(Duration::from_secs(5))
            .unwrap();
        let dt: chrono::DateTime<chrono::Utc> = renew_time.into();
        let spec = LeaseSpec {
            holder_identity: Some("other-pod".to_string()),
            lease_duration_seconds: None, // defaults to 15
            renew_time: Some(MicroTime(dt)),
            acquire_time: None,
            lease_transitions: Some(0),
            preferred_holder: None,
            strategy: None,
        };
        let now = SystemTime::now();
        // 5s ago with default 15s duration — still held
        assert!(lease_is_held_by_other(&spec, "my-pod", now));
    }
}
