use clap::{Parser, Subcommand};

/// Mill — container orchestrator for small-to-medium deployments.
#[derive(Debug, Parser)]
#[command(name = "mill", version, about)]
pub struct Cli {
    /// API endpoint.
    #[arg(long, global = true, default_value = "http://127.0.0.1:4400", env = "MILL_ADDRESS")]
    pub address: String,

    /// Auth token (or MILL_TOKEN env var).
    #[arg(long, global = true, env = "MILL_TOKEN")]
    pub token: Option<String>,

    /// Data directory for node state.
    #[arg(long, global = true, default_value = "/var/lib/mill", env = "MILL_DATA_DIR")]
    pub data_dir: String,

    /// Output as JSON instead of tables.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a new cluster (first node).
    Init {
        /// Cluster auth token (auto-generated if omitted).
        #[arg(long)]
        token: Option<String>,
        /// Advertise address for this node.
        #[arg(long)]
        advertise: Option<String>,
        /// Cloud provider name.
        #[arg(long)]
        provider: Option<String>,
        /// Cloud provider API token.
        #[arg(long)]
        provider_token: Option<String>,
        /// Contact email for ACME (Let's Encrypt). Enables automatic TLS.
        #[arg(long)]
        acme_email: Option<String>,
        /// Use the Let's Encrypt staging directory.
        #[arg(long)]
        acme_staging: bool,
    },

    /// Join an existing cluster.
    Join {
        /// Address of an existing cluster node.
        address: String,
        /// Cluster auth token.
        #[arg(long)]
        token: String,
        /// Advertise address for this node.
        #[arg(long)]
        advertise: Option<String>,
        /// Cloud provider name.
        #[arg(long)]
        provider: Option<String>,
        /// Cloud provider API token.
        #[arg(long)]
        provider_token: Option<String>,
        /// Contact email for ACME (Let's Encrypt). Enables automatic TLS.
        #[arg(long)]
        acme_email: Option<String>,
        /// Use the Let's Encrypt staging directory.
        #[arg(long)]
        acme_staging: bool,
    },

    /// Leave the cluster gracefully.
    Leave,

    /// Deploy or update from config file.
    Deploy {
        /// Path to config file.
        #[arg(short = 'f', long = "file", default_value = "./mill.conf")]
        file: String,
    },

    /// Restart a service (same image, new container).
    Restart {
        /// Service name.
        service: String,
    },

    /// Spawn an ephemeral task.
    Spawn {
        /// Task template name.
        task: Option<String>,

        /// Container image (inline mode).
        #[arg(long)]
        image: Option<String>,

        /// CPU allocation.
        #[arg(long)]
        cpu: Option<f64>,

        /// Memory allocation (e.g. "2G").
        #[arg(long)]
        memory: Option<String>,

        /// Timeout (e.g. "30m", "1h").
        #[arg(long)]
        timeout: Option<String>,

        /// Environment variables (KEY=VALUE).
        #[arg(long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,
    },

    /// List running tasks.
    Ps,

    /// Kill a running task.
    Kill {
        /// Task instance ID.
        task_id: String,
    },

    /// Cluster overview.
    Status,

    /// Stream logs for a service or task.
    Logs {
        /// Service or task name.
        name: Option<String>,

        /// Follow log output.
        #[arg(short = 'f', long)]
        follow: bool,

        /// Number of lines to show.
        #[arg(short = 'n', long = "tail")]
        tail: Option<u64>,

        /// Stream logs from all services.
        #[arg(long)]
        all: bool,
    },

    /// Manage secrets.
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },

    /// Manage volumes.
    Volume {
        #[command(subcommand)]
        action: VolumeAction,
    },

    /// List cluster nodes.
    Nodes,

    /// Drain a node (reschedule allocs, mark unschedulable).
    Drain {
        /// Node ID.
        node_id: String,
    },

    /// Remove a drained node from the cluster.
    Remove {
        /// Node ID.
        node_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretAction {
    /// Set or update a secret.
    Set {
        /// Secret name.
        name: String,
        /// Secret value.
        value: String,
    },
    /// Get a secret value.
    Get {
        /// Secret name.
        name: String,
    },
    /// List all secrets.
    List,
    /// Delete a secret.
    Delete {
        /// Secret name.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum VolumeAction {
    /// List volumes and their locations.
    List,
    /// Destroy a persistent volume (fails if in use).
    Delete {
        /// Volume name.
        name: String,
    },
}
