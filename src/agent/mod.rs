//! Agent Core - Central processing unit for the agent framework

pub mod history;

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

use crate::capabilities::CapabilityManifest;
use crate::checkpoints::CheckpointManager;
use crate::config::AppConfig;
use crate::context::{build_pruned_messages, truncate_for_summary};
use crate::memory::{
    CodeModeStore,
    CompactionResult, Compactor, HistoryMessage, MemoryEngine, Reflection, Reflector,
    ReflectorConfig, SessionHistory, SessionStore,
};
use crate::orchestrator::ToolOrchestrator;
use crate::persona::{shared::load_current_persona, PersonaConfig};
use crate::plugins::{HookEvent, PluginManager};
use crate::providers::{CompletionRequest, Message, ProviderPool, ToolCall};
use crate::skills::{ExtractorConfig, SkillExtractor, ToolTraceEntry};
use regex::Regex;
use tokio::sync::mpsc;
use dirs;

/// Usage statistics
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub total_messages: u64,
    pub total_tokens: u64,
}

/// Mid-loop intervention: a user message injected while the agent is running.
#[derive(Debug, Clone)]
pub struct Intervention {
    pub message: String,
}

/// Global registry of active intervention channels per session.
/// Channels send interventions into the running agent loop.
pub struct InterventionRegistry {
    senders: RwLock<HashMap<String, mpsc::Sender<Intervention>>>,
}

impl InterventionRegistry {
    pub fn new() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
        }
    }

    /// Register a session as actively running (call at loop start).
    pub async fn register(&self, session_key: &str) -> mpsc::Receiver<Intervention> {
        let (tx, rx) = mpsc::channel(16);
        let mut senders = self.senders.write().await;
        senders.insert(session_key.to_string(), tx);
        rx
    }

    /// Unregister when loop ends.
    pub async fn unregister(&self, session_key: &str) {
        let mut senders = self.senders.write().await;
        senders.remove(session_key);
    }

    /// Inject a message into a running agent loop. Returns false if session is not active.
    pub async fn inject(&self, session_key: &str, message: String) -> bool {
        let senders = self.senders.read().await;
        if let Some(tx) = senders.get(session_key) {
            tx.send(Intervention { message }).await.is_ok()
        } else {
            false
        }
    }

    /// Check if a session currently has an active loop.
    pub async fn is_active(&self, session_key: &str) -> bool {
        let senders = self.senders.read().await;
        senders.contains_key(session_key)
    }
}

/// Tool info
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

/// Build a strict nudge message injected when the agent has made too many
/// tool calls without updating the user.
pub(crate) fn build_nudge_message(tool_call_count: u32) -> String {
    format!(
        "[NUDGE] You have made {} tool calls without updating the user. \
         You MUST now call the `send_message` tool to report your current \
         progress to the user. Be concise — state what you have done so far \
         and what remains. Then continue working on the task until it is \
         complete. Do NOT stop after the message — keep going.",
        tool_call_count
    )
}

/// Core agent state
pub struct AgentCore {
    config: AppConfig,
    memory: Arc<MemoryEngine>,
    providers: Arc<ProviderPool>,
    orchestrator: Arc<ToolOrchestrator>,
    persona: PersonaConfig,
    usage: std::sync::atomic::AtomicU64,
    pub sessions: RwLock<HashMap<String, SessionHistory>>,
    session_store: Arc<SessionStore>,
    code_mode: Arc<CodeModeStore>,
    model_store: Arc<crate::memory::model_store::ModelStore>,
    compactor: Arc<Compactor>,
    reflector: Arc<Reflector>,
    plugins: Arc<PluginManager>,
    checkpoint_manager: Arc<CheckpointManager>,
    extractor: Arc<SkillExtractor>,
    /// Intervention registry for mid-loop message injection
    pub intervention_registry: Arc<InterventionRegistry>,
    /// Last activity timestamp per session (epoch seconds)
    last_activity: RwLock<HashMap<String, u64>>,
    /// Channel name of the current request (set per-request)
    current_channel: parking_lot::RwLock<Option<String>>,
    /// User ID of the current request (set per-request)
    current_user_id: parking_lot::RwLock<Option<String>>,
    /// Shared context for sub-agent tool: (channel, user_id), updated per-request
    subagent_context: Arc<parking_lot::RwLock<(Option<String>, Option<String>)>>,
    /// When true, skip loading persona from disk and use config.persona directly.
    /// Used by /code mode to enforce the coding agent persona.
    override_system_prompt: Arc<RwLock<Option<String>>>,
}

impl AgentCore {
    pub fn new(
        // This is a hack to avoid modifying the whole signature
        memory: Arc<MemoryEngine>,
        providers: Arc<ProviderPool>,
        orchestrator: Arc<ToolOrchestrator>,
        config: AppConfig,
        session_store: Arc<SessionStore>,
        code_mode: Arc<CodeModeStore>,
        model_store: Arc<crate::memory::model_store::ModelStore>,
        plugins: Arc<PluginManager>,
        checkpoint_manager: Arc<CheckpointManager>,
        subagent_context: Arc<parking_lot::RwLock<(Option<String>, Option<String>)>>,
    ) -> Result<Self> {
        let persona = load_current_persona().unwrap_or_else(|err| {
            tracing::warn!(
                "Failed to load current persona, using config persona: {}",
                err
            );
            config.persona.clone()
        });
        let data_dir = PathBuf::from(shellexpand::tilde(&config.memory.database_path).into_owned())
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("~/.auxloclaw"));

        let compactor = Arc::new(Compactor::new(config.memory.clone(), data_dir.clone()));

        let reflector_config = ReflectorConfig {
            enabled: config.memory.reflection_enabled,
            min_messages: config.memory.reflection_min_messages,
            cooldown_secs: config.memory.reflection_cooldown_secs,
            max_messages: config.agent.recent_history_turns * 2,
            max_prompt_chars: (config.agent.context_window_tokens as usize).min(20_000),
        };
        let reflector = Arc::new(Reflector::new(reflector_config, data_dir.clone()));

        let extractor_config = ExtractorConfig {
            enabled: config.memory.extraction_enabled,
            min_tool_calls: config.memory.extraction_min_tool_calls,
            cooldown_secs: config.memory.extraction_cooldown_secs,
            pattern_threshold: config.memory.extraction_pattern_threshold,
            skills_dir: dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("auxloclaw")
                .join("skills"),
        };
        let extractor = Arc::new(SkillExtractor::new(extractor_config));

        Ok(Self {
            config,
            memory,
            providers,
            orchestrator,
            persona,
            usage: std::sync::atomic::AtomicU64::new(0),
            sessions: RwLock::new(HashMap::new()),
            session_store,
            code_mode,
            model_store,
            compactor,
            reflector,
            plugins,
            checkpoint_manager,
            extractor,
            intervention_registry: Arc::new(InterventionRegistry::new()),
            last_activity: RwLock::new(HashMap::new()),
            current_channel: parking_lot::RwLock::new(Option::<String>::None),
            current_user_id: parking_lot::RwLock::new(Option::<String>::None),
            subagent_context,
            override_system_prompt: Arc::new(RwLock::new(None)),
        })
    }

    pub async fn load_sessions(&self) -> Result<()> {
        let persisted = self.session_store.load_all()?;
        let mut sessions = self.sessions.write().await;
        for (key, history) in persisted {
            sessions.insert(key, history);
        }
        tracing::info!(" Loaded {} sessions from disk", sessions.len());
        Ok(())
    }

    /// Enable override mode to skip loading persona from disk.
    /// Used by /code mode to enforce the coding agent persona.
    /// Set a system prompt override, completely replacing the normal persona.
    /// Used by /code mode to inject the pure coding agent prompt.
    pub async fn set_system_prompt_override(&self, session_key: &str, prompt: String) {
        // Persist to disk so it survives restarts
        if let Err(e) = self.code_mode.activate(session_key, prompt.clone()) {
            tracing::warn!("Failed to persist code mode override: {}", e);
        }
        let mut override_prompt = self.override_system_prompt.write().await;
        *override_prompt = Some(prompt);
    }

    pub async fn clear_system_prompt_override(&self, session_key: &str) {
        // Remove from disk
        if let Err(e) = self.code_mode.deactivate(session_key) {
            tracing::warn!("Failed to remove persisted code mode: {}", e);
        }
        let mut override_prompt = self.override_system_prompt.write().await;
        *override_prompt = None;
    }
    
    /// Check if a session has a persisted code mode override
    pub fn get_persisted_code_override(&self, session_key: &str) -> Option<String> {
        self.code_mode.get_override(session_key)
    }

    /// Process a message with tool execution loop
    pub async fn set_session_context(&self, channel: &str, user_id: &str) {
        *self.current_channel.write() = Some(channel.to_string());
        *self.current_user_id.write() = Some(user_id.to_string());
        // Sync to shared context so sub-agent tool can read it
        *self.subagent_context.write() = (Some(channel.to_string()), Some(user_id.to_string()));
    }

    pub async fn process(&self, message: &str, session_id: Option<&str>) -> String {
        let session_key = session_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "default".to_string());

        let effective_message = self
            .plugins
            .process_message_hooks(
                HookEvent::BeforeMessage,
                Some(&session_key),
                message.to_string(),
            )
            .await;

        // Build provider request context with bounded history and reflections.
        let history = self.get_history(&session_key).await;
        let reflections = self.get_reflections(&session_key).unwrap_or_else(Vec::new);

        let mut system_prompt = self.build_system_prompt(&session_key).await;
        tracing::debug!("=== SYSTEM PROMPT START (first 500 chars) ===");
        tracing::debug!("{}", &system_prompt[..system_prompt.len().min(500)]);
        tracing::debug!("=== SYSTEM PROMPT END ===");
        if !reflections.is_empty() {
            system_prompt.push_str("\n\n## Recent Reflections\nOnly the newest bounded reflections are included to avoid context bloat.\n");
            for reflection in reflections.iter().take(3) {
                let serialized =
                    serde_json::to_string(reflection).unwrap_or_else(|_| reflection.title.clone());
                system_prompt.push_str("- ");
                system_prompt
                    .push_str(&truncate_for_summary(&serialized, 1_000).replace('\n', " "));
                system_prompt.push('\n');
            }
        }

        let mut messages = build_pruned_messages(
            system_prompt,
            &history,
            effective_message.clone(),
            self.config.agent.recent_history_turns,
            self.config.agent.context_window_tokens,
        );

        // Register intervention channel for this session
        let mut intervention_rx = self.intervention_registry.register(&session_key).await;

        // Tool execution loop
        let mut iterations = 0;
        let max_iterations = self.config.agent.max_tool_iterations as usize;
        let nudge_threshold = self.config.agent.nudge_after_tool_calls;
        let mut total_tool_calls: u32 = 0;
        let mut final_response = String::new();
        let mut tool_trace: Vec<ToolTraceEntry> = Vec::new();

        loop {
            iterations += 1;
            // Drain any mid-loop interventions (user messages injected while running)
            while let Ok(intervention) = intervention_rx.try_recv() {
                messages.push(Message::new("user", &intervention.message));
                tracing::debug!("Intervention injected into session {}", session_key);
            }
            if iterations > max_iterations {
                final_response = "Error: Max tool iterations reached".into();
                break;
            }

            let code_only = {
                let lock = self.override_system_prompt.read().await;
                lock.is_some()
            };
            let request = self.build_request(messages.clone(), code_only);

            match self.providers.complete(request).await {
                Ok(response) => {
                    // Update usage
                    if let Some(usage) = &response.usage {
                        self.usage.fetch_add(
                            usage.total_tokens as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }

                    // Check if there are tool calls
                    if let Some(tool_calls) = &response.tool_calls {
                        if !tool_calls.is_empty() {
                            tracing::debug!("Model returned {} tool_calls", tool_calls.len());

                            // Add assistant message with tool calls
                            messages.push(Message::with_tool_calls("assistant", tool_calls.clone()));

                            // Execute each tool
                            for tool_call in tool_calls {
                                let result = self.execute_tool(tool_call).await;
                                total_tool_calls += 1;
                                let success = !result.starts_with("Tool error:");
                                let summary = if result.len() > 500 {
                                    format!("{}...", &result[..result.floor_char_boundary(500)])
                                } else {
                                    result.clone()
                                };
                                tool_trace.push(ToolTraceEntry {
                                    tool_name: tool_call.function.name.clone(),
                                    arguments: tool_call.function.arguments.clone(),
                                    result_summary: summary,
                                    success,
                                    iteration: iterations,
                                });

                                // Add tool result as a message
                                messages.push(Message::tool_result(
                                    &tool_call.id,
                                    &tool_call.function.name,
                                    &result,
                                ));
                            }

                            // Check if nudge threshold crossed
                            if nudge_threshold > 0 && total_tool_calls >= nudge_threshold {
                                let nudge = build_nudge_message(total_tool_calls);
                                messages.push(Message::new("user", &nudge));
                                total_tool_calls = 0;
                                tracing::info!("Nudge injected after {} tool calls", nudge_threshold);
                            }

                            tracing::debug!("Continuing loop with {} messages", messages.len());
                            // Continue loop to get next response
                            continue;
                        }
                    }

                    // No tool calls - this is the final response
                    final_response = response.content;
                    break;
                }
                Err(e) => {
                    tracing::error!("Provider error: {}", e);
                    final_response = format!("Error: {}", e);
                    break;
                }
            }
        }

        // Unregister intervention channel
        self.intervention_registry.unregister(&session_key).await;

        // Store in history
        self.add_to_history(&session_key, "user", &effective_message)
            .await;
        self.add_to_history(&session_key, "assistant", &final_response)
            .await;

        // Run compaction check after assistant response
        if let Some(result) = self.run_post_compaction_check(&session_key).await {
            tracing::info!(
                "Session compacted: {} messages saved ~{} tokens",
                result.compacted_messages,
                result.tokens_saved
            );
        }

        // Spawn background skill extraction (non-blocking)
        if !tool_trace.is_empty() {
            let trace_clone = tool_trace.clone();
            let session_clone = session_key.clone();
            let extractor = Arc::clone(&self.extractor);
            let reflection = self.get_reflections(&session_key).and_then(|mut r| r.pop());
            let user_msg = effective_message.clone();
            tokio::spawn(async move {
                if let Err(e) = extractor
                    .run(&trace_clone, &session_clone, reflection.as_ref(), &user_msg)
                    .await
                {
                    tracing::warn!("Skill extraction background task failed: {e}");
                }
            });
        }

        // Filter out <thought></thought> blocks including content using regex
        let re = Regex::new(r"(?s)<thought>.*?</thought>").unwrap();
        let filtered_response = re.replace_all(&final_response, "").to_string();
        let filtered_response = self
            .plugins
            .process_message_hooks(
                HookEvent::AfterMessage,
                Some(&session_key),
                filtered_response,
            )
            .await;

        // Update last activity timestamp for the session
        self.touch_activity(&session_key).await;

        filtered_response
    }

    async fn execute_tool(&self, tool_call: &ToolCall) -> String {
        let args: serde_json::Value =
            serde_json::from_str(&tool_call.function.arguments).unwrap_or(serde_json::Value::Null);

        tracing::info!(
            " Executing tool: {} with args: {}",
            tool_call.function.name,
            args
        );

        // Execute via orchestrator
        let result = self
            .orchestrator
            .execute_tool(&tool_call.function.name, args)
            .await;

        if result.success {
            serde_json::to_string(&result.output).unwrap_or_else(|_| result.output.to_string())
        } else {
            format!(
                "Tool error: {}",
                result.error.as_ref().unwrap_or(&"Unknown error".into())
            )
        }
    }

    async fn get_history(&self, session_key: &str) -> Vec<HistoryMessage> {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(session_key) {
            session.messages.clone()
        } else {
            vec![]
        }
    }

    pub async fn add_to_history(&self, session_key: &str, role: &str, content: &str) {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .entry(session_key.to_string())
            .or_insert_with(|| SessionHistory::new(session_key));

        session.add_message(role, content, None);

        // Note: removed the old 50 message limit - compaction handles this now

        let session_clone = session.clone();
        drop(sessions);

        let _ = self.session_store.save(session_key, &session_clone);
    }

    /// Run post-compaction check after assistant response
    /// Returns the compaction result if compaction was triggered
    pub async fn run_post_compaction_check(&self, session_key: &str) -> Option<CompactionResult> {
        // Get current message count
        let message_count = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_key)
                .map(|s| s.messages.len())
                .unwrap_or(0)
        };

        // Check if compaction should run
        if !self.compactor.should_compact(session_key, message_count) {
            return None;
        }

        tracing::info!(
            "Compaction triggered for session {} ({} messages >= threshold {})",
            session_key,
            message_count,
            self.config.memory.compaction_threshold
        );

        // Get mutable session and compact
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(session_key) {
            match self.compactor.compact(session).await {
                Ok(result) => {
                    if result.success {
                        tracing::info!(
                            "Compaction complete: {} -> {} messages, ~{} tokens saved",
                            result.original_messages,
                            result.compacted_messages,
                            result.tokens_saved
                        );

                        // Save compacted session
                        let session_clone = session.clone();
                        drop(sessions);
                        let _ = self.session_store.save(session_key, &session_clone);

                        return Some(result);
                    } else {
                        tracing::warn!(
                            "Compaction failed: {}",
                            result
                                .error
                                .as_ref()
                                .unwrap_or(&"Unknown error".to_string())
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("Compaction error: {}", e);
                }
            }
        }

        None
    }

    fn build_request(&self, messages: Vec<Message>, code_only: bool) -> CompletionRequest {
        let all_tools: Vec<crate::providers::ToolDefinition> = self
            .orchestrator
            .get_definitions()
            .into_iter()
            .map(|t| crate::providers::ToolDefinition {
                tool_type: t.tool_type,
                function: crate::providers::FunctionDefinition {
                    name: t.function.name,
                    description: t.function.description,
                    parameters: t.function.parameters,
                },
            })
            .collect();

        let tools: Vec<crate::providers::ToolDefinition> = if code_only {
            all_tools
                .into_iter()
                .filter(|t| {
                    matches!(
                        t.function.name.as_str(),
                        "read_file"
                            | "list_files"
                            | "edit_file"
                            | "edit_file_llm"
                            | "create_or_rewrite_file"
                            | "run_bash_command"
                            | "run_sequential_cmds"
                            | "run_parallel_cmds"
                            | "grep_search"
                    )
                })
                .collect()
        } else {
            all_tools
        };

        // Resolve per-user model override (model name, base_url, api_key)
        let (effective_model, user_base_url, user_api_key) = {
            let channel = self.current_channel.read();
            let user_id = self.current_user_id.read();
            match (channel.as_deref(), user_id.as_deref()) {
                (Some(ch), Some(uid)) => {
                    match crate::commands::model::resolve_user_model(&self.model_store, ch, uid) {
                        Ok((_provider_type, base_url, api_key, Some(model))) => {
                            tracing::info!(
                                "Using user model override: {} base={:?}",
                                model, base_url
                            );
                            (model, base_url, api_key)
                        }
                        _ => (self.config.agent.default_model.clone(), None, None),
                    }
                }
                _ => (self.config.agent.default_model.clone(), None, None),
            }
        };

        let is_override = self.current_channel.read().is_some();
        tracing::info!("Using model: {} (user_override: {})", effective_model, is_override);

        CompletionRequest {
            model: effective_model,
            messages,
            temperature: Some(self.config.agent.temperature),
            max_tokens: Some(self.config.agent.max_tokens),
            tools: Some(tools),
            stream: None,
            base_url: user_base_url,
            api_key: user_api_key,
        }
    }

    pub fn capability_manifest(&self) -> CapabilityManifest {
        CapabilityManifest::new(&self.config, Some(&self.orchestrator))
    }

    async fn build_system_prompt(&self, session_key: &str) -> String {
        use crate::persona::SystemPromptBuilder;

        let tools = self.orchestrator.get_definitions();
        let capability_summary = self.capability_manifest().prompt_summary();
        let mcp_summaries = self.orchestrator.mcp_summaries();

        // Check if there's an override system prompt (e.g., /code mode)
        {
            let override_lock = self.override_system_prompt.read().await;
            if let Some(ref override_prompt) = *override_lock {
                tracing::info!("[build_system_prompt] OVERRIDE active - bypassing persona, using custom prompt ({} chars)", override_prompt.len());
                let mut prompt = format!("{}\n\n{}", override_prompt, capability_summary);
                if !mcp_summaries.is_empty() {
                    prompt.push_str("\n\n## Connected Integrations (MCP)\n");
                    for s in &mcp_summaries {
                        prompt.push_str(&format!("- {}\n", s));
                    }
                }
                return prompt;
            }
        }

        // Normal persona flow
        tracing::info!("[build_system_prompt] No override - using normal persona flow");
        let persona = load_current_persona().unwrap_or_else(|err| {
            tracing::warn!(
                "Failed to reload current persona, using cached persona: {}",
                err
            );
            self.persona.clone()
        });
        let base_prompt = SystemPromptBuilder::new(persona)
            .with_tools(&tools)
            .with_skills(&[])
            .build();

        let mut prompt = format!("{}\n\n{}", base_prompt, capability_summary);
        if !mcp_summaries.is_empty() {
            prompt.push_str("\n\n## Connected Integrations (MCP)\n");
            for s in &mcp_summaries {
                prompt.push_str(&format!("- {}\n", s));
            }
        }
        prompt
    }

    pub async fn memory_summary(&self) -> String {
        let sessions = self.sessions.read().await;
        if sessions.is_empty() {
            return "No conversation history stored yet.".into();
        }

        let mut summary = String::new();
        for (key, session) in sessions.iter() {
            summary.push_str(&format!(
                "Session: {} ({} messages)\n",
                key,
                session.messages.len()
            ));
        }
        summary
    }

    /// Clear a session from memory and disk
    pub async fn clear_session(&self, session_id: &str) {
        // Remove from in-memory sessions
        self.sessions.write().await.remove(session_id);

        // Delete from disk
        if let Err(e) = self.session_store.delete(session_id) {
            tracing::warn!("Failed to delete session file: {}", e);
        }

        tracing::info!("Cleared session: {}", session_id);
    }

    pub async fn recover_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    pub async fn new_session(&self, session_id: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.to_string(), SessionHistory::new(session_id));
        let session = sessions.get(session_id).cloned()
            .ok_or_else(|| anyhow::anyhow!("session not found after insert: {}", session_id))?;
        drop(sessions);
        self.session_store.save(session_id, &session)?;
        Ok(())
    }

    pub fn list_tools(&self) -> Vec<ToolInfo> {
        self.orchestrator
            .get_definitions()
            .into_iter()
            .map(|t| ToolInfo {
                name: t.function.name,
                description: t.function.description,
            })
            .collect()
    }

    pub async fn get_usage_stats(&self) -> Usage {
        Usage {
            total_messages: 0,
            total_tokens: self.usage.load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    pub fn model_name(&self) -> &str {
        &self.config.agent.default_model
    }

    /// Run reflection on a session
    /// Can be triggered explicitly or after significant activity
    pub async fn run_reflection(&self, session_key: &str) -> Option<Reflection> {
        // Get current message count
        let message_count = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_key)
                .map(|s| s.messages.len())
                .unwrap_or(0)
        };

        // Check if reflection should run
        if !self.reflector.should_reflect(session_key, message_count) {
            return None;
        }

        tracing::info!(
            "Reflection triggered for session {} ({} messages)",
            session_key,
            message_count
        );

        // Get session and reflect
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(session_key) {
            match self.reflector.reflect(session).await {
                Ok(Some(reflection)) => {
                    tracing::info!(
                        "Reflection complete: {} - {}",
                        reflection.reflection_type.to_string().to_lowercase(),
                        reflection.title
                    );
                    return Some(reflection);
                }
                Ok(None) => {
                    tracing::debug!("Reflection skipped for session {}", session_key);
                }
                Err(e) => {
                    tracing::error!("Reflection error: {}", e);
                }
            }
        }

        None
    }

    /// Update last activity timestamp for a session
    pub async fn touch_activity(&self, session_key: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut last_activity = self.last_activity.write().await;
        last_activity.insert(session_key.to_string(), now);
    }

    /// Get sessions that have been inactive for longer than reflection_interval_secs
    /// and have enough messages to qualify for reflection
    pub async fn get_sessions_needing_reflection(&self) -> Vec<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let interval_secs = self.config.memory.reflection_interval_secs;
        let min_messages = self.config.memory.reflection_min_messages;

        let sessions = self.sessions.read().await;
        let last_activity = self.last_activity.read().await;

        let mut result = Vec::new();

        for (key, session) in sessions.iter() {
            let last = last_activity.get(key).copied().unwrap_or(0);
            let inactive_secs = now.saturating_sub(last);

            if inactive_secs < interval_secs || session.messages.len() < min_messages {
                continue;
            }

            if !self.reflector.should_reflect(key, session.messages.len()) {
                continue;
            }

            if let Ok(reflections) = self.reflector.load_reflections(key) {
                if let Some(latest) = reflections.first() {
                    if latest.message_count >= session.messages.len() {
                        continue;
                    }
                }
            }

            result.push(key.clone());
        }

        result
    }

    /// Get reflections for a session
    pub fn get_reflections(&self, session_key: &str) -> Option<Vec<Reflection>> {
        self.reflector.load_reflections(session_key).ok()
    }

    /// Get all reflections across all sessions
    pub fn get_all_reflections(&self) -> Option<Vec<Reflection>> {
        self.reflector.load_all_reflections().ok()
    }

    pub async fn create_checkpoint(&self, session_id: &str, label: Option<&str>) -> Result<String> {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(session_id) {
            self.checkpoint_manager
                .create_checkpoint(session_id, session, label)
        } else {
            Err(anyhow::anyhow!("Session not found: {}", session_id))
        }
    }

    pub async fn rollback_session(&self, session_id: &str, checkpoint_id: &str) -> Result<()> {
        let history = self
            .checkpoint_manager
            .rollback(session_id, checkpoint_id)?;
        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.to_string(), history);
        Ok(())
    }

    /// Hot-reload MCP integrations from config on disk.
    /// Returns a user-friendly summary of what was loaded.
    pub async fn reload_mcp(&self) -> String {
        match self.orchestrator.reload_mcp().await {
            Ok((count, summaries)) => {
                if count == 0 {
                    "MCP reload complete. No tools loaded (check config).".to_string()
                } else {
                    let mut msg = format!("MCP reload complete: {} tools loaded.\n", count);
                    for s in &summaries {
                        msg.push_str(&format!("- {}\n", s));
                    }
                    msg
                }
            }
            Err(e) => format!("MCP reload failed: {}", e),
        }
    }

    /// List MCP tool names for a given server name
    pub fn mcp_tools_for(&self, server_name: &str) -> Vec<String> {
        let prefix = format!("mcp_{}_", server_name);
        self.orchestrator.tools_with_prefix(&prefix)
    }
}
