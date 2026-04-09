# Troubleshooting

This page covers common issues when running Wren and how to diagnose them.

## General Debugging

### Enable Debug Logging

Increase log verbosity to see detailed scheduling decisions and API interactions:

```bash
kubectl set env deployment/wren-controller -n wren-system RUST_LOG=debug

# Follow logs in real-time
kubectl logs -n wren-system deployment/wren-controller -f
```

Log levels:
- `error` — only failures
- `warn` — recoverable issues
- `info` — state transitions and key events (default)
- `debug` — scheduling decisions, resource allocations, API calls
- `trace` — everything (very verbose, avoid in production)

Restore normal logging:

```bash
kubectl set env deployment/wren-controller -n wren-system RUST_LOG=info
```

### Check Controller Status

Verify the controller is running:

```bash
kubectl get pod -n wren-system -l app.kubernetes.io/name=wren-controller
kubectl describe pod -n wren-system -l app.kubernetes.io/name=wren-controller

# Check health probes
kubectl get events -n wren-system | grep wren-controller
```

If pod is not Running:
1. Check resource limits: `kubectl top pod -n wren-system`
2. Check node capacity: `kubectl top nodes`
3. Check RBAC permissions: see [Configuration](configuration.md)

### Access Metrics

View live metrics:

```bash
kubectl port-forward -n wren-system svc/wren-controller-metrics 8080:8080 &
curl http://localhost:8080/metrics | grep wren_
```

This helps identify bottlenecks (high scheduling latency, queue buildup, etc.).

## Job Stuck in Scheduling

### Symptom

Job status shows `Scheduling` for extended time:

```bash
wren status my-job
# State: Scheduling
# Message: Waiting for resources
```

### Diagnosis

1. **Check available resources:**

```bash
kubectl top nodes
kubectl get nodes -o custom-columns=NAME:.metadata.name,CPU:.status.allocatable.cpu,MEM:.status.allocatable.memory,PODS:.status.allocatable.pods

# Identify if any nodes have free capacity
```

2. **Check if topology constraints are too strict:**

```bash
kubectl get wrenjob my-job -o yaml | grep topology
# If topology.prefer_same_switch or max_hops are set, verify nodes match them

kubectl get nodes --show-labels | grep topology
# Check if nodes have the expected topology labels
```

3. **Check scheduler latency:**

```bash
kubectl port-forward -n wren-system svc/wren-controller-metrics 8080:8080
curl http://localhost:8080/metrics | grep scheduling_latency
```

High latency (> 1 second) indicates scheduler is thrashing or algo is slow.

4. **Enable debug logging and look for scheduling output:**

```bash
kubectl set env deployment/wren-controller -n wren-system RUST_LOG=debug
kubectl logs -n wren-system deployment/wren-controller -f | grep scheduling

# Look for messages like:
# "no fit: insufficient CPU"
# "no fit: topology constraint violated"
# "placement found: nodes=[node-1, node-2]"
```

### Solutions

**Resources unavailable:**

- Cluster is full — need more nodes or terminate lower-priority jobs
- Check backfill is enabled: `kubectl get wrenqueue default -o yaml | grep backfill`
- Increase job priority: `wren submit job.yaml --priority 100`

**Topology constraints too strict:**

```yaml
# Don't require same switch if not needed
# topology:
#   prefer_same_switch: false  # or omit

# Or increase max_hops
topology:
  max_hops: 10
```

**Scheduler algorithm issue:**

- If cluster is mostly idle but job doesn't schedule, check logs for errors
- Report with debug logs to the Wren team

## Job Failed with "No Valid User"

### Symptom

Job immediately transitions to `Failed`:

```bash
kubectl get wrenjob my-job -o yaml | grep -A5 status
# state: Failed
# message: "Job failed: no valid user identity"
```

### Diagnosis

1. **Check annotation was stamped:**

```bash
kubectl get wrenjob my-job -o yaml | grep wren.giar.dev/user
```

Expected: `wren.giar.dev/user: alice`

If missing:
- Webhook may not be running: `kubectl get validatingwebhookconfiguration`
- Webhook may be failing: `kubectl logs -n wren-system -l app=wren-webhook`

2. **Check WrenUser CRD exists:**

```bash
kubectl get wrenuser alice
# Error: wrens.wren.giar.dev "alice" not found → need to create WrenUser
```

3. **Check WrenUser has valid UID:**

```bash
kubectl get wrenuser alice -o yaml
# spec.uid should be > 0 and not 0 (root)
```

### Solutions

**Create WrenUser CRD:**

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
    - 5000
  homeDir: /home/alice
  defaultProject: data-science
```

Apply:

```bash
kubectl apply -f wrenuser.yaml
```

**Sync from LDAP:**

Create a CronJob that pulls users from LDAP and creates WrenUser CRDs:

```bash
kubectl create cronjob sync-ldap --image=ldaptools --schedule="0 */6 * * *" -- \
  ldapsearch -x -H ldap://ldap.example.com -b ou=people,dc=example,dc=com \
  | xargs -I {} kubectl apply -f /dev/stdin
```

## Pod Not Starting / Stuck in Pending

### Symptom

Worker or launcher pod stays in `Pending`:

```bash
kubectl get pods -l wren.giar.dev/job-name=my-job
# NAME                   READY   STATUS
# my-job-launcher        0/1     Pending
# my-job-worker-0        0/1     Pending
```

### Diagnosis

1. **Check pod events:**

```bash
kubectl describe pod my-job-launcher

# Look for events:
# Type     Reason            Message
# ----     ------            -------
# Warning  Insufficient...   "insufficient cpu", "insufficient memory"
```

2. **Check node availability:**

```bash
kubectl get nodes
kubectl top nodes
kubectl get nodes -o custom-columns=NAME:.metadata.name,READY:.status.conditions[?(@.type=="Ready")].status
```

3. **Check if node is cordoned or has taints:**

```bash
kubectl get nodes -o custom-columns=NAME:.metadata.name,CORDONED:.spec.unschedulable
kubectl describe nodes | grep Taints
```

### Solutions

**Insufficient resources:**

- Scale up node pool or add more nodes
- Reduce job resource requests: `container.resources.requests`
- Kill lower-priority jobs to free up capacity

**Node cordoned:**

```bash
kubectl uncordon node-name
```

**Node has taints:**

Add toleration to job:

```yaml
container:
  # ... other config ...
  tolerations:
    - key: gpu
      operator: Equal
      value: "true"
      effect: NoSchedule
```

**PVC or network issue:**

```bash
# Check if PVCs are bound
kubectl get pvc

# Check network connectivity
kubectl run nettest --image=busybox -- sleep 3600
kubectl exec nettest -- ping <target-node>
```

## High CPU Usage

### Symptom

Controller pod uses > 500m CPU consistently:

```bash
kubectl top pod -n wren-system -l app.kubernetes.io/name=wren-controller
```

### Diagnosis

1. **Check queue depth:**

```bash
curl http://localhost:8080/metrics | grep queue_depth
```

High queue depth + high CPU = scheduler running many passes (thrashing).

2. **Check scheduling latency:**

```bash
curl http://localhost:8080/metrics | grep scheduling_latency
```

If p99 > 500ms, scheduler is slow.

3. **Check reconciliation frequency:**

```bash
kubectl logs -n wren-system deployment/wren-controller | \
  grep "reconcile" | wc -l
```

If > 100 reconciles/second, controller is thrashing.

### Solutions

**Reduce log overhead:**

```bash
kubectl set env deployment/wren-controller -n wren-system RUST_LOG=warn
```

**Increase reconciliation backoff:**

(Requires code change in reconciler) Set requeue duration higher for non-urgent events.

**Scale up controller resources:**

```bash
kubectl set resources deployment/wren-controller -n wren-system \
  --limits=cpu=1000m,memory=1Gi \
  --requests=cpu=500m,memory=512Mi
```

**Add more nodes to reduce scheduling pressure:**

More nodes = more placement options = faster scheduling.

## Jobs Complete But Pods Remain

### Symptom

Completed job shows status `Succeeded`, but pods are still running:

```bash
wren status completed-job
# State: Succeeded

kubectl get pods -l wren.giar.dev/job-name=completed-job
# my-job-launcher   1/1    Running   0   5m
# my-job-worker-0   1/1    Running   0   5m
```

### Diagnosis

1. **Check pod TTL:**

```bash
kubectl get pod my-job-launcher -o yaml | grep ttlSecondsAfterFinished
# Should be set (default: 86400 = 24 hours)
```

2. **Check if TTL controller is running:**

```bash
# Check if ttl-after-finished feature gate is enabled
kubectl api-resources | grep TTL
```

### Solutions

Pods are intentionally preserved for 24 hours to allow log retrieval. This is normal.

To manually clean up:

```bash
kubectl delete pods -l wren.giar.dev/job-name=completed-job
```

To shorten TTL, modify job spec:

```yaml
spec:
  ttlSecondsAfterFinished: 3600  # 1 hour instead of 24
```

## Logs Not Available

### Symptom

`wren logs` returns nothing or error:

```bash
wren logs my-job
# error: pods not found
```

### Diagnosis

1. **Check if pods still exist:**

```bash
kubectl get pods -l wren.giar.dev/job-name=my-job --all-namespaces
```

2. **Check pod logs directly:**

```bash
kubectl logs my-job-launcher -n <namespace>
kubectl logs my-job-worker-0 -n <namespace>
```

3. **Check if job was submitted to correct namespace:**

```bash
kubectl get wrenjob -A | grep my-job
```

### Solutions

**Pods were deleted (beyond TTL):**

Logs are gone. Consider:
- Increase job TTL: `ttlSecondsAfterFinished: 604800` (7 days)
- Use a log aggregator (ELK, Loki, etc.) to persist logs

**Job in different namespace:**

```bash
wren logs my-job -n production  # specify namespace
```

**Get logs before pod deletion:**

```bash
kubectl logs <pod-name> -n <namespace> > job.log
```

## MPI Job Fails with Connection Timeout

### Symptom

MPI job fails during startup:

```bash
wren logs my-mpi-job --rank 0
# ...
# [pmi-connect-timeout]: Unable to connect to process manager
# [rank 0] process exited with code 1
```

### Diagnosis

1. **Check hostfile was created:**

```bash
kubectl get configmap -l wren.giar.dev/job-name=my-mpi-job
# Should show <job>-hostfile

kubectl get configmap <job>-hostfile -o yaml | grep -A20 data
# Should list all nodes
```

2. **Check if all worker pods are running:**

```bash
kubectl get pods -l wren.giar.dev/job-name=my-mpi-job
# All should be Running before mpirun starts
```

3. **Check network connectivity:**

```bash
kubectl exec <job>-launcher -- ping <job>-worker-0.<headless-svc>
# Should succeed
```

4. **Check MPI implementation:**

```bash
kubectl get wrenjob my-mpi-job -o yaml | grep -A5 mpi
# Check implementation: openmpi, cray-mpich, intel-mpi
```

### Solutions

**Worker pods not all running:**

- Increase pod startup timeout: backoff in Job spec
- Check worker pod logs: `kubectl logs <job>-worker-0`
- Verify resources are available: `kubectl top nodes`

**Network connectivity issue:**

```bash
# Verify headless service exists
kubectl get svc <job>-workers

# Verify DNS resolution
kubectl exec <pod> -- nslookup <job>-worker-0.<job>-workers.default.svc.cluster.local

# Check network policy
kubectl get networkpolicies
```

**MPI implementation mismatch:**

Ensure container image has the correct MPI:

```yaml
container:
  image: myimage:with-cray-mpich  # must match mpi.implementation
mpi:
  implementation: cray-mpich
```

## Reaper Job Stuck in Pending

### Symptom

Job with `backend: reaper` stays in `Scheduling`:

```bash
wren status reaper-job
# State: Scheduling
# Backend: reaper
```

### Diagnosis

1. **Check if Reaper agents are running:**

```bash
kubectl get pods -n wren-system -l app=reaper -o wide

# Should show agents on compute nodes
```

2. **Check if ReaperPod CRDs are created:**

```bash
kubectl get reaperpod -l wren.giar.dev/job-name=reaper-job
# Should show one ReaperPod per rank in Pending or Running state
```

3. **Check Reaper agent logs:**

```bash
kubectl logs -n wren-system -l app=reaper | grep reaper-job
```

### Solutions

**Reaper agents not running:**

```bash
# Deploy Reaper
helm install wren charts/wren/ --set reaper.enabled=true

# Check node labels/tolerations
kubectl get nodes -L wren-reaper
```

**ReaperPod created but not transitioning:**

```bash
# Check ReaperPod status
kubectl get reaperpod reaper-job-rank-0 -o yaml | tail -20

# Check agent logs for errors
kubectl logs -n wren-system -l app=reaper --tail=100
```

**Agent-node communication issue:**

Verify network connectivity from agent pod to compute node:

```bash
kubectl exec -n wren-system <reaper-pod> -- \
  ssh compute-01 echo "connectivity ok"
```

## Leader Election Issues

### Symptom

Multiple controller replicas all think they're leader, or no leader elected:

```bash
# Check controller logs
kubectl logs -n wren-system deployment/wren-controller | grep leader

# All show "this instance is now the leader"
```

### Diagnosis

1. **Check Lease resource:**

```bash
kubectl get lease -n wren-system wren-controller-leader -o yaml

# Check .spec.holderIdentity and .metadata.managedFields
```

2. **Check controller identities:**

```bash
kubectl get pods -n wren-system -o wide | grep wren-controller

# Each should have a unique hostname (leader election identity)
```

### Solutions

**Reset leader election:**

```bash
kubectl delete lease -n wren-system wren-controller-leader

# Controller will immediately elect a new leader
kubectl logs -n wren-system -l app.kubernetes.io/name=wren-controller -f | grep leader
```

**Disable leader election (for testing only):**

```bash
kubectl set env deployment/wren-controller -n wren-system WREN_LEADER_ELECTION=false

# Now all replicas will process jobs (NOT recommended for production)
```

## Getting Help

When reporting issues, include:

1. **Controller logs (last 100 lines):**

```bash
kubectl logs -n wren-system deployment/wren-controller --tail=100 > controller.log
```

2. **Job manifest:**

```bash
kubectl get wrenjob my-job -o yaml > job.yaml
```

3. **Job pod logs:**

```bash
kubectl logs -l wren.giar.dev/job-name=my-job --tail=50 > job-pods.log
```

4. **Cluster info:**

```bash
kubectl get nodes -o wide
kubectl top nodes
kubectl get events -n wren-system --sort-by='.lastTimestamp'
```

5. **Metrics snapshot:**

```bash
curl http://localhost:8080/metrics > metrics.txt
```

Include all of these when reporting a bug on GitHub.
