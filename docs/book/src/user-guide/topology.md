# Topology-Aware Placement

Wren can place jobs on nodes that are physically close in the network, reducing MPI latency and improving performance. This guide covers topology constraints and placement scoring.

## Why Topology Matters

In HPC clusters, nodes are connected via hierarchical networks:

```
        Switch A (Slingshot)
       /                  \
    Node0              Node1
    Node2              Node3
    Node4              Node5

        Switch B
       /        \
    Node6      Node7
```

Jobs with good topology (all nodes on Switch A) have:
- Lower network latency
- Fewer hops between MPI processes
- Better RDMA performance

Jobs with poor topology (half on Switch A, half on Switch B) have:
- Extra network hops
- Potential congestion at inter-switch links
- 10-50% slower MPI performance

## Node Labels and Topology Keys

Wren discovers cluster topology from node labels:

```bash
kubectl get nodes -L topology.kubernetes.io/zone,topology.kubernetes.io/region
# NAME     STATUS   ... ZONE    REGION
# node-0   Ready        us-west us-west-1
# node-1   Ready        us-west us-west-1
# node-2   Ready        us-west us-west-2
# node-3   Ready        us-west us-west-2
```

Or custom HPC-specific labels:

```bash
kubectl label node node-0 switch=switch-a rack=rack-0-0 cabinet=cab-0
kubectl label node node-1 switch=switch-a rack=rack-0-1 cabinet=cab-0
kubectl label node node-2 switch=switch-b rack=rack-1-0 cabinet=cab-1
```

Then use those labels for placement:

```yaml
spec:
  topology:
    topologyKey: "switch"  # Prefer nodes with same 'switch' label
```

## Topology Spec Fields

```yaml
topology:
  preferSameSwitch: true
  maxHops: 2
  topologyKey: "topology.kubernetes.io/zone"
```

- `preferSameSwitch` — prefer all nodes on the same switch/zone (default: false)
- `maxHops` — maximum allowed hops between any two nodes (default: unlimited)
- `topologyKey` — node label key to use for grouping (default: `topology.kubernetes.io/zone`)

## Placement Scoring

Wren scores candidate node placements:

1. **Same-switch bonus** — all nodes on same switch get +100 points
2. **Hop penalty** — each hop between nodes costs -10 points per hop
3. **Pack bonus** — nodes in same zone/rack get clustering bonus

Example:

Cluster:
- Switch A: nodes 0, 1, 2 (low latency)
- Switch B: nodes 3, 4, 5 (low latency)
- Cross-switch latency is 10x higher

Job: 4 nodes

Candidate 1: [0, 1, 2, 3]
- 3 nodes on Switch A (bonus)
- 1 node on Switch B (no bonus)
- Cross-switch hops: penalty

Candidate 2: [0, 1, 3, 4]
- 2 nodes per switch
- More cross-switch hops
- Lower score

Wren picks Candidate 1.

## Container Backend Example: Same-Switch Topology

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: simulation-v1
spec:
  nodes: 8
  queue: batch
  walltime: "4h"

  container:
    image: "nvcr.io/nvidia/pytorch:24.01-py3"
    command: ["mpirun", "-np", "8", "--hostfile", "/etc/wren/hostfile", "./simulate"]
    hostNetwork: true

  mpi:
    implementation: openmpi
    sshAuth: true
    fabricInterface: hsn0

  topology:
    preferSameSwitch: true
    maxHops: 0  # All nodes must be on same switch
    topologyKey: "switch"
```

This job requires all 8 nodes on the same switch. If no switch has 8 available nodes, the job stays in `Scheduling` until resources free up.

## Reaper Backend Example: Zone-Based Topology

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: climate-sim
spec:
  nodes: 16
  backend: reaper
  walltime: "24h"

  reaper:
    script: |
      #!/bin/bash
      module load cray-mpich
      srun ./climate_model.x

  topology:
    preferSameSwitch: true
    maxHops: 2
    topologyKey: "topology.kubernetes.io/zone"
```

This job prefers nodes in the same Kubernetes zone, with maximum 2 hops between any pair. Wren will:
1. Try to place all 16 nodes in the same zone (highest score)
2. If unavailable, split across 2 zones with max 2 hops
3. Reject placements requiring >2 hops

## No Topology Constraint

If you don't care about network proximity:

```yaml
spec:
  # No topology field — cluster picks any available nodes
```

The scheduler places nodes greedily to fill resources, ignoring distance.

## Topology Constraints vs. Max Hops

- `preferSameSwitch` — soft constraint, but high-scoring. Job runs if available nodes are spread out.
- `maxHops` — hard constraint. Job stays in Scheduling if violated.

Example with `preferSameSwitch: true, maxHops: 3`:
- If all nodes available on one switch: use them (score +100)
- If only 4 nodes on one switch, rest elsewhere: place them across multiple switches (max 3 hops penalty)
- If some placement requires 4+ hops: still allowed (maxHops: 3 allows up to 3 hops)

Wait: **maxHops is actually a hard constraint** — violating it blocks scheduling.

Correct: With `maxHops: 3`, the scheduler rejects any placement with >3 hops between any node pair. Job stays in Scheduling.

## Discovering Cluster Topology

```bash
# View node labels
kubectl get nodes -L topology.kubernetes.io/zone,topology.kubernetes.io/region
kubectl get nodes -L switch,rack,cabinet

# Describe a node to see all labels
kubectl describe node node-0

# Check what Wren detected
kubectl logs -n wren-system deployment/wren-controller | grep topology
```

If your cluster doesn't have labels, add them:

```bash
# Add zone labels (if using cloud provider labels)
kubectl label node node-0 topology.kubernetes.io/zone=us-west-1a
kubectl label node node-1 topology.kubernetes.io/zone=us-west-1a
kubectl label node node-2 topology.kubernetes.io/zone=us-west-1b

# Add custom HPC labels
kubectl label node node-0 switch=slingshot-a rack=r0
kubectl label node node-1 switch=slingshot-a rack=r0
kubectl label node node-2 switch=slingshot-b rack=r1
```

Then use those labels in topology specs:

```yaml
spec:
  topology:
    topologyKey: "switch"  # Use the 'switch' label for grouping
```

## Topology Key Strategy

Choose a `topologyKey` that matches your network hierarchy:

| Cluster Type | Recommended topologyKey | Reason |
|--------------|-------------------------|--------|
| Kubernetes cloud (GKE, EKS, AKS) | `topology.kubernetes.io/zone` | Built-in zone labels |
| HPC with Slingshot | `switch` | Network switch is most relevant |
| HPC with InfiniBand | `rack` | Rack-level switch bandwidth |
| Small cluster | `topology.kubernetes.io/hostname` | Node locality |

## Performance Tips

1. **Always enable topology for MPI-heavy workloads:**
   ```yaml
   topology:
     preferSameSwitch: true
     maxHops: 2
   ```

2. **Use strict maxHops for latency-sensitive jobs:**
   - Trading off resource utilization for guaranteed performance
   - Example: `maxHops: 0` ensures all nodes on same switch

3. **Monitor placement scores in Prometheus:**
   ```
   wren_topology_score{job="my-job"}
   ```
   - Average score shows placement quality
   - Low scores indicate poor topology

4. **Label your nodes at cluster setup time:**
   - Add switch, rack, zone labels during Day 1
   - Wren then uses them automatically
   - No labeling required per job

## Common Mistakes

- **Forgetting `topologyKey`:** Wren uses `topology.kubernetes.io/zone` by default. If your cluster uses custom labels (e.g., `switch`), you must specify it.
- **Setting `maxHops: 0` on large jobs:** Might never schedule if cluster doesn't have that many nodes on one switch. Use `maxHops: 2` for more flexibility.
- **Not adding node labels:** If nodes lack topology labels, Wren can't optimize placement. Add labels via kubectl at cluster setup.
