# CLI Reference

The `wren` command-line tool provides a Slurm-like interface for job submission, monitoring, and management.

## Installation

Install the Wren CLI from a prebuilt release or build from source:

```bash
# From release
wget https://github.com/miguelgila/wren/releases/download/v0.3.0/wren-linux-x86_64
chmod +x wren-linux-x86_64
sudo mv wren-linux-x86_64 /usr/local/bin/wren

# From source
cargo install --path crates/wren-cli
```

Verify installation:

```bash
wren --version
# wren 0.3.0
```

## Configuration

The CLI uses your kubeconfig to connect to the Kubernetes cluster:

```bash
export KUBECONFIG=~/.kube/config
wren queue
```

Override the default namespace:

```bash
wren --namespace my-namespace queue
```

Or set in kubeconfig context:

```bash
kubectl config set-context my-cluster --namespace=default
```

## Commands

### wren submit

Submit a job to Wren.

```bash
wren submit <job.yaml>
```

Prints the assigned job ID:

```bash
$ wren submit training-job.yaml
Submitted job 'training-job' (ID: 42)
```

The job ID is assigned by Wren and increments sequentially (Slurm-style).

**Options:**
- `--namespace <ns>` — submit to a specific namespace (default: current kubeconfig context)
- `--dry-run` — validate without submitting

**Example:**

```bash
# Create job spec
cat > job.yaml <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: my-simulation
spec:
  nodes: 4
  queue: default
  walltime: "4h"
  container:
    image: "python:3.11"
    command: ["python", "simulate.py"]
EOF

# Submit
wren submit job.yaml
# Submitted job 'my-simulation' (ID: 42)
```

### wren queue

List jobs in a queue (like Slurm's `squeue`).

```bash
wren queue
```

Output:

```
JOBID NAME              STATE       NODES QUEUE     PRIORITY USER     AGE
   42 my-simulation     Running        4 default   50       alice    2m
   43 analysis          Scheduling     8 batch     100      bob      1m
   44 training          Pending        2 gpu       50       alice    30s
```

**Fields:**
- `JOBID` — sequential job ID
- `NAME` — job name
- `STATE` — current state (Pending, Scheduling, Running, Succeeded, Failed, Cancelled)
- `NODES` — number of nodes allocated/requested
- `QUEUE` — queue name
- `PRIORITY` — job priority
- `USER` — user who submitted the job
- `AGE` — time since job creation

**Options:**
- `-u <user>` — filter by user
- `-q <queue>` — filter by queue
- `-a` — show all jobs (including completed)
- `-l` — long output with additional details

**Examples:**

```bash
# All jobs
wren queue -a

# My jobs only
wren queue -u alice

# Jobs in gpu queue
wren queue -q gpu

# Detailed output
wren queue -l
```

### wren status

Show detailed status of a specific job.

```bash
wren status <job-id>
```

Output:

```
Job ID:          42
Name:            my-simulation
State:           Running
Queue:           default
Priority:        50
Nodes:           4
Assigned nodes:  [node-0, node-1, node-2, node-3]
Start time:      2024-01-15 10:30:45 UTC
Walltime:        4h
Elapsed:         0h 12m 34s
Ready workers:   4/4
```

**Options:**
- `--namespace <ns>` — look in a specific namespace

**Example:**

```bash
wren status 42
# Shows detailed info about job 42
```

### wren logs

View job output (like Slurm's `scontrol show job` or `sacct`).

```bash
wren logs <job-id>
```

By default, shows output from all ranks combined.

**Options:**
- `--rank <n>` — show output from a specific rank
- `-f` — follow output in real-time (like `tail -f`)
- `--since <duration>` — show logs since duration ago (e.g., `5m`, `1h`)

**Examples:**

```bash
# View all output
wren logs 42

# Rank 0 only
wren logs 42 --rank 0

# Follow output live
wren logs 42 -f

# Recent logs
wren logs 42 --since 5m
```

### wren cancel

Cancel a job (like Slurm's `scancel`).

```bash
wren cancel <job-id>
```

Cancellation:
1. Terminates running pods/processes (SIGTERM, then SIGKILL)
2. Cleans up resources (Service, ConfigMap, Pods)
3. Updates job status to `Cancelled`

**Options:**
- `--signal <sig>` — send specific signal (default: SIGTERM)

**Examples:**

```bash
# Cancel job 42
wren cancel 42
# Cancelled job 42

# Cancel with SIGKILL (force kill)
wren cancel 42 --signal KILL
```

### wren delete (alias)

Synonym for `wren cancel`:

```bash
wren delete 42
```

## YAML Job Submission

All CLI commands use the same YAML format as `kubectl apply`:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: my-job
spec:
  nodes: 4
  queue: default
  priority: 50
  walltime: "4h"

  container:
    image: "python:3.11"
    command: ["python", "train.py"]

  mpi:
    implementation: openmpi
    sshAuth: true
```

Submit with:

```bash
wren submit job.yaml
```

## Job Submission Workflow

Typical workflow:

```bash
# 1. Create job spec
cat > job.yaml <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: my-job
spec:
  nodes: 4
  walltime: "4h"
  container:
    image: "myapp:latest"
    command: ["./myapp"]
EOF

# 2. Submit
wren submit job.yaml
# Submitted job 'my-job' (ID: 42)

# 3. Check queue
wren queue
# JOBID NAME   STATE       NODES QUEUE   PRIORITY ...
#    42 my-job Scheduling     4 default 50       ...

# 4. Wait for running
sleep 10
wren status 42
# State: Running
# Ready workers: 4/4

# 5. View logs
wren logs 42

# 6. Track progress
wren logs 42 -f

# 7. Cancel if needed
wren cancel 42
```

## Job Naming

Job names must be:
- Alphanumeric with hyphens and underscores
- Unique within the namespace
- Max 63 characters (Kubernetes name constraint)

The job ID (auto-assigned by Wren) is numeric and globally unique within the cluster.

## Output and Formatting

By default, commands print human-readable output. Add `-o json` or `-o yaml` for structured output (future versions):

```bash
# Currently: human-readable
wren queue

# Future: JSON output
wren queue -o json
```

## Error Handling

Common errors and solutions:

### "No WrenUser found for alice"

The CLI detected user "alice" but no WrenUser CRD exists. An admin must create it:

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

### "Job not found: 42"

Job ID 42 doesn't exist. List jobs to find the right ID:

```bash
wren queue -a  # Include completed jobs
```

### "Failed to connect to Kubernetes API"

Kubeconfig is invalid or cluster is unreachable. Check:

```bash
kubectl cluster-info
export KUBECONFIG=~/.kube/config
```

### "Queue 'gpu' does not exist"

The queue was not created before job submission. Create it:

```bash
kubectl apply -f - <<EOF
apiVersion: wren.giar.dev/v1alpha1
kind: WrenQueue
metadata:
  name: gpu
spec:
  maxNodes: 32
  maxWalltime: "12h"
  defaultPriority: 50
EOF
```

## Integration with Scripts

The CLI is designed for scripting. Examples:

### Submit and wait for completion

```bash
#!/bin/bash
set -e

# Submit job
JOB_NAME=$1
JOB_ID=$(wren submit $JOB_NAME.yaml | grep -oP '\(ID: \K[0-9]+')
echo "Submitted job $JOB_ID"

# Wait for running state
while true; do
  STATE=$(wren status $JOB_ID 2>/dev/null | grep "^State:" | awk '{print $NF}')
  if [[ "$STATE" == "Running" ]]; then
    echo "Job is running"
    break
  fi
  sleep 5
done

# Wait for completion
while true; do
  STATE=$(wren status $JOB_ID 2>/dev/null | grep "^State:" | awk '{print $NF}')
  if [[ "$STATE" == "Succeeded" || "$STATE" == "Failed" ]]; then
    echo "Job finished: $STATE"
    break
  fi
  sleep 10
done

# Retrieve logs
wren logs $JOB_ID > output-$JOB_ID.txt
echo "Logs saved to output-$JOB_ID.txt"
```

Run with:

```bash
./run-and-wait.sh my-job
```

### Batch job submission

```bash
#!/bin/bash
# Submit multiple jobs from a directory

for jobfile in jobs/*.yaml; do
  echo "Submitting $jobfile..."
  wren submit "$jobfile"
done

# Wait for all to complete
wren queue -a
```

### Monitor job status

```bash
#!/bin/bash
# Print status of job 42 every 10 seconds

JOB_ID=42
while true; do
  clear
  wren status $JOB_ID
  sleep 10
done
```

## Comparison with Slurm

Wren CLI mirrors Slurm commands:

| Slurm | Wren |
|-------|------|
| `sbatch job.sh` | `wren submit job.yaml` |
| `squeue` | `wren queue` |
| `scontrol show job 42` | `wren status 42` |
| `scontrol show job 42 | tail -20` | `wren logs 42` |
| `scancel 42` | `wren cancel 42` |
| `sacct -u alice` | `wren queue -u alice` |

Key differences:
- Wren uses YAML (Kubernetes CRDs), not scripts
- Job IDs are numeric and sequential (like Slurm)
- No `sbatch` script syntax; use YAML spec fields
- Topology and MPI config are declarative in YAML

## Future Enhancements

Planned CLI features:
- `-o json` / `-o yaml` structured output
- `wren accounting` — usage reports per user/project (like Slurm's `sacct`)
- Job templates and environment variable expansion
- Batch submission with parameter substitution
- Interactive job shells with live monitoring
