use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "devctl", version, about, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the devctl version
    Version,
    /// Record a commit (default HEAD) in its branch's measuring worktree and
    /// print the summary
    Record {
        /// The commit to record (any revision, e.g. a hash or `HEAD~1`)
        #[arg(long)]
        commit: Option<String>,
    },
    /// Show the last record of each branch
    Status {
        /// Show only this branch
        #[arg(long)]
        branch: Option<String>,
    },
    /// The background recorder for one branch. Internal: started by the
    /// `post-commit` hook.
    #[command(hide = true)]
    Worker {
        /// The branch whose queue to record
        #[arg(long)]
        branch: String,
    },
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
