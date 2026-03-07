use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kube::{config::KubeConfigOptions, Client, Config};
use tracing_subscriber::EnvFilter;

mod cancel;
mod logs;
mod queue;
mod status;
mod submit;

/// Wren — HPC Job Scheduler for Kubernetes
#[derive(Parser)]
#[command(name = "wren", version, about, long_about = None)]
struct Cli {
    /// Kubernetes context to use (defaults to current kubeconfig context)
    #[arg(long, global = true)]
    context: Option<String>,

    /// Path to kubeconfig file (defaults to $KUBECONFIG or ~/.kube/config)
    #[arg(long, global = true, env = "KUBECONFIG")]
    kubeconfig: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

/// Build a Kubernetes client, optionally using a specific kubeconfig file and/or context.
async fn build_client(kubeconfig: Option<&str>, context: Option<&str>) -> Result<Client> {
    let config = match (kubeconfig, context) {
        (Some(path), ctx) => {
            let opts = KubeConfigOptions {
                context: ctx.map(|s| s.to_string()),
                ..Default::default()
            };
            let kubeconfig = kube::config::Kubeconfig::read_from(path)
                .with_context(|| format!("failed to read kubeconfig from '{path}'"))?;
            Config::from_custom_kubeconfig(kubeconfig, &opts)
                .await
                .context("failed to build config from kubeconfig")?
        }
        (None, Some(ctx)) => {
            let opts = KubeConfigOptions {
                context: Some(ctx.to_string()),
                ..Default::default()
            };
            Config::from_kubeconfig(&opts)
                .await
                .with_context(|| format!("failed to load kubeconfig context '{ctx}'"))?
        }
        (None, None) => Config::infer().await.context("failed to infer kubeconfig")?,
    };
    Client::try_from(config).context("failed to create Kubernetes client")
}

#[derive(Subcommand)]
enum Commands {
    /// Submit an MPI job from a YAML file
    Submit {
        /// Path to the WrenJob YAML file
        file: String,
        /// Override the queue
        #[arg(short, long)]
        queue: Option<String>,
        /// Override the number of nodes
        #[arg(short, long)]
        nodes: Option<u32>,
    },

    /// Show the current job queue (like squeue)
    Queue {
        /// Filter by queue name
        #[arg(short, long)]
        queue: Option<String>,
        /// Filter by namespace
        #[arg(short, long)]
        namespace: Option<String>,
    },

    /// Cancel a running or pending job (like scancel)
    Cancel {
        /// Job name to cancel
        job: String,
        /// Namespace
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },

    /// Show detailed status of a job (like scontrol show job)
    Status {
        /// Job name
        job: String,
        /// Namespace
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },

    /// View logs from a job's pods (optionally filtered by rank)
    Logs {
        /// Job name
        job: String,
        /// Specific MPI rank to view logs for
        #[arg(short, long)]
        rank: Option<u32>,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
        /// Namespace
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let client = build_client(cli.kubeconfig.as_deref(), cli.context.as_deref()).await?;

    match cli.command {
        Commands::Submit { file, queue, nodes } => {
            submit::run(client, &file, queue.as_deref(), nodes).await
        }
        Commands::Queue { queue, namespace } => {
            self::queue::run(client, queue.as_deref(), namespace.as_deref()).await
        }
        Commands::Cancel { job, namespace } => cancel::run(client, &job, &namespace).await,
        Commands::Status { job, namespace } => status::run(client, &job, &namespace).await,
        Commands::Logs {
            job,
            rank,
            follow,
            namespace,
        } => logs::run(client, &job, rank, follow, &namespace).await,
    }
}
