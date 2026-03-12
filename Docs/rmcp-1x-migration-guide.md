# Upgrading to RMCP 1.x — Migration Guide
After PR #715, PR #720, PR #739 land, most public model structs become `#[non_exhaustive]` and gain builder-style constructors. Direct struct-literal construction will no longer compile. This guide covers every breaking change and how to fix it.
## TL;DR
| What changed | Old pattern | New pattern |
|--------------|-------------|-------------|
| Struct construction | `Foo { field_a, field_b, .. }` | `Foo::new(required_fields).with_optional(val)` |
| `#[non_exhaustive]` on structs | Could use `..Default::default()` | Must use `::new()` + `.with_*()` builders |
| `#[non_exhaustive]` on enums/errors | Exhaustive match | Add `_ => {}` catch-all arm |
| `CreateMessageResult::with_stop_reason` | `with_stop_reason(Option<String>)` | `with_stop_reason(impl Into<String>)` |
| `LoggingMessageNotificationParam::with_logger` | static `with_logger(level, logger, data)` | instance `.with_logger(logger)` chained after `::new()` |
| `CreateElicitationResult::with_content` | static `with_content(action, val)` | instance `.with_content(val)` chained after `::new()` |
| `CallToolRequestParams::with_task` | `with_task(Option<JsonObject>)` | `with_task(JsonObject)` |
## 1. ServerInfo / InitializeResult
**Before:**
```rust
ServerInfo {
    instructions: Some("A calculator".into()),
    capabilities: ServerCapabilities::builder().enable_tools().build(),
    ..Default::default()
}
```
**After:**
```rust
ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    .with_instructions("A calculator")
```
Other builders: `.with_server_info(impl)`, `.with_protocol_version(ver)`.
## 2. CallToolRequestParams
**Before:**
```rust
CallToolRequestParams {
    meta: None,
    name: "my_tool".into(),
    arguments: Some(serde_json::json!({"a": 1}).as_object().cloned().unwrap()),
    task: None,
}
```
**After:**
```rust
CallToolRequestParams::new("my_tool")
    .with_arguments(serde_json::json!({"a": 1}).as_object().cloned().unwrap())
```
## 3. GetPromptRequestParams
**Before:**
```rust
GetPromptRequestParams {
    meta: None,
    name: "my_prompt".into(),
    arguments: Some(args),
}
```
**After:**
```rust
GetPromptRequestParams::new("my_prompt")
    .with_arguments(args)
```
## 4. GetPromptResult
**Before:**
```rust
GetPromptResult {
    description: Some("Help text".to_string()),
    messages: vec![...],
}
```
**After:**
```rust
GetPromptResult::new(vec![...])
    .with_description("Help text")
```
## 5. InitializeRequestParams / ClientInfo
**Before:**
```rust
InitializeRequestParams {
    meta: None,
    protocol_version: ProtocolVersion::LATEST,
    capabilities: ClientCapabilities { elicitation: Some(...), ..Default::default() },
    client_info: Implementation {
        name: "my-client".to_string(),
        version: "1.0.0".to_string(),
        title: None,
        description: None,
        website_url: None,
        icons: None,
    },
}
```
**After:**
```rust
InitializeRequestParams::new(
    ClientCapabilities::builder()
        .enable_elicitation_with(elicitation_cap)
        .build(),
    Implementation::new("my-client", "1.0.0"),
)
```
## 6. Implementation
**Before:**
```rust
Implementation {
    name: "my-server".to_string(),
    version: "0.1.0".to_string(),
    title: None, description: None, website_url: None, icons: None,
}
```
**After:**
```rust
Implementation::new("my-server", "0.1.0")
```
## 7. CreateMessageResult
**Before:**
```rust
CreateMessageResult {
    message: SamplingMessage::assistant_text("Hello"),
    model: "test-model".to_string(),
    stop_reason: Some("endTurn".to_string()),
}
```
**After:**
```rust
CreateMessageResult::new(
    SamplingMessage::assistant_text("Hello"),
    "test-model".to_string(),
)
.with_stop_reason("endTurn")
```
## 8. CreateMessageRequestParams
**Before:**
```rust
CreateMessageRequestParams {
    meta: None, task: None,
    messages: vec![...],
    model_preferences: None,
    system_prompt: Some("You are helpful".into()),
    include_context: None,
    temperature: Some(0.7),
    max_tokens: 1024,
    stop_sequences: None, metadata: None, tools: None, tool_choice: None,
}
```
**After:**
```rust
CreateMessageRequestParams::new(vec![...], 1024)
    .with_system_prompt("You are helpful")
    .with_temperature(0.7)
```
## 9. Tool (manual construction)
**Before:**
```rust
Tool {
    name: "my_tool".into(),
    title: None,
    description: Some("Does stuff".into()),
    input_schema: schema.into(),
    output_schema: None,
    annotations: None,
    execution: None,
    icons: None,
    meta: None,
}
```
**After:**
```rust
Tool::new_with_raw("my_tool", Some("Does stuff".into()), schema)
```
Optional chaining: `.with_title(t)`, `.with_annotations(a)`, `.with_execution(e)`, `.with_icons(i)`, `.with_meta(m)`, `.with_raw_output_schema(s)`.
## 10. ToolAnnotations
**Before:**
```rust
Some(ToolAnnotations {
    title: Some("My Tool".into()),
    read_only_hint: Some(true),
    destructive_hint: None,
    idempotent_hint: None,
    open_world_hint: None,
})
```
**After:**
```rust
Some(ToolAnnotations::from_raw(
    Some("My Tool".into()),
    Some(true),  // read_only_hint
    None,        // destructive_hint
    None,        // idempotent_hint
    None,        // open_world_hint
))
```
Or use the existing static builder — note `with_title` is a static function, not an instance method:
```rust
// Correct
ToolAnnotations::with_title("My Tool").read_only(true)
// Wrong — does not compile
ToolAnnotations::new().with_title("My Tool")
```
## 11. Task
**Before:**
```rust
Task {
    task_id, status: TaskStatus::Working,
    status_message: Some("Accepted".into()),
    created_at: ts.clone(), last_updated_at: ts,
    ttl: None, poll_interval: None,
}
```
**After:**
```rust
Task::new(task_id, TaskStatus::Working, ts.clone(), ts)
    .with_status_message("Accepted")
```
## 12. Other Structs with New Constructors
| Struct | Constructor |
|--------|-------------|
| `ReadResourceRequestParams` | `::new(uri)` |
| `ReadResourceResult` | `::new(contents)` |
| `SubscribeRequestParams` | `::new(uri)` |
| `SetLevelRequestParams` | `::new(level)` |
| `ProgressNotificationParam` | `::new(token, progress)` |
| `CompleteRequestParams` | `::new(ref, argument).with_context(ctx)` |
| `CompleteResult` | `::new(completion)` |
| `Icon` | `::new(src).with_mime_type(m).with_sizes(s)` |
| `Prompt` | `::new(name, Some(desc), args).with_title(t)` or `::from_raw(name, Some(desc), args)` |
| `PromptArgument` | `::new(name).with_description(d).with_required(b)` |
| `PromptMessage` | `::new(role, content)` |
| `ModelPreferences` | `::new().with_hints(h).with_cost_priority(f)` |
| `ModelHint` | `::new(name)` |
| `CreateElicitationResult` | `::new(action).with_content(val)` |
| `ListTasksResult` | `::new(tasks)` — see warning below |
| `CreateTaskResult` | `::new(task)` |
| `GetTaskPayloadResult` | `::new(value)` |
| `JsonRpcRequest` | `::new(id, request)` |
| `JsonRpcError` | `::new(id, error)` |
| `RequestContext` | `::new(id, peer)` |
| `RawEmbeddedResource` | `::new(resource)` |
| `ConstTitle` | `::new(const_, title)` |
| `TitledSingleSelectEnumSchema` | `::new(one_of)` |
| `TitledMultiSelectEnumSchema` | `::new(items).with_min_items(n)...` |
| `TitledItems` | `::new(any_of)` |
| `ResourceUpdatedNotificationParam` | `::new(uri)` |
| `LoggingMessageNotificationParam` | `::new(level, data).with_logger(logger)` |
| `ElicitationResponseNotificationParam` | `::new(elicitation_id)` |
| `UnsubscribeRequestParams` | `::new(uri)` |
| `PromptReference` | `::new(name).with_title(t)` |
| `Root` | `::new(uri).with_name(n)` |
| `ListRootsResult` | `::new(roots)` |
> ⚠️ **ListTasksResult gap:** `::new(tasks)` only sets tasks; `next_cursor` and `total` are always initialized to `None`. PR #715 adds no `.with_next_cursor()` or `.with_total()` builders. Since `#[non_exhaustive]` blocks struct literals, there is currently no way to set these fields from outside the crate. If you relied on them, you'll need to wait for builder methods to be added or file an issue.
## 13. #[non_exhaustive] Enums & Errors
Several error enums and a few other enums are now `#[non_exhaustive]`:
- `RmcpError`
- `ClientInitializeError`
- `ServerInitializeError`
- `ElicitationError`
- `AuthError`
- `StreamableHttpError`
- `StreamableHttpProtocolError`
- `StreamableHttpPostResponse`
- `WorkerQuitReason`
- `SessionQuitReason`
- `LocalSessionWorkerError`
- `QuitReason`
- `ElicitationAction` (add `_ => {}` in match arms)
**Fix:** Add a wildcard arm to any match on these types:
```rust
match err {
    RmcpError::Service(e) => { /* ... */ }
    // ... other variants ...
    _ => { /* handle unknown future variants */ }
}
```
## 14. Notification Construction
Notifications now have a `::new(params)` constructor instead of struct literals:
**Before:**
```rust
ElicitationCompletionNotification {
    method: ElicitationCompletionNotificationMethod,
    params: notification_params,
    extensions: Default::default(),
}
```
**After:**
```rust
ElicitationCompletionNotification::new(notification_params)
```
Same pattern applies to `CreateElicitationRequest::new(params)`.
## 15. ClientCapabilities Builder
Elicitation capabilities now go through the builder:
**Before:**
```rust
ClientCapabilities {
    elicitation: Some(ElicitationCapability { ... }),
    ..Default::default()
}
```
**After:**
```rust
ClientCapabilities::builder()
    .enable_elicitation_with(ElicitationCapability { ... })
    .build()
```
## Quick Migration Checklist
- [ ] Search for struct-literal construction of any type listed above → replace with `::new()` + `.with_*()`
- [ ] Search for `..Default::default()` on affected types → replace with builder
- [ ] Search for `match` on any newly `#[non_exhaustive]` enum → add `_ => {}` arm
- [ ] Check `ToolAnnotations::with_title` call sites → it's a static function, not `new().with_title(...)`
- [ ] Check `Prompt::new` call sites → it takes 3 args: `new(name, description, arguments)`
- [ ] Check `ListTasksResult` usage → `::new(tasks)` cannot set `next_cursor` or `total` (no builders exist yet)
- [ ] Run `cargo build` and follow remaining compiler errors — the compiler is your friend here
