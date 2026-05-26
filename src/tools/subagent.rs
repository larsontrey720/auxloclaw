use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::coordination::{AgentCoordinator, SubAgentConfig, TaskDelegator};
use crate::memory::model_store::ModelStore;
use crate::orchestrator::{Tool, ToolResult};

pub struct DelegateToSubAgentTool {
    coordinator: Arc<RwLock<Option<Arc<AgentCoordinator>>>>,
    model_store: Arc<ModelStore>,
    current_context: Arc<parking_lot::RwLock<(Option<String>, Option<String>)>>,
}

impl DelegateToSubAgentTool {
    pub fn new(
        coordinator: Arc<RwLock<Option<Arc<AgentCoordinator>>>>,
        model_store: Arc<ModelStore>,
        current_context: Arc<parking_lot::RwLock<(Option<String>, Option<String>)>>,
    ) -> Self {
        Self {
            coordinator,
            model_store,
            current_context,
        }
    }
}

#[async_trait]
impl Tool for DelegateToSubAgentTool {
    fn name(&self) -> &str {
        "delegate_to_subagent"
    }

    fn description(&self) -> &str {
        "Delegate a task to a specialized sub-agent with isolated context. \
         The sub-agent runs independently with its own conversation, tools, and persona, \
         then returns the result. Use this to parallelize research, offload focused coding tasks, \
         or isolate complex analysis from the main conversation."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task description for the sub-agent"
                },
                "agent_type": {
                    "type": "string",
                    "enum": ["researcher", "coder", "analyst", "planner", "reviewer", "general"],
                    "description": "Specialist type. Auto-detected if omitted."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 120)",
                    "minimum": 10,
                    "maximum": 600
                },
                "max_tools": {
                    "type": "integer",
                    "description": "Max tool calls the sub-agent can make (default: 15)",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let task = match args.get("task").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => {
                return Ok(ToolResult {
                    tool_name: "delegate_to_subagent".into(),
                    success: false,
                    output: json!({"error": "Missing required parameter: task"}),
                    error: Some("Missing required parameter: task".into()),
                    duration_ms: 0,
                });
            }
        };

        let agent_type = args
            .get("agent_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);

        let max_tools = args
            .get("max_tools")
            .and_then(|v| v.as_u64())
            .unwrap_or(15) as u32;

        let coordinator_guard = self.coordinator.read().await;
        let coordinator = match coordinator_guard.as_ref() {
            Some(c) => c.clone(),
            None => {
                return Ok(ToolResult {
                    tool_name: "delegate_to_subagent".into(),
                    success: false,
                    output: json!({"error": "Sub-agent system not initialized"}),
                    error: Some("Sub-agent system not initialized".into()),
                    duration_ms: 0,
                });
            }
        };
        drop(coordinator_guard);

        let resolved_type = agent_type.unwrap_or_else(|| {
            let delegator = TaskDelegator::new();
            delegator.classify_task(&task)
        });

        // Read current user context (set by the agent per-request)
        let (override_model, override_base_url, override_api_key) = {
            let (channel, user_id) = {
                let ctx = self.current_context.read();
                (ctx.0.clone(), ctx.1.clone())
            };
            match (channel, user_id) {
                (Some(ch), Some(uid)) => {
                    match self.model_store.get(&ch, &uid) {
                        Ok(Some(ov)) => {
                            let api_key = ov.encrypted_api_key.as_ref().and_then(|enc| {
                                self.model_store.decrypt_key(enc).ok()
                            });
                            tracing::info!(
                                "Sub-agent inheriting model override: model={:?}, base_url={:?}, has_key={}",
                                ov.model_id, ov.base_url, api_key.is_some()
                            );
                            (ov.model_id, ov.base_url, api_key)
                        }
                        Ok(None) => (None, None, None),
                        Err(e) => {
                            tracing::warn!("Failed to read model override for sub-agent: {}", e);
                            (None, None, None)
                        }
                    }
                }
                _ => (None, None, None),
            }
        };

        let config = SubAgentConfig {
            agent_type: resolved_type.clone(),
            task: task.clone(),
            isolated_context: true,
            timeout_secs,
            max_tools,
            override_model,
            override_base_url,
            override_api_key,
        };

        tracing::info!(
            "Delegating task to sub-agent (type={}, timeout={}s, max_tools={})",
            resolved_type, timeout_secs, max_tools
        );

        match coordinator.spawn_sub_agent(config).await {
            Ok(sub_agent) => match sub_agent.execute().await {
                Ok(result) => {
                    tracing::info!(
                        "Sub-agent {} completed: {} tools, {} iterations, success={}",
                        result.agent_id,
                        result.tools_used.len(),
                        result.iterations,
                        result.success
                    );
                    Ok(ToolResult {
                        tool_name: "delegate_to_subagent".into(),
                        success: result.success,
                        output: json!({
                            "agent_id": result.agent_id,
                            "response": result.response,
                            "tools_used": result.tools_used,
                            "iterations": result.iterations,
                            "success": result.success,
                        }),
                        error: result.error,
                        duration_ms: 0,
                    })
                }
                Err(e) => {
                    tracing::warn!("Sub-agent execution failed: {}", e);
                    Ok(ToolResult {
                        tool_name: "delegate_to_subagent".into(),
                        success: false,
                        output: json!({"error": format!("Sub-agent execution failed: {}", e)}),
                        error: Some(e.to_string()),
                        duration_ms: 0,
                    })
                }
            },
            Err(e) => {
                tracing::warn!("Failed to spawn sub-agent: {}", e);
                Ok(ToolResult {
                    tool_name: "delegate_to_subagent".into(),
                    success: false,
                    output: json!({"error": format!("Failed to spawn sub-agent: {}", e)}),
                    error: Some(e.to_string()),
                    duration_ms: 0,
                })
            }
        }
    }
}
