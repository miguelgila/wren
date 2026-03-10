# 08 — Distributed PyTorch Training (CPU, Gloo Backend)

Trains a tiny model across 2 bare-metal nodes using PyTorch DistributedDataParallel
with the Gloo backend. No GPU, no shared filesystem, no NCCL — runs on any laptop
with Docker and Kind.

## What It Proves

- **Multi-node gradient sync** — DDP averages gradients across ranks over TCP
- **Weight convergence** — both ranks end with identical model parameters
- **Kubernetes-native delivery** — training script delivered as a ConfigMap volume
- **Bare-metal execution** — ReaperPod CRD → Pod with `runtimeClassName: reaper-v2`
- **No custom API** — standard Kubernetes CRDs, volumes, logs, lifecycle

## Architecture

```
                    ┌──────────────────────────────────┐
                    │         Wren Controller           │
                    │                                    │
                    │  Gang-schedule 2 nodes             │
                    │  Inject MASTER_ADDR, RANK, etc.    │
                    │  Create ReaperPod CRDs             │
                    └────────┬───────────────┬──────────┘
                             │               │
                    ┌────────▼──────┐ ┌──────▼────────┐
                    │ ReaperPod     │ │ ReaperPod     │
                    │ rank-0        │ │ rank-1        │
                    └────────┬──────┘ └──────┬────────┘
                             │               │
                    ┌────────▼──────┐ ┌──────▼────────┐
                    │ Reaper Ctrl   │ │ Reaper Ctrl   │
                    │ → Pod         │ │ → Pod         │
                    │ reaper-v2     │ │ reaper-v2     │
                    └────────┬──────┘ └──────┬────────┘
                             │               │
                    ┌────────▼──────┐ ┌──────▼────────┐
                    │   Worker 0    │ │   Worker 1    │
                    │  ConfigMap    │ │  ConfigMap    │
  Gloo/TCP ◄──────►│  train.py     │ │  train.py     │◄──────► Gloo/TCP
  gradient sync     │  bare metal   │ │  bare metal   │  gradient sync
                    └───────────────┘ └───────────────┘
```

## Files

| File | Description |
|------|-------------|
| `configmap.yaml` | ConfigMap with the PyTorch training script |
| `job.yaml` | WrenJob manifest (what users submit via `wren submit`) |
| `user.yaml` | WrenUser CRD mapping "researcher" to uid 1000 |
| `setup.sh` | Self-contained Kind demo (creates cluster, installs Reaper, runs training) |

## Running the Demo

```bash
# From the wren repo root (reaper repo must be cloned alongside)
./examples/08-distributed-training/setup.sh

# Cleanup
./examples/08-distributed-training/setup.sh --cleanup
```

The setup script:
1. Creates a Kind cluster with 2 worker nodes
2. Installs Reaper runtime + ReaperPod CRD and controller
3. Installs Python3 + PyTorch (CPU-only, ~150 MB) on each node
4. Creates a ConfigMap with the training script
5. Creates 2 ReaperPod CRDs — one per node, with distributed training env vars
6. Waits for completion, prints training output

### Expected Output

```
[rank 0/2] starting on worker-0, master=172.18.0.3:29500
[rank 0] process group initialized
[rank 0] epoch 1/5  loss=1.2345
...
[rank 0] param sums: [1.234, 1.234]
[rank 0] weights synchronized: YES
[rank 0] done in 2.34s
```

## With the Wren Controller

When the controller is deployed:

```bash
kubectl apply -f configmap.yaml
kubectl apply -f user.yaml
wren submit job.yaml
```

The controller handles gang scheduling, env var injection (MASTER_ADDR, RANK,
WORLD_SIZE, etc.), volume mounting, and ReaperPod creation.

## Design Notes

### Why ReaperPod CRD?

The ReaperPod CRD gives you the full Kubernetes feature set for free:

| Feature | ReaperPod CRD | Direct process launch |
|---------|---------------|----------------------|
| ConfigMap volumes | native | not available |
| Secret volumes | native | not available |
| kubectl logs | native | not available |
| kubectl exec | native | not available |
| Pod lifecycle | kubelet manages | manual tracking |
| User identity | securityContext | manual uid/gid |
| Node scheduling | nodeSelector | manual routing |

The Wren reaper backend creates ReaperPod CRDs directly via the Kubernetes API.

### Why Gloo?

PyTorch supports three distributed backends: NCCL (GPU), Gloo (CPU/GPU), and MPI.
Gloo works out of the box on CPU-only machines — perfect for laptop demos. In
production with GPUs, change `backend="gloo"` to `backend="nccl"`.

### Phase 3b: Multi-GPU

When Phase 3b lands:
```yaml
spec:
  nodes: 2
  tasksPerNode: 8        # 8 ranks per node (one per GPU)
```

The Reaper runtime would set `CUDA_VISIBLE_DEVICES` per rank and bind each
process to its NUMA domain. `LOCAL_RANK` would range from 0–7 on each node.
