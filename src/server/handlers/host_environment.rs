use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use tokio_util::sync::CancellationToken;

use crate::server::SshMcpServer;

impl SshMcpServer {
    pub(in crate::server) async fn execute_host_environment(
        &self,
        refresh: bool,
        cancellation: CancellationToken,
    ) -> std::result::Result<CallToolResult, McpError> {
        match self
            .connection
            .host_environment(refresh, cancellation)
            .await
        {
            Ok(snapshot) => {
                let value = serde_json::to_value(snapshot)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                Ok(CallToolResult::structured(value))
            }
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Error collecting host environment: {error}"
            ))])),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ssh::environment::HostEnvironment;
    use rmcp::model::CallToolResult;

    #[test]
    fn snapshot_wire_content_is_compact_deterministic_and_compatible() {
        let value = serde_json::to_value(HostEnvironment::default()).unwrap();
        let first = CallToolResult::structured(value.clone());
        let second = CallToolResult::structured(value.clone());
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );
        assert_eq!(first.content.len(), 1);
        let text = &first.content[0].as_text().unwrap().text;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(text).unwrap(),
            value
        );
        assert_eq!(first.structured_content, Some(value));
        assert!(!text.contains('\n'));
        assert_eq!(first.is_error, Some(false));
    }
}
