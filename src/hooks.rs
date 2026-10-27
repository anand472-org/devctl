//! `devctl hook <name>`: what the git hook scripts call. Installing them comes
//! with `devctl hooks install`.

use std::io::Read as _;

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::HookName;
use crate::ctx::Ctx;
use crate::gate::{self, GateReport};
use crate::{Process, in_session};

/// Runs hook `name` with git's arguments `args`.
///
/// # Errors
/// For `pre-push`: when the push is blocked or the gate cannot be evaluated
/// (it fails closed). Hooks not in this build always fail.
pub fn run_hook(name: HookName, args: &[String]) -> Result<()> {
    match name {
        HookName::PrePush => {
            let process = Process::Hook(name.as_str().to_string());
            let report = in_session(
                &process,
                || tracing::info_span!("hook", name = name.as_str()),
                |ctx| {
                    let mut stdin = String::new();
                    std::io::stdin()
                        .read_to_string(&mut stdin)
                        .context("cannot read the refs git passed on stdin")?;
                    let report = pre_push(ctx, args, &stdin)?;
                    print!("{}", report.text);
                    Ok(report)
                },
            )
            .context("the pre-push gate could not run, so the push is blocked")?;
            if report.blocked {
                bail!("push blocked by devctl; fix what is listed above, then push again");
            }
            Ok(())
        }
        HookName::PostCommit | HookName::PreCommit => bail!(
            "hook `{}` is not available in this build; upgrade devctl, or remove the hook \
             from .git/hooks",
            name.as_str()
        ),
    }
}

/// The `pre-push` gate: `args` are git's (`<remote name or URL> <URL>`),
/// `stdin` its ref lines.
///
/// # Errors
/// On missing arguments, unparsable input or any failure evaluating a rule.
pub fn pre_push(ctx: &Ctx, args: &[String], stdin: &str) -> Result<GateReport> {
    let name = args.first().ok_or_else(|| {
        anyhow!(
            "pre-push was called without the remote name; git passes it, so run \
             `git push` instead of calling the hook directly"
        )
    })?;
    let remote = gate::Remote::from_hook_args(name, args.get(1).map(String::as_str));
    let updates = gate::parse_pre_push(stdin)?;
    let mut text = String::new();
    let mut blocked = false;
    for update in &updates {
        let results = gate::evaluate(ctx, &remote, update)?;
        blocked |= gate::blocked(&results);
        text.push_str(&gate::render(update, &results));
    }
    Ok(GateReport { text, blocked })
}
