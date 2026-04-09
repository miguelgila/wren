# User Identity

Wren jobs must run as valid, non-root users. User identity serves two purposes: scheduling (quotas, fair-share, audit) and execution (file ownership, permissions on shared filesystems). This guide covers WrenUser CRDs, identity mapping, and multi-user policies.

## Why User Identity Matters

On HPC clusters with shared filesystems (Lustre, GPFS, NFS with AUTH_SYS):
- Files are owned by Unix UID, not usernames
- Processes run with specific UID/GID
- Permissions depend on UID/GID, not Kubernetes identity

Wren bridges Kubernetes (username-based) and HPC (UID-based) by:
1. Capturing user identity at job submission
2. Looking up Unix UID/GID for that user
3. Running processes with correct UID/GID
4. Setting environment variables (USER, HOME, LOGNAME) correctly

## WrenUser CRD

A WrenUser maps a Kubernetes username to Unix identity:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: alice
spec:
  uid: 1001
  gid: 1001
  supplementalGroups:
    - 1001
    - 5000  # project-climate group
  homeDir: /home/alice
  defaultProject: climate-sim
```

Fields:
- `uid` — Unix UID (required)
- `gid` — Primary Unix GID (required)
- `supplementalGroups` — additional GIDs (optional, for multi-project access)
- `homeDir` — HOME directory for shell session (optional)
- `defaultProject` — default project for fair-share accounting (optional)

WrenUsers are **cluster-scoped** — one per user across all namespaces.

## User Identity Flow

Three-layer identity system:

```
Layer 1: Capture
┌─────────────────────────────────────┐
│ kubectl apply -f job.yaml           │
│ User: alice (from kubeconfig)       │
└────────────┬────────────────────────┘
             │
        Mutating Webhook
             │ stamps wren.giar.dev/user annotation
             ▼
┌─────────────────────────────────────┐
│ WrenJob:                            │
│   metadata.annotations:             │
│     wren.giar.dev/user: alice       │
└────────────┬────────────────────────┘

Layer 2: Resolution
             │
        Wren Controller
             │ looks up WrenUser named "alice"
             ▼
┌─────────────────────────────────────┐
│ WrenUser: alice                     │
│   uid: 1001                         │
│   gid: 1001                         │
│   homeDir: /home/alice              │
└────────────┬────────────────────────┘

Layer 3: Execution
             │
       Backend (container or reaper)
             │ sets UID/GID, env vars
             ▼
┌─────────────────────────────────────┐
│ Container:                          │
│   securityContext.runAsUser: 1001   │
│   env.USER: alice                   │
│   env.HOME: /home/alice             │
└─────────────────────────────────────┘
```

## Creating WrenUsers

### Manual Creation

For small clusters, create WrenUser CRDs directly:

```bash
kubectl apply -f - <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: alice
spec:
  uid: 1001
  gid: 1001
  homeDir: /home/alice
  defaultProject: climate-sim
---
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: bob
spec:
  uid: 1002
  gid: 1002
  homeDir: /home/bob
EOF
```

### LDAP Sync

For large clusters with centralized identity, sync from LDAP:

```bash
#!/bin/bash
# sync-ldap-users.sh
# Runs as a CronJob to keep WrenUser CRDs in sync with LDAP

ldapsearch -H ldap://ldap.example.com -b cn=users,dc=example,dc=com \
  '(objectClass=posixAccount)' uid uidNumber gidNumber homeDirectory | \
  awk '
    /^dn:/ { name = $3; gsub(/,cn.*/, "", name); next }
    /^uid:/ { username = $2; next }
    /^uidNumber:/ { uid = $2; next }
    /^gidNumber:/ { gid = $2; next }
    /^homeDirectory:/ { home = $2; next }
    /^$/ {
      if (uid && gid && username) {
        print "apiVersion: wren.giar.dev/v1alpha1"
        print "kind: WrenUser"
        print "metadata:"
        print "  name: " username
        print "spec:"
        print "  uid: " uid
        print "  gid: " gid
        print "  homeDir: " home
        print "---"
      }
      username = uid = gid = home = ""
    }
  ' | kubectl apply -f -
```

Deploy as CronJob:

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: ldap-sync
  namespace: wren-system
spec:
  schedule: "0 */6 * * *"  # Every 6 hours
  jobTemplate:
    spec:
      template:
        spec:
          serviceAccountName: wren-controller
          containers:
          - name: sync
            image: "wren-ldap-sync:latest"
            command: ["/scripts/sync-ldap-users.sh"]
          restartPolicy: OnFailure
```

## Submitting Jobs with User Identity

When you submit a job:

```bash
kubectl apply -f job.yaml
```

The Wren mutating webhook automatically:
1. Reads your Kubernetes user identity
2. Stamps `wren.giar.dev/user: <your-username>` annotation
3. Your kubeconfig user is overridden by the webhook (can't impersonate)

The Wren controller then:
1. Reads the annotation
2. Looks up the matching WrenUser CRD
3. Uses the UID/GID for process execution
4. Sets environment variables from the WrenUser spec

### Example: Submit and Check Identity

```bash
# Submit as alice (authenticated via x509 cert, OIDC, etc.)
kubectl apply -f training-job.yaml

# Webhook stamps user identity
kubectl get wrenjob training-job -o jsonpath='{.metadata.annotations}'
# {"wren.giar.dev/user": "alice"}

# Check pod running as alice's UID
kubectl exec -it training-job-worker-0 -- id
# uid=1001(alice) gid=1001(alice) groups=1001(alice),5000(project-climate)

# Verify HOME is set correctly
kubectl exec -it training-job-worker-0 -- echo $HOME
# /home/alice
```

## Container Backend Identity

For container backend jobs, Wren sets:

**Kubernetes securityContext:**
```yaml
securityContext:
  runAsUser: 1001
  runAsGroup: 1001
  supplementalGroups: [1001, 5000]
```

**Environment variables:**
```
USER=alice
LOGNAME=alice
HOME=/home/alice
```

**Example pod spec created by Wren:**
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: training-job-worker-0
  annotations:
    wren.giar.dev/user: alice
spec:
  securityContext:
    runAsUser: 1001
    runAsGroup: 1001
    supplementalGroups: [1001, 5000]
  containers:
  - name: worker
    image: "pytorch:latest"
    env:
    - name: USER
      value: alice
    - name: LOGNAME
      value: alice
    - name: HOME
      value: /home/alice
    volumeMounts:
    - name: shared-fs
      mountPath: /data
  volumes:
  - name: shared-fs
    nfs:
      server: nfs.example.com
      path: /export/data
```

File written to NFS will be owned by UID 1001, visible as "alice" on the storage server.

## Reaper Backend Identity

For reaper backend jobs, Wren:
1. Passes UID/GID to the Reaper agent via job request
2. Reaper agent calls `setuid()/setgid()` before exec
3. Sets USER, HOME, LOGNAME env vars
4. Mounted volumes respect UID/GID permissions

No changes to user's job script needed — the Reaper agent handles identity.

## Security: No Anonymous or Root Execution

Wren enforces:

**Hard rule: every job must have a valid, non-root user.**

The controller rejects a job at scheduling time if:
1. `wren.giar.dev/user` annotation is missing
2. No matching WrenUser CRD exists
3. WrenUser has `uid: 0` (root)

Example rejection:

```bash
kubectl apply -f job.yaml
# Error: No WrenUser found for user "unknown"
# Job failed with message: "Missing or invalid user identity"
```

This protects against:
- Anonymous job submission
- Privilege escalation (running as root)
- Accidental user mistakes

## Multi-User Fair-Share

The `project` field on WrenJobSpec groups jobs for fair-share accounting:

```yaml
spec:
  project: climate-sim  # User alice, project climate-sim
```

Fair-share tracks usage by `(user, project)` pair:
- Alice's climate-sim jobs compete with her weather-sim jobs
- Alice's jobs compete with Bob's jobs globally
- Fair-share factor boosts lower-usage projects

## Quotas and Limits

WrenQueue enforces per-user limits:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: default
spec:
  maxJobsPerUser: 10  # Max 10 concurrent jobs per user
```

When alice has 10 running jobs, her 11th job waits in queue.

## Checking User Identity

```bash
# List all WrenUsers
kubectl get wrenuser

# Check a user's UID/GID
kubectl get wrenuser alice -o yaml
# Shows: uid, gid, supplementalGroups, homeDir, defaultProject

# Find jobs by user
kubectl get wrenjob -o json | \
  jq '.items[] | select(.metadata.annotations."wren.giar.dev/user" == "alice")'
```

## Troubleshooting Identity

### Job fails: "No WrenUser found for alice"

Solution: Create the WrenUser CRD.

```bash
kubectl apply -f - <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenUser
metadata:
  name: alice
spec:
  uid: 1001
  gid: 1001
  homeDir: /home/alice
EOF
```

### Mounted NFS shows wrong permissions

Check:
1. WrenUser UID/GID matches NFS expectations
2. Container is running with correct securityContext
3. Pod can mount the NFS path (RBAC, firewall)

```bash
# Verify pod UID
kubectl exec -it <pod> -- id
# Should show: uid=1001

# Check mounted volume
kubectl exec -it <pod> -- ls -l /data
# Files should be owned by UID 1001
```

### HOME directory not set

Verify WrenUser has `homeDir`:

```bash
kubectl get wrenuser alice -o jsonpath='{.spec.homeDir}'

# If empty, update:
kubectl patch wrenuser alice --type merge -p '{"spec":{"homeDir":"/home/alice"}}'
```

## API Gateway (Future)

When Wren adds a REST API gateway (Phase 7+), the gateway will:
1. Authenticate users via OIDC/token (independent of Kubernetes)
2. Map external identity to Kubernetes username
3. Stamp `wren.giar.dev/user` annotation before creating WrenJob
4. Wren controller enforces identity as today (require valid WrenUser, reject root)

The three-layer architecture works unchanged: webhook validates annotation, controller resolves WrenUser, backend executes with correct UID/GID.
