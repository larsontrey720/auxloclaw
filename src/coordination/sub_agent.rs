//! Sub-agent with isolated context
//! Executes specialized tasks independently

use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::providers::{CompletionRequest, Message, ProviderPool, ToolCall};
use crate::orchestrator::ToolOrchestrator;
use crate::memory::{SessionHistory, SessionStore};
use crate::persona::PersonaConfig;
use crate::persona::SystemPromptBuilder;
use crate::error_recovery::{AgentError, RecoveryAction};

/// Sub-agent configuration
#[derive(Debug, Clone)]
pub struct SubAgentConfig {
    /// Type of agent (researcher, coder, analyst, etc.)
    pub agent_type: String,
    /// Task description
    pub task: String,
    /// Isolate context from main agent
    pub isolated_context: bool,
    /// Timeout in seconds
    pub timeout_secs: u64,
    /// Max tool calls allowed
    pub max_tools: u32,
    /// Override model (inherits from user's ModelStore if set)
    pub override_model: Option<String>,
    /// Override base URL (inherits from user's ModelStore if set)
    pub override_base_url: Option<String>,
    /// Override API key (inherits from user's ModelStore if set)
    pub override_api_key: Option<String>,
}

/// Sub-agent result
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    pub agent_id: String,
    pub task: String,
    pub response: String,
    pub tools_used: Vec<String>,
    pub iterations: u32,
    pub success: bool,
    pub error: Option<String>,
}

impl SubAgentResult {
    pub fn summary(&self) -> String {
        if self.success {
            format!(
                "Sub-agent {} completed '{}' using {} tools in {} iterations",
                self.agent_id, self.task, self.tools_used.len(), self.iterations
            )
        } else {
            format!(
                "Sub-agent {} failed on '{}': {}",
                self.agent_id,
                self.task,
                self.error.as_deref().unwrap_or("Unknown error")
            )
        }
    }
}

/// Predefined sub-agent personas
fn get_agent_persona(agent_type: &str) -> PersonaConfig {
    let behavior = match agent_type {
        "researcher" => "You are a research specialist. Focus on gathering information, analyzing sources, and synthesizing findings. Be thorough and cite sources.",
        "coder" => "You are a code specialist. Focus on implementing, debugging, and optimizing code. Follow best practices and write clean, maintainable code.",
        "analyst" => "You are a data analyst. Focus on interpreting data, identifying patterns, and providing actionable insights. Present findings clearly.",
        "planner" => "You are a planning specialist. Focus on breaking down complex tasks into steps, identifying dependencies, and creating actionable plans.",
        "reviewer" => "You are a review specialist. Focus on quality assurance, identifying issues, and suggesting improvements. Be constructive and specific.",
        _ => "You are a specialized sub-agent. Focus on your assigned task and provide clear, actionable results.",
    };

    PersonaConfig {
        name: format!("{}_agent", agent_type),
        behavior: behavior.to_string(),
        style: crate::persona::StyleConfig {
            length: crate::persona::ResponseLength::Balanced,
            tone: crate::persona::Tone::Professional,
            formatting: crate::persona::FormattingConfig::default(),
        },
        persona_file: None,
    }
}

/// Sub-agent with isolated context
pub struct SubAgent {
    id: String,
    config: SubAgentConfig,
    providers: Arc<ProviderPool>,
    orchestrator: Arc<ToolOrchestrator>,
    session_store: Arc<SessionStore>,
    isolated_session: RwLock<SessionHistory>,
    persona: PersonaConfig,
    default_model: String,
    terminated: RwLock<bool>,
}

impl SubAgent {
    pub fn new(
        config: SubAgentConfig,
        providers: Arc<ProviderPool>,
        orchestrator: Arc<ToolOrchestrator>,
        session_store: Arc<SessionStore>,
        app_config: AppConfig,
    ) -> Result<Self> {
        let id = format!("{}_{}", config.agent_type, Uuid::new_v4());
        let persona = get_agent_persona(&config.agent_type);
        let isolated_session = RwLock::new(SessionHistory::new(&id));

        Ok(Self {
            id,
            config,
            providers,
            orchestrator,
            session_store,
            isolated_session,
            persona,
            default_model: app_config.agent.default_model,
            terminated: RwLock::new(false),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Execute the sub-agent task
    pub async fn execute(&self) -> Result<SubAgentResult> {
        let result = timeout(
            Duration::from_secs(self.config.timeout_secs),
            self.execute_inner(),
        )
        .await
        .map_err(|_| AgentError::TimeoutError {
            operation: format!("Sub-agent {} execution", self.id),
            duration_secs: self.config.timeout_secs,
        })??;

        Ok(result)
    }

    async fn execute_inner(&self) -> Result<SubAgentResult> {
        let mut tools_used = Vec::new();
        let mut iterations = 0;
        let mut response = String::new();

        // Build system prompt with sub-agent context
        let system_prompt = SystemPromptBuilder::new(self.persona.clone())
            .with_tools(&self.orchestrator.get_definitions())
            .with_skills(&[])
            .build();

        let mut messages = vec![Message::new("system", system_prompt)];

        messages.push(Message::new("user", &self.config.task));

        loop {
            if *self.terminated.read().await {
                return Ok(SubAgentResult {
                    agent_id: self.id.clone(),
                    task: self.config.task.clone(),
                    response: "Terminated by coordinator".to_string(),
                    tools_used,
                    iterations,
                    success: false,
                    error: Some("Terminated".to_string()),
                });
            }

            iterations += 1;
            if iterations > self.config.max_tools {
                response = "Max iterations reached".to_string();
                break;
            }

            let request = CompletionRequest {
                model: self.config.override_model.clone().unwrap_or_else(|| self.default_model.clone()),
                messages: messages.clone(),
                temperature: Some(0.7),
                max_tokens: Some(4096),
                tools: Some(self.orchestrator.get_definitions().into_iter().map(|t| {
                    crate::providers::ToolDefinition {
                        tool_type: t.tool_type,
                        function: crate::providers::FunctionDefinition {
                            name: t.function.name,
                            description: t.function.description,
                            parameters: t.function.parameters,
                        },
                    }
                }).collect()),
                stream: None,
                base_url: self.config.override_base_url.clone(),
                api_key: self.config.override_api_key.clone(),
            };

            let completion = self.providers.complete(request).await?;

            if let Some(tool_calls) = &completion.tool_calls {
                if !tool_calls.is_empty() {
                    messages.push(Message::with_tool_calls("assistant", tool_calls.clone()));

                    for tc in tool_calls {
                        tools_used.push(tc.function.name.clone());
                        let args: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);

                        let result = self.orchestrator.execute_tool(&tc.function.name, args).await;
                        let result_str = if result.success {
                            serde_json::to_string(&result.output).unwrap_or_else(|_| result.output.to_string())
                        } else {
                            format!("Error: {}", result.error.unwrap_or_default())
                        };

                        messages.push(Message::tool_result(tc.id.clone(), &tc.function.name, &result_str));
                    }
                    continue;
                }
            }

            response = completion.content;
            break;
        }

        // Persist isolated session
        {
            let mut session = self.isolated_session.write().await;
            session.add_message("user", &self.config.task, None);
            session.add_message("assistant", &response, None);
        }

        let session = self.isolated_session.read().await.clone();
        let _ = self.session_store.save(&self.id, &session);

        Ok(SubAgentResult {
            agent_id: self.id.clone(),
            task: self.config.task.clone(),
            response: response.clone(),
            tools_used,
            iterations,
            success: true,
            error: None,
        })
    }

    /// Terminate the sub-agent
    pub async fn terminate(&self) {
        *self.terminated.write().await = true;
        tracing::info!("Sub-agent {} terminated", self.id);
    }

    /// Get session history
    pub async fn get_history(&self) -> Vec<crate::memory::HistoryMessage> {
        self.isolated_session.read().await.messages.clone()
    }
}
