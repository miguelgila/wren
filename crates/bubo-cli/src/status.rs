use anyhow::{Context, Result};
use bubo_core::BuboJob;
use kube::{api::Api, Client};

/// Show detailed status information about a specific BuboJob.
pub async fn run(job: &str, namespace: &str) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("failed to create Kubernetes client")?;

    let api: Api<BuboJob> = Api::namespaced(client, namespace);

    let bubojob = api
        .get(job)
        .await
        .with_context(|| format!("job {job} not found in namespace {namespace}"))?;

    let spec = &bubojob.spec;
    let status = bubojob.status.as_ref();

    println!("Name:        {}", job);
    println!("Namespace:   {}", namespace);
    println!("Queue:       {}", spec.queue);
    println!("Priority:    {}", spec.priority);
    println!("Nodes:       {}", spec.nodes);
    println!("Tasks/Node:  {}", spec.tasks_per_node);
    println!(
        "Backend:     {}",
        format!("{:?}", spec.backend).to_lowercase()
    );
    if let Some(wt) = &spec.walltime {
        println!("Walltime:    {}", wt);
    }

    println!();
    println!("Status:");
    if let Some(s) = status {
        println!("  State:       {}", s.state);
        if let Some(msg) = &s.message {
            println!("  Message:     {}", msg);
        }
        println!("  Workers:     {}/{}", s.ready_workers, s.total_workers);
        if let Some(start) = &s.start_time {
            println!("  Start Time:  {}", start);
        }
        if let Some(end) = &s.completion_time {
            println!("  End Time:    {}", end);
        }
        if !s.assigned_nodes.is_empty() {
            println!("  Nodes:       {}", s.assigned_nodes.join(", "));
        }
    } else {
        println!("  State:       Unknown (no status subresource)");
    }

    if !spec.dependencies.is_empty() {
        println!();
        println!("Dependencies:");
        for dep in &spec.dependencies {
            println!("  {:?} -> {}", dep.dep_type, dep.job);
        }
    }

    Ok(())
}
