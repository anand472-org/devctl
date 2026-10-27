use clap::{Parser, Subcommand, ValueEnum};

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
    /// Run the pre-push rules for the current branch tip without pushing
    Gate {
        /// The branch the push would update (default: the current branch)
        #[arg(long)]
        target: Option<String>,
    },
    /// The background recorder for one branch. Internal: started by the
    /// `post-commit` hook.
    #[command(hide = true)]
    Worker {
        /// The branch whose queue to record
        #[arg(long)]
        branch: String,
    },
    /// Manage the git hooks devctl uses
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Runs a git hook. Internal: called by the scripts `devctl hooks
    /// install` writes.
    #[command(hide = true)]
    Hook {
        /// The hook git is running
        name: HookName,
        /// The arguments git passed to the hook
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// What `devctl hooks` can do.
#[derive(Debug, Subcommand)]
pub enum HooksAction {
    /// Write the `pre-commit`, `post-commit` and `pre-push` hook scripts into
    /// the repository's hooks directory
    Install,
}

/// The git hooks devctl implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookName {
    PostCommit,
    PreCommit,
    PrePush,
}

impl HookName {
    /// The hook's git name (`pre-push`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HookName::PostCommit => "post-commit",
            HookName::PreCommit => "pre-commit",
            HookName::PrePush => "pre-push",
        }
    }
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
