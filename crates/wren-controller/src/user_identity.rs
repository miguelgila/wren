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

    // -------------------------------------------------------------------------
    // Mock K8s client helpers
    // -------------------------------------------------------------------------

    /// Build a `kube::Client` whose only response is the given status code + body.
    fn make_client_with_response(status: u16, body: &str) -> kube::Client {
        use bytes::Bytes;
        use http_body_util::Full;
        use tower::service_fn;

        let body = Bytes::from(body.to_owned());
        let svc = service_fn(move |_req: http::Request<_>| {
            let body = body.clone();
            async move {
                let resp = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Full::from(body))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        kube::Client::new(svc, "default")
    }

    fn make_annotations(user: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("wren.giar.dev/user".to_string(), user.to_string());
        m
    }

    fn wren_user_json(name: &str, uid: u32, gid: u32) -> String {
        serde_json::json!({
            "apiVersion": "wren.giar.dev/v1alpha1",
            "kind": "WrenUser",
            "metadata": { "name": name },
            "spec": {
                "uid": uid,
                "gid": gid,
                "supplementalGroups": [2000, 3000],
                "homeDir": format!("/home/{}", name),
                "defaultProject": "test-project"
            }
        })
        .to_string()
    }

    // -------------------------------------------------------------------------
    // resolve_user_identity async tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_resolve_user_identity_no_annotations() {
        let client = make_client_with_response(200, "{}");
        let result = resolve_user_identity(&client, None).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_resolve_user_identity_no_user_annotation() {
        let client = make_client_with_response(200, "{}");
        let annotations = BTreeMap::new(); // no wren.giar.dev/user key
        let result = resolve_user_identity(&client, Some(&annotations))
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_resolve_user_identity_user_found() {
        let body = wren_user_json("alice", 1001, 1001);
        let client = make_client_with_response(200, &body);
        let annotations = make_annotations("alice");
        let result = resolve_user_identity(&client, Some(&annotations))
            .await
            .unwrap();
        let identity = result.expect("expected Some(UserIdentity)");
        assert_eq!(identity.username, "alice");
        assert_eq!(identity.uid, 1001);
        assert_eq!(identity.gid, 1001);
        assert_eq!(identity.supplemental_groups, vec![2000, 3000]);
        assert_eq!(identity.home_dir.as_deref(), Some("/home/alice"));
        assert_eq!(identity.default_project.as_deref(), Some("test-project"));
    }

    #[tokio::test]
    async fn test_resolve_user_identity_user_not_found() {
        let not_found = serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": "wrenusers.wren.giar.dev \"bob\" not found",
            "reason": "NotFound",
            "code": 404
        })
        .to_string();
        let client = make_client_with_response(404, &not_found);
        let annotations = make_annotations("bob");
        let result = resolve_user_identity(&client, Some(&annotations))
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_resolve_user_identity_uid_zero_rejected() {
        let body = wren_user_json("root-user", 0, 0);
        let client = make_client_with_response(200, &body);
        let annotations = make_annotations("root-user");
        let result = resolve_user_identity(&client, Some(&annotations))
            .await
            .unwrap();
        assert!(result.is_none(), "uid=0 should be rejected");
    }

    #[tokio::test]
    async fn test_resolve_user_identity_api_error() {
        let error_body = serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": "internal server error",
            "reason": "InternalError",
            "code": 500
        })
        .to_string();
        let client = make_client_with_response(500, &error_body);
        let annotations = make_annotations("alice");
        let result = resolve_user_identity(&client, Some(&annotations)).await;
        assert!(result.is_err(), "500 should propagate as error");
    }

    // -------------------------------------------------------------------------
    // extract_username tests (existing)
    // -------------------------------------------------------------------------

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
