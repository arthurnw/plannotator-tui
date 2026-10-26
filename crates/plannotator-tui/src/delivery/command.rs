//! Delivery through a user-supplied command, for terminals and multiplexers other than Herdr.
//!
//! `PLANNOTATOR_TUI_DELIVER_COMMAND` names a shell command that receives the feedback on
//! stdin and hands it to the agent however its environment allows (tmux `send-keys`, Rex
//! `rex send`, …). Its exit status maps onto the same outcomes as Herdr's `agent prompt`, so
//! the app reacts the same way: retry later, fall back to the clipboard, or report the error.

use std::io::Write as _;
use std::process::{Command, Stdio};

use super::{Delivery, DeliveryError};

/// `EX_TEMPFAIL`: the agent is at a dialog; keep the notes pending and retry later.
const EXIT_BLOCKED: i32 = 75;
/// `EX_UNAVAILABLE`: there is no agent to send to.
const EXIT_UNAVAILABLE: i32 = 69;

#[derive(Debug)]
pub(crate) struct DeliverCommand {
    command: String,
    agent: Option<String>,
}

impl DeliverCommand {
    fn new(command: String, agent: Option<String>) -> Self {
        Self { command, agent }
    }

    /// From `PLANNOTATOR_TUI_DELIVER_COMMAND`, labelled by `PLANNOTATOR_TUI_DELIVER_AGENT`.
    pub(crate) fn from_lookup(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let non_empty = |k: &str| env(k).filter(|v| !v.trim().is_empty());
        let command = non_empty("PLANNOTATOR_TUI_DELIVER_COMMAND")?;
        Some(Self::new(command, non_empty("PLANNOTATOR_TUI_DELIVER_AGENT")))
    }

    fn shell(&self) -> Command {
        if cfg!(windows) {
            let mut shell = Command::new("cmd");
            shell.args(["/C", &self.command]);
            shell
        } else {
            let mut shell = Command::new("sh");
            shell.args(["-c", &self.command]);
            shell
        }
    }
}

impl Delivery for DeliverCommand {
    fn agent_host(&self) -> Option<&str> {
        self.agent.as_deref()
    }

    fn describe(&self) -> String {
        self.agent.clone().unwrap_or_else(|| "deliver command".to_owned())
    }

    fn is_agent(&self) -> bool {
        true
    }

    fn deliver(&self, feedback: &str) -> Result<(), DeliveryError> {
        let mut child =
            self.shell().stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(
                |err| DeliveryError::Unavailable(format!("cannot run the deliver command: {err}")),
            )?;
        let stdin = child.stdin.take();
        // Write from a second thread while this one drains stderr, so neither side can block
        // the other on a full pipe. A command that exits without reading closes the pipe; its
        // exit status still decides the outcome, so a write error is not the failure to report.
        let output = std::thread::scope(|scope| {
            if let Some(mut stdin) = stdin {
                scope.spawn(move || {
                    let _ = stdin.write_all(feedback.as_bytes());
                });
            }
            child.wait_with_output()
        })?;
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let message = |fallback: &str| if stderr.is_empty() { fallback.to_owned() } else { stderr.clone() };
        match output.status.code() {
            Some(0) => Ok(()),
            Some(EXIT_BLOCKED) => Err(DeliveryError::Blocked(message("the agent is at a dialog"))),
            Some(EXIT_UNAVAILABLE) => Err(DeliveryError::Unavailable(message("no agent to send to"))),
            code => Err(DeliveryError::Failed(anyhow::anyhow!(
                "the deliver command failed ({}): {}",
                code.map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
                message("no output")
            ))),
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, clippy::unwrap_used, reason = "tests assert by panicking")]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("plannotator-deliver-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_command_receives_the_feedback_verbatim_on_stdin() {
        let dir = temp_dir("stdin");
        let out = dir.join("got.md");
        let command = DeliverCommand::new(format!("cat > '{}'", out.display()), None);
        let feedback = "# Annotations on plan.md\n\n## Annotation 1 (line 2)\n> “quoted” · é\n";
        command.deliver(feedback).expect("delivered");
        assert_eq!(std::fs::read_to_string(out).expect("read"), feedback);
    }

    fn run(script: &str) -> Result<(), DeliveryError> {
        DeliverCommand::new(script.to_owned(), None).deliver("x")
    }

    #[test]
    fn exit_75_is_a_blocked_agent_with_the_commands_message() {
        let result = run("echo 'claude is asking a question' >&2; exit 75");
        assert!(matches!(result, Err(DeliveryError::Blocked(m)) if m == "claude is asking a question"));
    }

    #[test]
    fn exit_69_is_an_unavailable_agent() {
        assert!(matches!(run("exit 69"), Err(DeliveryError::Unavailable(_))));
    }

    #[test]
    fn any_other_failure_keeps_the_exit_status_and_stderr() {
        let err = run("echo boom >&2; exit 3").unwrap_err();
        assert!(
            matches!(&err, DeliveryError::Failed(e) if e.to_string().contains("exit 3") && e.to_string().contains("boom"))
        );
    }

    #[test]
    fn a_command_that_ignores_stdin_and_floods_stderr_neither_fails_nor_hangs() {
        let big = "a".repeat(1 << 20);
        let command = DeliverCommand::new("head -c 1048576 /dev/zero >&2".to_owned(), None);
        assert!(command.deliver(&big).is_ok());
    }

    #[test]
    fn a_blank_or_missing_command_is_not_configured() {
        assert!(DeliverCommand::from_lookup(|_| Some("  ".to_owned())).is_none());
        assert!(DeliverCommand::from_lookup(|_| None).is_none());
    }
}
