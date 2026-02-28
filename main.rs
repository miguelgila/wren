use anyhow::Result;
use clap::{Parser, Subcommand};

mod submit;
mod queue;
mod cancel;
mod status;
mod logs;

/// Bubo — HPC Job Scheduler for Kubernetes
#[derive(Parser)]
#[command(name = "bubo", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Submit an MPI job from a YAML file
    Submit {
        /// Path to the MPIJob YAML file
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
        /// Filter by user/namespace
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
    let cli = Cli::parse();

    match cli.command {
        Commands::Submit { file, queue, nodes } => {
            submit::run(&file, queue.as_deref(), nodes).await
        }
        Commands::Queue { queue, namespace } => {
            self::queue::run(queue.as_deref(), namespace.as_deref()).await
        }
        Commands::Cancel { job, namespace } => {
            cancel::run(&job, &namespace).await
        }
        Commands::Status { job, namespace } => {
            status::run(&job, &namespace).await
        }
        Commands::Logs {
            job,
            rank,
            follow,
            namespace,
        } => logs::run(&job, rank, follow, &namespace).await,
    }
}
