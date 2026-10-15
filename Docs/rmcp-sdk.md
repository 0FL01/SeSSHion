# rmcp - Official Rust MCP SDK (v3.5.1)

> Source: https://docs.rs/rmcp/3.5.1/rmcp/

## Overview

The official Rust SDK for the Model Context Protocol (MCP).

rmcp allows building MCP **servers** and **clients** in Rust.

## SeSSHion Integration

SeSSHion uses `server` and `transport-io` with the default SDK features.
The manual `ServerHandler::call_tool` implementation returns `CallToolResponse`;
internal handlers still return `CallToolResult`, converted at the dispatcher boundary.
Tool failures remain completed results with `isError: true`, not JSON-RPC errors.

`get_info` returns `ServerConfig` and uses `ProtocolVersion::LATEST_WITH_INITIALIZE`
(`2025-11-25`) as the fallback for clients using `initialize`.
`ProtocolVersion::LATEST` is MCP `2026-07-28`; the SDK also supports its discovery
and per-request metadata without an application compatibility layer.
For modern cacheable results, including the manual `tools/list` handler, the SDK
fills missing hints with `ttlMs: 0` and `cacheScope: "private"` before serialization.
This does not enable application caching; legacy responses remain unchanged.
The CLI gates authenticated SSH readiness before starting the stdio service, so
cold failures also reject modern opening RPCs without `initialize`. Once ready,
SSH recovery does not restart MCP or repeat initialization.
Background jobs remain SeSSHion's `job_id` / `check_process` contract; upgrading
the SDK does not enable MCP Tasks, MRTR, subscriptions, caching, or HTTP transport.

## Server Implementation

A server exposes capabilities (tools) to MCP clients like Claude Desktop or Cursor IDE.

### #[tool_router] macro example

```rust
use std::sync::Arc;
use rmcp::{
    ErrorData as McpError, model::*, tool, tool_router,
    handler::server::tool::ToolRouter
};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Counter {
    counter: Arc<Mutex<i32>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Counter {
    fn new() -> Self {
        Self {
            counter: Arc::new(Mutex::new(0)),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Increment the counter by 1")]
    async fn increment(&self) -> Result<CallToolResult, McpError> {
        let mut counter = self.counter.lock().await;
        *counter += 1;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            counter.to_string(),
        )]))
    }
}
```

### Structured Output

Tools can return structured JSON with schemas using the `Json` wrapper:

```rust
use schemars::JsonSchema;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct CalculationRequest {
    a: i32,
    b: i32,
    operation: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct CalculationResult {
    result: i32,
    operation: String,
}

#[tool(name = "calculate", description = "Perform a calculation")]
async fn calculate(
    &self,
    params: Parameters<CalculationRequest>
) -> Result<Json<CalculationResult>, String> {
    let result = match params.0.operation.as_str() {
        "add" => params.0.a + params.0.b,
        "multiply" => params.0.a * params.0.b,
        _ => return Err("Unknown operation".to_string()),
    };
    Ok(Json(CalculationResult { result, operation: params.0.operation }))
}
```

## Key Types

| Type | Description |
|------|-------------|
| `ServerHandler` | Trait to implement for server types |
| `ToolRouter<T>` | Router for tool dispatch |
| `CallToolResult` | Completed tool result, including tool errors |
| `CallToolResponse` | Server handler response encompassing completed, input-required, or task results |
| `ContentBlock::text()` | Create text content response |
| `ErrorData` | MCP error representation |

## Modules

- `handler` - Server/client handlers
- `model` - MCP protocol models
- `service` - Service layer (`ServiceExt`, `Peer`)
- `transport` - Transport implementations

## Server Startup

Implement `ServerHandler` for your server type, then call `.serve(...)` with a transport:

```rust
use rmcp::service::ServiceExt;
use rmcp::transport::stdio;

async fn main() {
    let server = MyServer::new();
    server.serve(stdio()).await.unwrap();
}
```

## Re-exports

- `schemars` - JSON Schema generation
- `serde` - Serialization
- `serde_json` - JSON handling
