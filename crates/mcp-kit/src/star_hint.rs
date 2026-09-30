//! A one-time star line on stderr after the fifth successful CLI run.
//!
//! The caller chooses the message, opt-out variable and state directory. The line is shown once
//! ever, and never for the MCP server, for a non-TTY stderr, in CI, or when
//! the caller's opt-out variable is set.

use std::io::IsTerminal;
use std::path::Path;

const FILE_NAME: &str = "star-hint";
const SHOWN: &str = "shown";
const SHOW_AFTER_RUNS: u32 = 5;

/// What the environment says about where the run is happening.
pub struct Context {
    pub stderr_is_tty: bool,
    pub mcp: bool,
    pub ci: bool,
    pub opted_out: bool,
}

impl Context {
    fn from_process(opt_out_env: &str, mcp: bool) -> Self {
        let set = |key: &str| std::env::var_os(key).is_some_and(|v| !v.is_empty());
        Context {
            stderr_is_tty: std::io::stderr().is_terminal(),
            mcp,
            ci: set("CI"),
            opted_out: set(opt_out_env),
        }
    }

    fn quiet(&self) -> bool {
        self.mcp || !self.stderr_is_tty || self.ci || self.opted_out
    }
}

/// Decide from the stored counter text. Returns the text to store next and
/// whether to print the line now.
pub fn step(stored: Option<&str>, context: &Context) -> (Option<String>, bool) {
    if context.quiet() {
        return (None, false);
    }
    let stored = stored.map(str::trim).unwrap_or("");
    if stored == SHOWN {
        return (None, false);
    }
    let runs = stored.parse::<u32>().unwrap_or(0).saturating_add(1);
    if runs >= SHOW_AFTER_RUNS {
        (Some(SHOWN.to_string()), true)
    } else {
        (Some(runs.to_string()), false)
    }
}

fn apply(path: &Path, context: &Context) -> bool {
    let stored = std::fs::read_to_string(path).ok();
    let (next, show) = step(stored.as_deref(), context);
    if let Some(next) = next {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(path, next).is_err() {
            return false;
        }
    }
    show
}

/// Call after a CLI run that succeeded. Best effort: any file error is silent.
pub fn after_success(message: &str, opt_out_env: &str, state_dir: &Path, mcp: bool) {
    let context = Context::from_process(opt_out_env, mcp);
    if context.quiet() {
        return;
    }
    if apply(&state_dir.join(FILE_NAME), &context) {
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(stderr_is_tty: bool, ci: bool, opted_out: bool) -> Context {
        Context { stderr_is_tty, ci, opted_out, mcp: false }
    }

    #[test]
    fn counts_four_runs_then_shows_once() {
        let interactive = context(true, false, false);
        let mut stored: Option<String> = None;
        for run in 1..=4 {
            let (next, show) = step(stored.as_deref(), &interactive);
            assert!(!show, "run {run}");
            stored = next;
        }
        assert_eq!(stored.as_deref(), Some("4"));
        let (next, show) = step(stored.as_deref(), &interactive);
        assert!(show);
        assert_eq!(next.as_deref(), Some("shown"));
        let (next, show) = step(next.as_deref(), &interactive);
        assert!(!show);
        assert_eq!(next, None);
    }

    #[test]
    fn stays_silent_and_uncounted_when_gated() {
        for quiet in [context(false, false, false), context(true, true, false), context(true, false, true), Context { mcp: true, ..context(true, false, false) }] {
            assert_eq!(step(Some("4"), &quiet), (None, false));
        }
    }

    #[test]
    fn garbage_counter_restarts() {
        let interactive = context(true, false, false);
        assert_eq!(step(Some("x"), &interactive), (Some("1".into()), false));
    }

    #[test]
    fn file_errors_are_silent_and_mcp_never_writes() {
        let dir = std::env::temp_dir().join(format!("mcp-kit-star-hint-errors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!apply(&dir, &context(true, false, false)));
        let path = dir.join(FILE_NAME);
        let mcp = Context { mcp: true, ..context(true, false, false) };
        assert!(!apply(&path, &mcp));
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn persists_and_shows_on_the_fifth_run_only() {
        let dir = std::env::temp_dir().join(format!("mcp-kit-star-hint-{}", std::process::id()));
        let path = dir.join("nested").join(FILE_NAME);
        let interactive = context(true, false, false);
        let shown: Vec<bool> = (0..7).map(|_| apply(&path, &interactive)).collect();
        assert_eq!(shown, [false, false, false, false, true, false, false]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "shown");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
