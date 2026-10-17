use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::server::{SshMcpServer, args::parse_routeros_tool_args};
use crate::ssh::{CommandOutput, RouterOsExecFailure};

impl SshMcpServer {
    pub(crate) async fn execute_routeros_tool(
        &self,
        arguments: &Map<String, Value>,
        cancellation: CancellationToken,
    ) -> std::result::Result<CallToolResult, McpError> {
        let parsed = parse_routeros_tool_args(arguments, self.max_chars)?;
        let timeout = self.resolve_timeout(parsed.timeout_ms);
        match self
            .connection
            .exec_routeros_command(parsed.command, timeout, cancellation)
            .await
        {
            Ok(mut output) => {
                append_truncation_notice(&mut output);
                Ok(Self::calltool_from_command_output(output))
            }
            Err(failure) => Ok(failure_result(failure)),
        }
    }
}

fn append_truncation_notice(output: &mut CommandOutput) {
    for (name, truncated) in [
        ("stdout", output.stdout_truncated),
        ("stderr", output.stderr_truncated),
    ] {
        if truncated {
            output.stdout.push_str(&format!(
                "\n[{name} truncated at the configured output limit]"
            ));
        }
    }
}

fn failure_result(mut failure: RouterOsExecFailure) -> CallToolResult {
    append_truncation_notice(&mut failure.output);
    let mut text = failure.output.stdout;
    if !failure.output.stderr.is_empty() {
        if !text.is_empty() {
            text.push_str("\n--- stderr ---\n");
        }
        text.push_str(&failure.output.stderr);
    }
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    if let Some(code) = failure.output.exit_code {
        text.push_str(&format!("SSH exit status received: {code}\n"));
    }
    text.push_str(&format!("Error: {}\n", failure.message));
    if failure.may_have_run {
        text.push_str("Command may have run; do not automatically retry. Channel closure does not guarantee remote termination or rollback; verify relevant RouterOS state.");
    } else {
        text.push_str("Command not started (EXEC was not queued or was explicitly rejected).");
    }
    CallToolResult::error(vec![ContentBlock::text(text)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_zero_status_never_overrides_failure() {
        let result = failure_result(RouterOsExecFailure {
            output: CommandOutput {
                stdout: "partial".to_string(),
                stderr: "diagnostic".to_string(),
                exit_code: Some(0),
                stdout_truncated: true,
                ..Default::default()
            },
            message: "deadline exceeded".to_string(),
            may_have_run: true,
        });
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].as_text().unwrap().text;
        for expected in [
            "partial",
            "diagnostic",
            "SSH exit status received: 0",
            "deadline exceeded",
            "may have run",
            "stdout truncated",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains("job_id"));
    }

    #[test]
    fn unqueued_failure_reports_not_started() {
        let result = failure_result(RouterOsExecFailure {
            output: CommandOutput::default(),
            message: "cancelled".to_string(),
            may_have_run: false,
        });
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("Command not started")
        );
    }
}
