use kube::Api;
use tracing::{debug, warn};
use wren_core::{UserIdentity, WrenError, WrenUser};

/// Resolves user identity from the `wren.giar.dev/user` annotation by looking up
/// the corresponding WrenUser CRD. Returns None if no annotation is present
/// or the WrenUser doesn't exist (graceful degradation).
pub async fn resolve_user_identity(
    client: &kube::Client,
    annotations: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<Option<UserIdentity>, WrenError> {
    let username = match extract_username(annotations) {
        Some(u) => u,
        None => return Ok(None),
    };

    let api: Api<WrenUser> = Api::all(client.clone());
    match api.get_opt(&username).await {
        Ok(Some(wren_user)) => {
            // Reject uid=0 (root) — jobs must never run as root
            if wren_user.spec.uid == 0 {
                warn!(user = %username, "WrenUser has uid=0 (root) — refusing identity");
                return Ok(None);
            }
            debug!(user = %username, uid = wren_user.spec.uid, "resolved user identity");
            Ok(Some(UserIdentity {
                username,
                uid: wren_user.spec.uid,
                gid: wren_user.spec.gid,
                supplemental_groups: wren_user.spec.supplemental_groups.clone(),
                home_dir: wren_user.spec.home_dir.clone(),
                default_project: wren_user.spec.default_project.clone(),
            }))
        }
        Ok(None) => {
            warn!(user = %username, "WrenUser not found — job will run without identity");
            Ok(None)
        }
        Err(e) => {
            warn!(user = %username, error = %e, "failed to look up WrenUser");
            Err(WrenError::KubeError(e))
        }
    }
}

/// Extract the username from annotations (testable without K8s client).
pub(crate) fn extract_username(
    annotations: Option<&std::collections::BTreeMap<String, String>>,
) -> Option<String> {
    annotations
        .and_then(|a| a.get("wren.giar.dev/user"))
        .filter(|u| !u.is_empty())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_extract_username_present() {
        let mut annotations = BTreeMap::new();
        annotations.insert("wren.giar.dev/user".to_string(), "miguel".to_string());
        assert_eq!(
            extract_username(Some(&annotations)),
            Some("miguel".to_string())
        );
    }

    #[test]
    fn test_extract_username_empty() {
        let mut annotations = BTreeMap::new();
        annotations.insert("wren.giar.dev/user".to_string(), "".to_string());
        assert_eq!(extract_username(Some(&annotations)), None);
    }

    #[test]
    fn test_extract_username_missing_key() {
        let annotations = BTreeMap::new();
        assert_eq!(extract_username(Some(&annotations)), None);
    }

    #[test]
    fn test_extract_username_no_annotations() {
        assert_eq!(extract_username(None), None);
    }

    #[test]
    fn test_user_identity_fields() {
        let id = UserIdentity {
            username: "miguel".to_string(),
            uid: 1001,
            gid: 1001,
            supplemental_groups: vec![1001, 5000],
            home_dir: Some("/home/miguel".to_string()),
            default_project: Some("climate-sim".to_string()),
        };
        assert_eq!(id.username, "miguel");
        assert_eq!(id.uid, 1001);
        assert_eq!(id.gid, 1001);
        assert_eq!(id.supplemental_groups, vec![1001, 5000]);
        assert_eq!(id.home_dir.as_deref(), Some("/home/miguel"));
        assert_eq!(id.default_project.as_deref(), Some("climate-sim"));
    }
}
