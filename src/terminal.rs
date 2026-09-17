//! Terminal adapters: the run's observer and input broker when the host is
//! a person at a shell.

use std::io::IsTerminal;

use promptforge_api_runtime::input::{InputBroker, InputError, InputOutcome};
use promptforge_api_runtime::types::observe::{Observation, Observer};
use tokio::io::AsyncBufReadExt;

/// Prints each lifecycle observation to stderr.
#[derive(Debug)]
pub(crate) struct StderrObserver;

impl Observer for StderrObserver {
    fn observe(&self, execution: &str, section: &str, event: Observation) {
        eprintln!("[{execution}] {section}: {event}");
    }
}

/// Answers `user_input()` from the terminal: one line per request. Without
/// a terminal on stdin the run is told input is unavailable.
#[derive(Debug)]
pub(crate) struct StdinBroker;

#[async_trait::async_trait]
impl InputBroker for StdinBroker {
    async fn user_input(
        &self,
        _execution: &str,
        section: &str,
    ) -> Result<InputOutcome, InputError> {
        if !std::io::stdin().is_terminal() {
            return Ok(InputOutcome::Unavailable);
        }
        eprint!("[{section}] > ");
        let mut line = String::new();
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        match reader.read_line(&mut line).await {
            Ok(0) => Ok(InputOutcome::Unavailable),
            Ok(_) => Ok(InputOutcome::Text(trim_line_ending(&line).to_owned())),
            Err(error) => Err(InputError::with_source("reading stdin failed", error)),
        }
    }
}

/// Strips one trailing line ending (`\n` or `\r\n`) from a read line.
fn trim_line_ending(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_are_trimmed_once() {
        assert_eq!(trim_line_ending("hi\n"), "hi");
        assert_eq!(trim_line_ending("hi\r\n"), "hi");
        assert_eq!(trim_line_ending("hi"), "hi");
        assert_eq!(trim_line_ending("hi\n\n"), "hi\n");
    }
}
