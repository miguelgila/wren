# Monitoring

Wren exposes comprehensive Prometheus metrics for observing scheduler behavior, cluster utilization, and job lifecycle events. This page describes all available metrics and how to integrate with Prometheus and Grafana.

## Metrics Endpoint

The controller exposes metrics at:

```
http://<controller-pod>:8080/metrics
```

Metrics are in Prometheus text format (line-based). Scrape interval is configurable in your Prometheus config (default: 30 seconds).

## Enabling Metrics Collection

### Prometheus Operator (Recommended)

If your cluster has prometheus-operator installed:

```bash
helm install wren charts/wren/ \
  --set metrics.serviceMonitor.enabled=true \
  --set metrics.serviceMonitor.additionalLabels.release=prometheus
```

This creates a ServiceMonitor resource. Prometheus automatically discovers and scrapes it.

Verify:

```bash
kubectl get servicemonitor -n wren-system
kubectl get prometheus -o yaml | grep serviceMonitorSelectorLabels
```

### Manual Prometheus Config

Add to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'wren'
    kubernetes_sd_configs:
      - role: pod
        namespaces:
          names:
            - wren-system
    relabel_configs:
      - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_scrape]
        action: keep
        regex: "true"
      - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_path]
        action: replace
        target_label: __metrics_path__
        regex: (.+)
      - source_labels: [__address__, __meta_kubernetes_pod_annotation_prometheus_io_port]
        action: replace
        regex: ([^:]+)(?::\d+)?;(\d+)
        replacement: $1:$2
        target_label: __address__
```

## Available Metrics

All metrics are prefixed with `wren_` and use standard Prometheus conventions.

### Job Lifecycle Metrics

#### wren_jobs_total

**Type:** Counter (increments only)

**Labels:** `state`, `queue`

**Description:** Total number of jobs that have entered each state, grouped by queue.

**Usage:** Understand job throughput and completion rate.

```prometheus
# Total successful jobs in 'default' queue over last hour
rate(wren_jobs_total{state="Succeeded", queue="default"}[1h])

# Failed jobs
increase(wren_jobs_total{state="Failed"}[1d])
```

#### wren_jobs_active

**Type:** Gauge (goes up and down)

**Labels:** `state`, `queue`

**Description:** Number of currently active jobs in each state per queue.

**Usage:** Monitor real-time job counts and detect stuck jobs.

```prometheus
# Jobs currently running
wren_jobs_active{state="Running"}

# Pending jobs (stuck waiting for resources)
wren_jobs_active{state="Scheduling"}

# Alert if pending jobs exceed threshold
wren_jobs_active{state="Scheduling", queue="default"} > 100
```

### Queue Metrics

#### wren_queue_depth

**Type:** Gauge (represents queue length)

**Labels:** `queue`

**Description:** Number of pending jobs in each queue.

**Usage:** Monitor queue backlog and identify resource bottlenecks.

```prometheus
# Queue 1 has 50 pending jobs
wren_queue_depth{queue="default"}

# Alert on excessive queue depth
wren_queue_depth{queue="default"} > 200
```

### Scheduling Metrics

#### wren_scheduling_attempts_total

**Type:** Counter

**Labels:** `result` (values: `success`, `failed`, `no_fit`)

**Description:** Total scheduling attempts by outcome.

**Usage:** Understand scheduling efficiency and failure modes.

```prometheus
# Successful placements per minute
rate(wren_scheduling_attempts_total{result="success"}[1m])

# Failed placements (resources unavailable)
increase(wren_scheduling_attempts_total{result="no_fit"}[1h])
```

#### wren_scheduling_latency_seconds

**Type:** Histogram (buckets: 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0)

**Labels:** `result` (`success`, `failed`, `no_fit`)

**Description:** Time spent in scheduling decisions, grouped by outcome.

**Usage:** Identify scheduling bottlenecks; monitor algorithm performance.

```prometheus
# 95th percentile latency for successful placements
histogram_quantile(0.95, rate(wren_scheduling_latency_seconds_bucket{result="success"}[5m]))

# Average latency
rate(wren_scheduling_latency_seconds_sum[5m]) / rate(wren_scheduling_latency_seconds_count[5m])

# Alert if p99 latency exceeds 1 second (indicates thrashing)
histogram_quantile(0.99, rate(wren_scheduling_latency_seconds_bucket{result="success"}[5m])) > 1.0
```

#### wren_job_wait_seconds

**Type:** Histogram (buckets: 1, 5, 10, 30, 60, 300, 600, 1800, 3600)

**Labels:** `queue`

**Description:** Time jobs spend waiting in queue before being scheduled.

**Usage:** Identify when resources are oversubscribed.

```prometheus
# Average wait time for jobs in 'default' queue
rate(wren_job_wait_seconds_sum{queue="default"}[1h]) / rate(wren_job_wait_seconds_count{queue="default"}[1h])

# Alert if 95th percentile wait exceeds 30 minutes
histogram_quantile(0.95, rate(wren_job_wait_seconds_bucket{queue="default"}[1h])) > 1800
```

### Node and Resource Metrics

#### wren_nodes_total

**Type:** Gauge

**Description:** Total number of nodes in the cluster.

**Usage:** Track cluster size changes.

```prometheus
wren_nodes_total
```

#### wren_nodes_allocatable

**Type:** Gauge

**Description:** Number of nodes with allocatable (non-tainted, not cordoned) capacity.

**Usage:** Identify when nodes go offline.

```prometheus
# Alert if > 10% of nodes are unavailable
wren_nodes_allocatable / wren_nodes_total < 0.9
```

#### wren_cpu_utilization

**Type:** Gauge (0.0 to 1.0)

**Description:** Cluster-wide CPU utilization as a fraction of total allocatable capacity.

**Usage:** Monitor overcommit and plan cluster expansion.

```prometheus
# Cluster is 80% utilized
wren_cpu_utilization > 0.8
```

#### wren_memory_utilization

**Type:** Gauge (0.0 to 1.0)

**Description:** Cluster-wide memory utilization.

**Usage:** Identify memory pressure.

```prometheus
# Alert on high memory utilization
wren_memory_utilization > 0.9
```

#### wren_gpu_utilization

**Type:** Gauge (0.0 to 1.0)

**Description:** Cluster-wide GPU utilization (if GPUs are present).

**Usage:** Monitor GPU allocation and identify GPU bottlenecks.

### Reservation Metrics

#### wren_active_reservations

**Type:** Gauge

**Description:** Number of active backfill reservations (high-priority jobs blocking resources).

**Usage:** Understand backfill overhead.

```prometheus
# If > 5, backfill is blocking many lower-priority jobs
wren_active_reservations > 5
```

### Topology Metrics

#### wren_topology_score

**Type:** Histogram

**Description:** Distribution of topology scores for successful placements (higher = better locality).

**Usage:** Verify topology-aware scheduling is working.

```prometheus
# Average topology score (0-1)
rate(wren_topology_score_sum[1h]) / rate(wren_topology_score_count[1h])
```

## Example Prometheus Rules

Create a PrometheusRule for alerting:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: wren-alerts
  namespace: wren-system
spec:
  groups:
    - name: wren
      interval: 30s
      rules:
        # Alert if jobs are stuck in scheduling for > 1 hour
        - alert: WrenJobsStuck
          expr: |
            wren_jobs_active{state="Scheduling"} > 0
            and
            (time() - (wren_job_wait_seconds_created{queue="default"} + wren_job_wait_seconds_sum{queue="default"} / wren_job_wait_seconds_count{queue="default"})) > 3600
          for: 15m
          annotations:
            summary: "Jobs stuck in Wren scheduling queue"
            description: "{{ $value }} jobs have been waiting > 1 hour"

        # Alert if scheduling latency is high (possible thrashing)
        - alert: WrenSchedulingLatencyHigh
          expr: |
            histogram_quantile(0.99, rate(wren_scheduling_latency_seconds_bucket{result="success"}[5m])) > 1.0
          for: 5m
          annotations:
            summary: "Wren scheduling latency is high"
            description: "p99 latency is {{ $value }}s (threshold: 1s)"

        # Alert if cluster is fully utilized
        - alert: WrenClusterFull
          expr: |
            (wren_cpu_utilization > 0.95 and wren_memory_utilization > 0.95)
            or
            wren_gpu_utilization > 0.95
          for: 10m
          annotations:
            summary: "Wren cluster is nearly fully utilized"
            description: "CPU: {{ with query "wren_cpu_utilization" }}{{ . | humanizePercentage }}{{ end }}, Memory: {{ with query "wren_memory_utilization" }}{{ . | humanizePercentage }}{{ end }}"

        # Alert if node count drops
        - alert: WrenNodesDown
          expr: |
            (wren_nodes_allocatable / wren_nodes_total) < 0.9
          for: 5m
          annotations:
            summary: "Wren has lost > 10% of nodes"
            description: "Only {{ $value | humanizePercentage }} of nodes are available"
```

## Grafana Integration

### Import Pre-built Dashboard

A Grafana dashboard JSON is available at: (to be added to the repository)

```bash
# Download and import
curl -s https://raw.githubusercontent.com/miguelgila/wren/main/grafana/wren-dashboard.json | \
  jq '.dashboard.title = "Wren Scheduler"' | \
  curl -X POST -H "Content-Type: application/json" \
    -d @- \
    http://grafana:3000/api/dashboards/db \
    -u admin:password
```

### Create Custom Dashboard

Example Grafana dashboard panels:

#### Panel 1: Queue Depth

```
Title: Queue Depth
Query: wren_queue_depth
Type: Graph
Legend: {{ queue }}
```

Shows number of pending jobs per queue over time.

#### Panel 2: Job Success Rate

```
Title: Job Success Rate (% of jobs that succeeded)
Query:
  rate(wren_jobs_total{state="Succeeded"}[1h]) /
  (
    rate(wren_jobs_total{state="Succeeded"}[1h]) +
    rate(wren_jobs_total{state="Failed"}[1h]) +
    rate(wren_jobs_total{state="WalltimeExceeded"}[1h]) +
    rate(wren_jobs_total{state="Cancelled"}[1h])
  )
Type: Stat
Format: Percent
```

#### Panel 3: Scheduling Latency (p95)

```
Title: Scheduling Latency (95th percentile)
Query: histogram_quantile(0.95, rate(wren_scheduling_latency_seconds_bucket[5m]))
Type: Graph
Legend: {{ result }}
Y-Axis: Seconds
```

#### Panel 4: Cluster Utilization

```
Title: Cluster Resource Utilization
Queries:
  - wren_cpu_utilization as CPU
  - wren_memory_utilization as Memory
  - wren_gpu_utilization as GPU
Type: Graph
Y-Axis: 0-1.0 (or 0-100%)
```

#### Panel 5: Active Jobs by State

```
Title: Active Jobs by State
Query: wren_jobs_active
Type: Graph
Legend: {{ state }}, {{ queue }}
```

## Metrics for Capacity Planning

### CPU/Memory Trends

Track utilization over time to identify growth and plan upgrades:

```prometheus
# 7-day average CPU utilization
avg_over_time(wren_cpu_utilization[7d])

# Month-over-month growth
rate(wren_jobs_total[30d]) - rate(wren_jobs_total offset 30d[30d])
```

### Job Completion Rate

Measure throughput:

```prometheus
# Jobs completed per day
increase(wren_jobs_total{state="Succeeded"}[1d])

# Average job duration
avg(wren_job_wait_seconds) + avg(wren_scheduling_latency_seconds)
```

### Backfill Effectiveness

Measure how much backfill helps:

```prometheus
# If backfill is working well, most lower-priority jobs complete despite high-priority jobs blocking
rate(wren_jobs_total{state="Succeeded"}[1h])
```

## Performance Tuning via Metrics

### High Scheduling Latency

If `wren_scheduling_latency_seconds` is high (> 100ms per attempt):

1. Check cluster size: large clusters need better algorithms
2. Check job complexity: topology constraints slow down scheduling
3. Increase log level to `debug` and look for algorithm bottlenecks

### Excessive Queue Depth

If `wren_queue_depth` grows without bound:

1. Cluster is oversubscribed — need to buy more nodes
2. Jobs are taking longer than expected — check `wren_job_wait_seconds`
3. Backfill is disabled — enable it to maximize utilization

### Stuck Jobs in Scheduling

If `wren_jobs_active{state="Scheduling"}` stays high for hours:

1. Topology constraints are too strict — jobs can't fit
2. Backfill reservations are blocking all nodes — check `wren_active_reservations`
3. New nodes aren't being detected — check node watcher logs

## Custom Metrics

To add custom metrics for your site:

1. Modify `crates/wren-controller/src/metrics.rs`
2. Register new metric in `Metrics::new()`
3. Update `.status` in reconciler or scheduler to record values
4. Export via Prometheus text format in `/metrics` endpoint

Example: track fair-share usage per user

```rust
pub struct Metrics {
    // ... existing fields ...
    pub fair_share_usage: IntGaugeVec,  // labels: user, project
}

// In reconciler:
metrics.fair_share_usage
    .with_label_values(&[&user, &project])
    .set(usage_hours);
```

Then query:

```prometheus
fair_share_usage{user="alice"} / 100  # As percentage of quota
```
