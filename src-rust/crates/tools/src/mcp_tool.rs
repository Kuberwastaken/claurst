// MCP tool wrapper: makes the tools of connected MCP servers look like native
// tools, so every front end (TUI, headless, ACP) registers them the same way.

use std::sync::Arc;

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use claurst_core::types::ToolDefinition;
use serde_json::Value;

/// One tool exposed by a connected MCP server.
pub struct McpToolWrapper {
    /// Definition under the exposed `mcp__<server>__<tool>` name; the manager
    /// routes calls by that name.
    tool_def: ToolDefinition,
    manager: Arc<claurst_mcp::McpManager>,
}

/// Wrap every tool of every connected server in `manager`.
pub fn mcp_tools(manager: &Arc<claurst_mcp::McpManager>) -> Vec<Box<dyn Tool>> {
    manager
        .all_tool_definitions()
        .into_iter()
        .map(|entry| {
            Box::new(McpToolWrapper {
                tool_def: entry.definition,
                manager: manager.clone(),
            }) as Box<dyn Tool>
        })
        .collect()
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.tool_def.name
    }

    fn description(&self) -> &str {
        &self.tool_def.description
    }

    fn permission_level(&self) -> PermissionLevel {
        // MCP tools run external processes – treat as Execute.
        PermissionLevel::Execute
    }

    // Gates itself: calls `ctx.check_permission` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        self.tool_def.input_schema.clone()
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        // Same shape as the central backstop's description, so the approval
        // prompt shows what the call will send.
        let rendered = serde_json::to_string(&input).unwrap_or_default();
        let preview: String = rendered.chars().take(200).collect();
        let desc = if preview.is_empty() || preview == "{}" || preview == "null" {
            format!("Run MCP tool '{}'", self.tool_def.name)
        } else {
            format!("Run MCP tool '{}' with input: {}", self.tool_def.name, preview)
        };
        if let Err(e) = ctx.check_permission(self.name(), &desc, false) {
            return ToolResult::error(e.to_string());
        }

        let args = if input.is_null() { None } else { Some(input) };

        match self.manager.call_tool(&self.tool_def.name, args).await {
            Ok(result) => {
                let output = claurst_mcp::mcp_result_to_output(&result);
                if result.is_error {
                    ToolResult::error(output.text)
                } else {
                    ToolResult::success(output.text).with_images(output.images)
                }
            }
            Err(e) => ToolResult::error(format!("MCP tool '{}' failed: {}", self.tool_def.name, e)),
        }
    }
}
