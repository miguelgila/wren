use axum::{routing::get, Router};
use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};
use std::sync::Arc;
use tracing::info;

#[derive(Clone)]
pub struct Metrics {
    pub registry: Arc<Registry>,

    // Job lifecycle
    pub jobs_total: IntCounterVec,
    pub jobs_active: IntGaugeVec,

    // Queue metrics
    pub queue_depth: IntGaugeVec,

    // Scheduling metrics
    pub scheduling_attempts: IntCounterVec,
    pub scheduling_latency: HistogramVec,
    pub job_wait_time: HistogramVec,

    // Node/resource metrics
    pub nodes_total: IntGauge,
    pub nodes_allocatable: IntGauge,
    pub cpu_utilization: prometheus::Gauge,
    pub memory_utilization: prometheus::Gauge,
    pub gpu_utilization: prometheus::Gauge,

    // Reservation tracking
    pub active_reservations: IntGauge,

    // Topology scoring
    pub topology_score: Histogram,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let jobs_total = IntCounterVec::new(
            Opts::new("wren_jobs_total", "Total number of jobs processed"),
            &["state", "queue"],
        )
        .expect("metric can be created");
        registry.register(Box::new(jobs_total.clone())).unwrap();

        let jobs_active = IntGaugeVec::new(
            Opts::new("wren_jobs_active", "Number of currently active jobs"),
            &["state", "queue"],
        )
        .expect("metric can be created");
        registry.register(Box::new(jobs_active.clone())).unwrap();

        let queue_depth = IntGaugeVec::new(
            Opts::new("wren_queue_depth", "Number of pending jobs per queue"),
            &["queue"],
        )
        .expect("metric can be created");
        registry.register(Box::new(queue_depth.clone())).unwrap();

        let scheduling_attempts = IntCounterVec::new(
            Opts::new(
                "wren_scheduling_attempts_total",
                "Total scheduling attempts",
            ),
            &["result"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(scheduling_attempts.clone()))
            .unwrap();

        let scheduling_latency = HistogramVec::new(
            HistogramOpts::new(
                "wren_scheduling_latency_seconds",
                "Time spent in scheduling decisions",
            )
            .buckets(vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0]),
            &["result"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(scheduling_latency.clone()))
            .unwrap();

        let job_wait_time = HistogramVec::new(
            HistogramOpts::new(
                "wren_job_wait_seconds",
                "Time jobs spend waiting in queue before scheduling",
            )
            .buckets(vec![
                1.0, 5.0, 10.0, 30.0, 60.0, 300.0, 600.0, 1800.0, 3600.0,
            ]),
            &["queue"],
        )
        .expect("metric can be created");
        registry.register(Box::new(job_wait_time.clone())).unwrap();

        let nodes_total = IntGauge::new("wren_nodes_total", "Total nodes tracked by scheduler")
            .expect("metric can be created");
        registry.register(Box::new(nodes_total.clone())).unwrap();

        let nodes_allocatable =
            IntGauge::new("wren_nodes_allocatable", "Nodes with available resources")
                .expect("metric can be created");
        registry
            .register(Box::new(nodes_allocatable.clone()))
            .unwrap();

        let cpu_utilization = prometheus::Gauge::new(
            "wren_cluster_cpu_utilization_ratio",
            "Cluster-wide CPU utilization ratio [0.0, 1.0]",
        )
        .expect("metric can be created");
        registry
            .register(Box::new(cpu_utilization.clone()))
            .unwrap();

        let memory_utilization = prometheus::Gauge::new(
            "wren_cluster_memory_utilization_ratio",
            "Cluster-wide memory utilization ratio [0.0, 1.0]",
        )
        .expect("metric can be created");
        registry
            .register(Box::new(memory_utilization.clone()))
            .unwrap();

        let gpu_utilization = prometheus::Gauge::new(
            "wren_cluster_gpu_utilization_ratio",
            "Cluster-wide GPU utilization ratio [0.0, 1.0]",
        )
        .expect("metric can be created");
        registry
            .register(Box::new(gpu_utilization.clone()))
            .unwrap();

        let active_reservations = IntGauge::new(
            "wren_active_reservations",
            "Number of active resource reservations (scheduling in progress)",
        )
        .expect("metric can be created");
        registry
            .register(Box::new(active_reservations.clone()))
            .unwrap();

        let topology_score = Histogram::with_opts(
            HistogramOpts::new(
                "wren_topology_placement_score",
                "Topology score of accepted placements",
            )
            .buckets(vec![0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0]),
        )
        .expect("metric can be created");
        registry.register(Box::new(topology_score.clone())).unwrap();

        Self {
            registry: Arc::new(registry),
            jobs_total,
            jobs_active,
            queue_depth,
            scheduling_attempts,
            scheduling_latency,
            job_wait_time,
            nodes_total,
            nodes_allocatable,
            cpu_utilization,
            memory_utilization,
            gpu_utilization,
            active_reservations,
            topology_score,
        }
    }

    pub fn record_job_state_change(&self, queue: &str, old_state: &str, new_state: &str) {
        if !old_state.is_empty() {
            self.jobs_active
                .with_label_values(&[old_state, queue])
                .dec();
        }
        self.jobs_active
            .with_label_values(&[new_state, queue])
            .inc();
        self.jobs_total.with_label_values(&[new_state, queue]).inc();
    }

    pub fn record_scheduling_latency(&self, result: &str, duration_secs: f64) {
        self.scheduling_latency
            .with_label_values(&[result])
            .observe(duration_secs);
    }

    pub fn record_job_wait_time(&self, queue: &str, wait_secs: f64) {
        self.job_wait_time
            .with_label_values(&[queue])
            .observe(wait_secs);
    }

    pub fn update_queue_depth(&self, queue: &str, depth: i64) {
        self.queue_depth.with_label_values(&[queue]).set(depth);
    }

    pub fn update_cluster_utilization(&self, cpu: f64, memory: f64, gpu: f64) {
        self.cpu_utilization.set(cpu);
        self.memory_utilization.set(memory);
        self.gpu_utilization.set(gpu);
    }

    pub fn update_node_counts(&self, total: i64, allocatable: i64) {
        self.nodes_total.set(total);
        self.nodes_allocatable.set(allocatable);
    }

    pub fn record_reservation_start(&self) {
        self.active_reservations.inc();
    }

    pub fn record_reservation_end(&self) {
        self.active_reservations.dec();
    }

    pub fn record_topology_score(&self, score: f64) {
        self.topology_score.observe(score);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

async fn metrics_handler(
    metrics: axum::extract::State<Metrics>,
) -> Result<String, axum::http::StatusCode> {
    let encoder = TextEncoder::new();
    let metric_families = metrics.registry.gather();
    let mut buffer = Vec::new();
    encoder
        .encode(&metric_families, &mut buffer)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    String::from_utf8(buffer).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}

async fn health_handler() -> &'static str {
    "ok"
}

pub async fn serve_metrics(metrics: Metrics, port: u16) {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(health_handler))
        .route("/readyz", get(health_handler))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .expect("failed to bind metrics port");

    info!(port, "metrics server listening");
    axum::serve(listener, app)
        .await
        .expect("metrics server failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let m = Metrics::new();
        m.record_job_state_change("default", "", "Pending");
        m.record_job_state_change("default", "Pending", "Running");

        let families = m.registry.gather();
        assert!(!families.is_empty());
    }

    #[test]
    fn test_scheduling_latency_histogram() {
        let m = Metrics::new();
        m.record_scheduling_latency("success", 0.025);
        m.record_scheduling_latency("no_placement", 0.001);

        let families = m.registry.gather();
        let latency = families
            .iter()
            .find(|f| f.get_name() == "wren_scheduling_latency_seconds");
        assert!(latency.is_some());
    }

    #[test]
    fn test_job_wait_time_histogram() {
        let m = Metrics::new();
        m.record_job_wait_time("default", 15.5);
        m.record_job_wait_time("gpu", 120.0);

        let families = m.registry.gather();
        let wait = families
            .iter()
            .find(|f| f.get_name() == "wren_job_wait_seconds");
        assert!(wait.is_some());
    }

    #[test]
    fn test_queue_depth_tracking() {
        let m = Metrics::new();
        m.update_queue_depth("default", 5);
        m.update_queue_depth("gpu", 3);

        let families = m.registry.gather();
        let depth = families.iter().find(|f| f.get_name() == "wren_queue_depth");
        assert!(depth.is_some());
    }

    #[test]
    fn test_cluster_utilization_metrics() {
        let m = Metrics::new();
        m.update_cluster_utilization(0.75, 0.6, 0.5);

        let families = m.registry.gather();
        let cpu = families
            .iter()
            .find(|f| f.get_name() == "wren_cluster_cpu_utilization_ratio");
        assert!(cpu.is_some());
    }

    #[test]
    fn test_reservation_tracking() {
        let m = Metrics::new();
        m.record_reservation_start();
        m.record_reservation_start();
        m.record_reservation_end();

        let families = m.registry.gather();
        let res = families
            .iter()
            .find(|f| f.get_name() == "wren_active_reservations");
        assert!(res.is_some());
    }

    #[test]
    fn test_topology_score_histogram() {
        let m = Metrics::new();
        m.record_topology_score(0.85);
        m.record_topology_score(1.0);

        let families = m.registry.gather();
        let topo = families
            .iter()
            .find(|f| f.get_name() == "wren_topology_placement_score");
        assert!(topo.is_some());
    }
}
