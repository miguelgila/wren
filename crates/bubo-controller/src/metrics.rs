use axum::{routing::get, Router};
use prometheus::{Encoder, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder};
use std::sync::Arc;
use tracing::info;

#[derive(Clone)]
pub struct Metrics {
    pub registry: Arc<Registry>,
    pub jobs_total: IntCounterVec,
    pub jobs_active: IntGaugeVec,
    pub queue_depth: IntGaugeVec,
    pub scheduling_attempts: IntCounterVec,
    pub nodes_total: IntGauge,
    pub nodes_allocatable: IntGauge,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let jobs_total = IntCounterVec::new(
            Opts::new("bubo_jobs_total", "Total number of jobs processed"),
            &["state", "queue"],
        )
        .expect("metric can be created");
        registry.register(Box::new(jobs_total.clone())).unwrap();

        let jobs_active = IntGaugeVec::new(
            Opts::new("bubo_jobs_active", "Number of currently active jobs"),
            &["state", "queue"],
        )
        .expect("metric can be created");
        registry.register(Box::new(jobs_active.clone())).unwrap();

        let queue_depth = IntGaugeVec::new(
            Opts::new("bubo_queue_depth", "Number of pending jobs per queue"),
            &["queue"],
        )
        .expect("metric can be created");
        registry.register(Box::new(queue_depth.clone())).unwrap();

        let scheduling_attempts = IntCounterVec::new(
            Opts::new(
                "bubo_scheduling_attempts_total",
                "Total scheduling attempts",
            ),
            &["result"],
        )
        .expect("metric can be created");
        registry
            .register(Box::new(scheduling_attempts.clone()))
            .unwrap();

        let nodes_total = IntGauge::new("bubo_nodes_total", "Total nodes tracked by scheduler")
            .expect("metric can be created");
        registry.register(Box::new(nodes_total.clone())).unwrap();

        let nodes_allocatable =
            IntGauge::new("bubo_nodes_allocatable", "Nodes with available resources")
                .expect("metric can be created");
        registry
            .register(Box::new(nodes_allocatable.clone()))
            .unwrap();

        Self {
            registry: Arc::new(registry),
            jobs_total,
            jobs_active,
            queue_depth,
            scheduling_attempts,
            nodes_total,
            nodes_allocatable,
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
        self.jobs_total
            .with_label_values(&[new_state, queue])
            .inc();
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
}
