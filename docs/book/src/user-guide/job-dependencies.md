# Job Dependencies

Wren supports Slurm-style job dependencies to build workflows. A job can depend on the status of prior jobs, allowing sequential or conditional execution.

## Dependency Types

```yaml
dependencies:
  - type: afterOk
    job: previous-job
```

Three dependency types are supported:

- `afterOk` — run after the referenced job completes successfully (exit code 0)
- `afterAny` — run after the referenced job completes, regardless of status
- `afterNotOk` — run only if the referenced job fails (non-zero exit code)

## Basic Example: Sequential Jobs

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: preprocess
spec:
  nodes: 2
  queue: batch
  walltime: "2h"

  container:
    image: "python:3.11"
    command: ["python", "preprocess.py"]

---
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: training
spec:
  nodes: 8
  queue: gpu
  walltime: "8h"

  container:
    image: "pytorch:latest"
    command: ["python", "train.py"]

  dependencies:
    - type: afterOk
      job: preprocess  # Wait for preprocess to succeed
```

The `training` job won't start until `preprocess` succeeds.

## Conditional Execution: afterNotOk

Run a job only if a prior job fails:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: simulation
spec:
  nodes: 4
  walltime: "4h"
  container:
    image: "myapp:latest"
    command: ["./simulate"]

---
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: debug
spec:
  nodes: 2
  walltime: "2h"
  container:
    image: "myapp:latest"
    command: ["./debug"]

  dependencies:
    - type: afterNotOk
      job: simulation  # Run only if simulation fails
```

Useful for:
- Debugging failed simulations
- Cleanup after failed jobs
- Fallback analyses

## Continue on Any: afterAny

Run a job regardless of the prior job's status:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: checkpoint
spec:
  nodes: 1
  walltime: "1h"
  container:
    image: "myapp:latest"
    command: ["./checkpoint"]

  dependencies:
    - type: afterAny
      job: training  # Run whether training succeeded or failed
```

Useful for:
- Collecting logs regardless of outcome
- Always-run cleanup jobs
- Reporting and validation

## Multiple Dependencies

A job can depend on multiple prior jobs:

```yaml
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: final-analysis
spec:
  nodes: 4
  walltime: "4h"
  container:
    image: "analysis:latest"
    command: ["./final-analysis"]

  dependencies:
    - type: afterOk
      job: preprocess
    - type: afterOk
      job: training
    - type: afterOk
      job: validation
```

`final-analysis` runs only after all three prior jobs succeed.

Combining dependency types:

```yaml
dependencies:
  - type: afterOk
    job: simulation-1
  - type: afterOk
    job: simulation-2
  - type: afterAny  # Continue even if comparison fails
    job: comparison
```

This says: wait for both simulations, then comparison (regardless of its status).

## Workflow Example: Multi-Stage Pipeline

```yaml
---
# Stage 1: Data preparation
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: prepare-data
spec:
  nodes: 4
  queue: batch
  walltime: "2h"
  container:
    image: "data-tools:latest"
    command: ["python", "prepare.py"]

---
# Stage 2: Split into two parallel simulations
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: sim-a
spec:
  nodes: 8
  queue: compute
  walltime: "8h"
  container:
    image: "simulator:latest"
    command: ["./run-sim-a"]
  dependencies:
    - type: afterOk
      job: prepare-data

---
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: sim-b
spec:
  nodes: 8
  queue: compute
  walltime: "8h"
  container:
    image: "simulator:latest"
    command: ["./run-sim-b"]
  dependencies:
    - type: afterOk
      job: prepare-data

---
# Stage 3: Merge results
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: merge-results
spec:
  nodes: 2
  queue: batch
  walltime: "1h"
  container:
    image: "data-tools:latest"
    command: ["python", "merge.py"]
  dependencies:
    - type: afterOk
      job: sim-a
    - type: afterOk
      job: sim-b

---
# Stage 4: Analysis
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: analyze
spec:
  nodes: 4
  queue: batch
  walltime: "4h"
  container:
    image: "analysis:latest"
    command: ["python", "analyze.py"]
  dependencies:
    - type: afterOk
      job: merge-results

---
# Stage 5: Report (always runs)
apiVersion: wren.giar.dev/v1alpha1
kind: WrenJob
metadata:
  name: report
spec:
  nodes: 1
  queue: batch
  walltime: "1h"
  container:
    image: "reporting:latest"
    command: ["python", "report.py"]
  dependencies:
    - type: afterAny
      job: analyze
```

Execution order:
1. `prepare-data` runs first
2. `sim-a` and `sim-b` run in parallel after `prepare-data` succeeds
3. `merge-results` waits for both simulations
4. `analyze` runs after merge completes
5. `report` runs regardless of analysis outcome

## Dependency Cycles

Wren detects circular dependencies and rejects them at submission time:

```yaml
# Job A depends on B
# Job B depends on C
# Job C depends on A  <- CYCLE DETECTED
```

Error:

```
Job validation failed: circular dependency detected in job chain
```

Prevention: Wren uses Kahn's algorithm to detect cycles before scheduling.

## Checking Dependency Status

```bash
# View job dependencies in CRD
kubectl get wrenjob training -o yaml

# Check blocking status
kubectl describe wrenjob training
# Status shows: "Waiting for dependency: preprocess"

# List jobs in dependency order
kubectl get wrenjob -o wide
```

Via CLI:

```bash
wren queue  # Shows which jobs are blocked by dependencies
```

## Job Lifecycle with Dependencies

With dependency: `afterOk: preprocess`

Possible states:

| preprocess state | training state | Notes |
|------------------|----------------|-------|
| Running | Pending | Waiting for preprocess to complete |
| Succeeded | Scheduling | Dependency satisfied, can now schedule |
| Failed | Failed | preprocess failed, training fails immediately |
| Cancelled | Failed | preprocess cancelled, training fails |

## Dependency Considerations

1. **Dependencies don't hold cluster resources:**
   - A waiting job doesn't reserve nodes
   - Nodes can be used by other jobs
   - When dependency is satisfied, job enters Scheduling

2. **Failed jobs block dependents:**
   - If job A fails and job B depends on A (afterOk), job B fails
   - Use `afterAny` to continue despite failures

3. **Job names must match exactly:**
   - Typos in job names are caught at job creation (validation fails)
   - Referenced job must exist before dependent job

4. **Cross-namespace dependencies not supported:**
   - Dependencies only work within the same namespace
   - All jobs in a workflow should be in the same namespace

## Future: Job Arrays

Job arrays are planned for Phase 4+. They'll allow:

```yaml
# Planned syntax (not yet implemented)
jobArray: "0-99"     # Create 100 jobs (0..99)
arrayTaskId: 0       # This is job 0 of the array
```

Combined with dependencies:

```yaml
dependencies:
  - type: afterOk
    job: "prepare[*]"  # Wait for all jobs in prepare array
```

Today, use explicit job names and individual dependencies for similar workflows.

## Tips and Patterns

### Pattern 1: Always-Run Cleanup

```yaml
dependencies:
  - type: afterAny
    job: main-job
```

Runs after main-job regardless of status. Useful for:
- Archiving results
- Collecting logs
- Deallocating resources

### Pattern 2: Conditional Failure Handling

```yaml
# Main job fails
# afterNotOk job runs to investigate
dependencies:
  - type: afterNotOk
    job: production-job

# If both succeed, analysis runs
```

### Pattern 3: Multi-Stage with Parallel Stages

```yaml
# Stage 1: prepare (serial)
# Stage 2: sim-a and sim-b (parallel, both depend on prepare)
# Stage 3: merge (depends on both sims)
# Stage 4: analyze (depends on merge)
```

### Pattern 4: Fan-Out to Fan-In

```yaml
# Fan-out: One job produces data for many
preprocess:
  nodes: 2

sim-1, sim-2, sim-3, sim-4:
  all depend on: afterOk preprocess

# Fan-in: Many jobs feed into one
final-merge:
  depends on: all four sims
```

Useful for parameter sweeps and comparative studies.
