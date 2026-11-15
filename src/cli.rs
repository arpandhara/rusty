use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "rusty")]
#[command(about = "Git like version control")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Launch interactive Terminal UI dashboard
    Tui,
    Init,
    HashObject {
        file: String,
    },
    Add {
        file: String,
    },
    Status,
    WriteTree,
    Commit {
        #[arg(short, long)]
        message: String,
    },
    Log,
    Push,
    Fetch,
    Clone {
        url: String,
        directory: Option<String>,
    },
    Checkout {
        branch: String,
    },
    Rm {
        #[arg(long)]
        cached: bool,

        path: String,
    },
    Login {
        #[arg(short, long)]
        server: Option<String>,
    },
    Logout,
    Whoami,
    Remote {
        #[command(subcommand)]
        command: RemoteCommands,
    },
    Merge {
        branch: Option<String>,

        #[arg(long)]
        abort: bool,
    },
    Branch {
        name: Option<String>,

        #[arg(short, long)]
        remote: bool,

        #[arg(short, long)]
        all: bool,
    },
    Pull,
    Fsck {
        /// Show objects that exist but are never directly referenced
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dangling: bool,

        /// Print out objects that exist but are unreachable from any reference
        #[arg(long)]
        unreachable: bool,

        /// Check connectivity of reachable objects without hashing blob payloads
        #[arg(long)]
        connectivity_only: bool,

        /// Enable strict validation (check tree entry names, hash syntax, commit messages)
        #[arg(long)]
        strict: bool,

        /// Write dangling objects into .rusty/lost-found/
        #[arg(long)]
        lost_found: bool,

        /// Output diagnostic report in JSON format for automated tooling
        #[arg(long)]
        json: bool,
    },
    /// Reset current HEAD, staging index, and/or working tree safely and explicitly
    Reset {
        /// Target commit, branch, or revision (e.g. HEAD, HEAD^, HEAD~2, main). Defaults to HEAD.
        target: Option<String>,

        /// Move the current branch reference (or HEAD if detached) to target commit
        #[arg(long)]
        move_ref: bool,

        /// Update the staging index to match the target tree
        #[arg(long)]
        index: bool,

        /// Update the working tree files to match the target blobs
        #[arg(long)]
        worktree: bool,

        /// Compatibility alias: equivalent to --move-ref
        #[arg(long)]
        soft: bool,

        /// Compatibility alias: equivalent to --move-ref --index
        #[arg(long)]
        mixed: bool,

        /// Compatibility alias: equivalent to --move-ref --index --worktree
        #[arg(long)]
        hard: bool,

        /// Preview planned changes and safety checks without mutating state
        #[arg(long)]
        dry_run: bool,

        /// Output the plan in machine-readable JSON format
        #[arg(long)]
        json: bool,

        /// Confirm execution without interactive prompt
        #[arg(short, long)]
        yes: bool,

        /// Optional paths to limit index and/or worktree updates
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Inspect or undo repository state changed by a previous operation
    Recover {
        #[command(subcommand)]
        command: RecoverCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum RecoverCommands {
    /// Inspect or restore the ref moved by the most recent completed reset
    Reset {
        /// Preview the recovery without changing the ref
        #[arg(long)]
        dry_run: bool,

        /// Output the recovery preview in machine-readable JSON format
        #[arg(long)]
        json: bool,

        /// Confirm restoration without interactive prompt
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum RemoteCommands {
    Add { name: String, url: String },
}
