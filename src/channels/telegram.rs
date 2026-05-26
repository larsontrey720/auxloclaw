//! Telegram channel adapter with full command support.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use teloxide::{
    dispatching::Dispatcher,
    prelude::*,
    types::{ChatAction, ChatId, Update},
    utils::command::BotCommands,
    Bot,
};

use crate::agent::AgentCore;
use crate::channels::markdown::markdown_to_telegram;
use crate::config::TelegramConfig;
use crate::persona::shared::{
    load_current_persona, reset_persona, set_behavior, set_length, set_name, set_no_em_dashes,
    set_no_emojis, set_tone, toggle_no_em_dashes, toggle_no_emojis,
};

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(description = "View agent memory")]
    Memory,
    #[command(description = "Clear conversation history")]
    Clear,
    #[command(description = "List available tools")]
    Tools,
    #[command(description = "View token usage statistics")]
    Usage,
    #[command(description = "Recover from crashed session")]
    Recover,
    #[command(description = "Show help message")]
    Help,
    #[command(description = "Check bot status")]
    Status,
    #[command(description = "Toggle voice mode or set voice")]
    Voice,
    #[command(description = "Manage agent persona")]
    Persona,
    #[command(description = "Update auxloclaw to the latest version")]
    Update,
    #[command(description = "Start a coding session in isolated workspace")]
    Code,
    #[command(description = "Start a new session")]
    New,
    #[command(description = "Exit coding mode")]
    Normal,
    #[command(description = "Override model/provider settings")]
    Model(String),
    #[command(description = "Manage MCP server integrations")]
    Mcp(String),
    #[command(description = "Manage API tokens for MCP servers")]
    Token(String),
}



/// Spawn a background task that sends the typing indicator every 4 seconds
/// until the returned guard is dropped (i.e. processing completes).
fn spawn_typing_loop(bot: &Bot, chat_id: i64) -> tokio::sync::oneshot::Sender<()> {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let bot = bot.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(4));
        tokio::pin!(let cancel = rx;);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let _ = bot.send_chat_action(ChatId(chat_id), ChatAction::Typing).await;
                }
                _ = &mut cancel => {
                    break;
                }
            }
        }
    });
    tx
}

#[derive(Debug, Clone)]
struct SessionState {
    message_count: u64,
    total_tokens: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    created_at: chrono::DateTime<chrono::Utc>,
    voice_mode: bool,
    voice_id: Option<String>,
    last_activity: chrono::DateTime<chrono::Utc>,
}

impl Default for SessionState {
    fn default() -> Self {
        let now = chrono::Utc::now();
        Self {
            message_count: 0,
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            created_at: now,
            voice_mode: false,
            voice_id: None,
            last_activity: now,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ModelFlowState {
    /// User selected custom subtype, waiting for endpoint URL + model ID
    WaitingForEndpoint { subtype: String },
    /// User sent endpoint, waiting for API key
    WaitingForKey,
}

pub struct TelegramState {
    agent: Arc<AgentCore>,
    model_store: Arc<crate::memory::model_store::ModelStore>,
    sessions: RwLock<HashMap<i64, SessionState>>,
    code_mode: Arc<crate::memory::CodeModeStore>,
    config: TelegramConfig,
    /// Per-user flow state for multi-step model setup (ephemeral, lost on restart)
    pending_model_flows: RwLock<HashMap<i64, ModelFlowState>>,
    /// Adapter for mid-task message delivery
    message_adapter: Option<Arc<crate::tools::TelegramAdapter>>,
}

impl TelegramState {
    pub fn new(
        agent: Arc<AgentCore>,
        model_store: Arc<crate::memory::model_store::ModelStore>,
        code_mode: Arc<crate::memory::CodeModeStore>,
        config: TelegramConfig,
        message_adapter: Option<Arc<crate::tools::TelegramAdapter>>,
    ) -> Self {
        Self {
            agent,
            model_store,
            sessions: RwLock::new(HashMap::new()),
            code_mode,
            config,
            pending_model_flows: RwLock::new(HashMap::new()),
            message_adapter,
        }
    }

    async fn get_or_create_session(&self, chat_id: i64) -> SessionState {
        let mut sessions = self.sessions.write().await;
        sessions
            .entry(chat_id)
            .or_insert_with(|| SessionState {
                created_at: chrono::Utc::now(),
                last_activity: chrono::Utc::now(),
                ..Default::default()
            })
            .clone()
    }

    async fn update_session(&self, chat_id: i64, tokens: Option<(u32, u32)>) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&chat_id) {
            session.message_count += 1;
            session.last_activity = chrono::Utc::now();
            if let Some((prompt, completion)) = tokens {
                session.prompt_tokens += prompt as u64;
                session.completion_tokens += completion as u64;
                session.total_tokens += (prompt + completion) as u64;
            }
        }
    }

    async fn is_coding(&self, chat_id: i64) -> bool {
        let session_key = format!("tg-code-{}", chat_id);
        self.code_mode.get_override(&session_key).is_some()
    }

    async fn enter_code_mode(&self, _chat_id: i64, _workspace: String) {
        // Code mode activation is handled by agent.set_system_prompt_override which persists to CodeModeStore
    }

    async fn exit_code_mode(&self, _chat_id: i64) {
        // Code mode deactivation is handled by agent.clear_system_prompt_override which removes from CodeModeStore
    }

    async fn clear_session(&self, chat_id: i64) {
        let mut sessions = self.sessions.write().await;
        sessions.insert(
            chat_id,
            SessionState {
                created_at: chrono::Utc::now(),
                last_activity: chrono::Utc::now(),
                ..Default::default()
            },
        );
    }
}

pub async fn start(
    agent: Arc<AgentCore>,
    model_store: Arc<crate::memory::model_store::ModelStore>,
    code_mode: Arc<crate::memory::CodeModeStore>,
    config: Option<TelegramConfig>,
    _persona: crate::persona::PersonaConfig,
    message_router: Option<Arc<crate::tools::MessageRouter>>,
) -> Result<()> {
    let config = config.ok_or_else(|| anyhow!("Telegram config required"))?;
    if !config.enabled || config.token.is_empty() {
        tracing::info!("Telegram gateway disabled");
        return Ok(());
    }

    let bot = Bot::new(config.token.clone());

    // Create Telegram adapter for mid-task message delivery
    let default_chat_id = config.allowed_users.first()
        .and_then(|u| u.parse::<i64>().ok());
    let tg_adapter = Arc::new(crate::tools::TelegramAdapter::new(
        bot.clone(),
        default_chat_id,
    ));

    if let Some(router) = message_router {
        router.register(tg_adapter.clone() as Arc<dyn crate::tools::PlatformAdapter>).await;
        tracing::info!("Telegram registered with message router");
    }

    let state = Arc::new(TelegramState::new(agent, model_store, code_mode, config, Some(tg_adapter)));

    let commands = vec![
        teloxide::types::BotCommand { command: "memory".into(), description: "View agent memory".into() },
        teloxide::types::BotCommand { command: "clear".into(), description: "Clear conversation history".into() },
        teloxide::types::BotCommand { command: "tools".into(), description: "List available tools".into() },
        teloxide::types::BotCommand { command: "usage".into(), description: "View token usage statistics".into() },
        teloxide::types::BotCommand { command: "recover".into(), description: "Recover from crashed session".into() },
        teloxide::types::BotCommand { command: "help".into(), description: "Show help message".into() },
        teloxide::types::BotCommand { command: "status".into(), description: "Check bot status".into() },
        teloxide::types::BotCommand { command: "voice".into(), description: "Toggle voice mode or set voice".into() },
        teloxide::types::BotCommand { command: "persona".into(), description: "Show or edit persona".into() },
        teloxide::types::BotCommand { command: "new".into(), description: "Start new session".into() },
        teloxide::types::BotCommand { command: "update".into(), description: "Update auxloclaw to the latest version".into() },
        teloxide::types::BotCommand { command: "code".into(), description: "Start a coding session in isolated workspace".into() },
        teloxide::types::BotCommand { command: "normal".into(), description: "Exit coding mode and return to normal persona".into() },
        teloxide::types::BotCommand { command: "model".into(), description: "Override model/provider settings".into() },
        teloxide::types::BotCommand { command: "mcp".into(), description: "Manage MCP server integrations".into() },
        teloxide::types::BotCommand { command: "token".into(), description: "Manage API tokens".into() },
    ];
    let _ = bot.set_my_commands(commands).await;

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(Update::filter_callback_query().endpoint(handle_callback_query))
        .branch(Update::filter_message().endpoint(handle_message));

    tokio::spawn(async move {
        Dispatcher::builder(bot, handler)
            .dependencies(dptree::deps![state])
            .build()
            .dispatch()
            .await;
    });

    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn render_persona() -> String {
    match load_current_persona() {
        Ok(persona) => format!(
            "Current persona\n\nName: {}\nLength: {}\nTone: {}\nNo em dashes: {}\nNo emojis: {}\n\nBehavior:\n{}\n\nCommands:\n/persona show\n/persona name <value>\n/persona behavior <value>\n/persona tone <professional|casual|technical|friendly>\n/persona length <concise|balanced|detailed>\n/persona no_emojis <on|off>\n/persona no_em_dashes <on|off>\n/persona reset",
            persona.name,
            persona.style.length,
            persona.style.tone,
            persona.style.formatting.no_em_dashes,
            persona.style.formatting.no_emojis,
            persona.behavior
        ),
        Err(e) => format!("Failed to load persona: {e}"),
    }
}

fn handle_persona_command_text(text: &str) -> String {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("/persona").unwrap_or("").trim();
    if rest.is_empty() || rest == "show" {
        return render_persona();
    }

    let mut parts = rest.splitn(2, ' ');
    let sub = parts.next().unwrap_or("").trim();
    let arg = parts.next().unwrap_or("").trim();

    let result = match sub {
        "name" if !arg.is_empty() => set_name(arg).map(|_| "Persona name updated".to_string()),
        "behavior" if !arg.is_empty() => set_behavior(arg).map(|_| "Persona behavior updated".to_string()),
        "tone" if !arg.is_empty() => set_tone(arg).map(|_| format!("Persona tone set to {arg}")),
        "length" if !arg.is_empty() => set_length(arg).map(|_| format!("Persona length set to {arg}")),
        "no_emojis" if !arg.is_empty() => {
            let enabled = matches!(arg, "on" | "true" | "yes" | "1");
            set_no_emojis(enabled).map(|_| format!("Persona no_emojis set to {enabled}"))
        }
        "no_emojis" => toggle_no_emojis().map(|enabled| format!("Persona no_emojis set to {enabled}")),
        "no_em_dashes" if !arg.is_empty() => {
            let enabled = matches!(arg, "on" | "true" | "yes" | "1");
            set_no_em_dashes(enabled).map(|_| format!("Persona no_em_dashes set to {enabled}"))
        }
        "no_em_dashes" => toggle_no_em_dashes().map(|enabled| format!("Persona no_em_dashes set to {enabled}")),
        "reset" => reset_persona().map(|_| "Persona reset to defaults".to_string()),
        _ => Err(anyhow!("Invalid persona command. Use /persona show, name, behavior, tone, length, no_emojis, no_em_dashes, or reset")),
    };

    match result {
        Ok(msg) => format!("{}\n\n{}", msg, render_persona()),
        Err(e) => format!("{}\n\n{}", e, render_persona()),
    }
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<TelegramState>,
) -> ResponseResult<()> {
    let chat_id: i64 = msg.chat.id.0;
    let _typing_guard = spawn_typing_loop(&bot, chat_id);

    let response = match cmd {
        Command::Memory => state.agent.memory_summary().await,
        Command::Clear => {
            let session_id = format!("tg:{}", chat_id);
            state.agent.clear_session(&session_id).await;
            state.sessions.write().await.remove(&chat_id);
            "Session cleared".to_string()
        }
        Command::Tools => {
            let tools = state.agent.list_tools();
            let mut list = String::from("Available tools\n\n");
            for tool in tools {
                list.push_str(&format!("- {}: {}\n", tool.name, tool.description));
            }
            list
        }
        Command::Usage => {
            let session = state.get_or_create_session(chat_id).await;
            let usage = state.agent.get_usage_stats().await;
            format!(
                "Current session\nMessages: {}\nTotal tokens: {}\nPrompt tokens: {}\nCompletion tokens: {}\n\nAll-time\nMessages: {}\nTokens: {}",
                session.message_count,
                session.total_tokens,
                session.prompt_tokens,
                session.completion_tokens,
                usage.total_messages,
                usage.total_tokens,
            )
        }
        Command::Recover => {
            let _ = state
                .agent
                .recover_session(&format!("tg:{}", chat_id))
                .await;
            "Session recovered".to_string()
        }
        Command::Help => {
            "Commands: /memory /clear /tools /usage /recover /status /voice /persona /new /update /code /normal"
                .to_string()
        }
        Command::Status => {
            let session = state.get_or_create_session(chat_id).await;
            let uptime = chrono::Utc::now() - session.created_at;
            format!(
                "Status: Online\nUptime: {}h {}m\nActive chats: {}\nModel: {}\nVoice mode: {}",
                uptime.num_hours(),
                uptime.num_minutes() % 60,
                state.sessions.read().await.len(),
                state.agent.model_name(),
                if session.voice_mode { "ON" } else { "OFF" },
            )
        }
        Command::Voice => {
            let mut sessions = state.sessions.write().await;
            let session = sessions.entry(chat_id).or_default();
            session.voice_mode = !session.voice_mode;
            format!(
                "Voice mode {}",
                if session.voice_mode { "ON" } else { "OFF" }
            )
        }
        Command::Persona => handle_persona_command_text(msg.text().unwrap_or("/persona")),
        Command::Update => crate::commands::update::handle_update().await,
        Command::Code => {
            // Enter code mode: set override on shared agent and track this chat
            let session_id = format!("tg-code-{}", chat_id);
            let workspace = crate::commands::code::ensure_workspace(&session_id)
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to create workspace: {}", e);
                    std::path::PathBuf::from("/tmp/auxloclaw-code")
                });
            let _ = crate::commands::code::init_workspace(&workspace);
            let code_prompt = crate::commands::code::build_code_system_prompt(&workspace);
            state.agent.set_session_context("telegram", &format!("{}", chat_id)).await;
            state.agent.set_system_prompt_override(&format!("tg-code-{}", chat_id), code_prompt).await;
            state.enter_code_mode(chat_id, workspace.display().to_string()).await;
            format!(
                "Coding mode activated.\nWorkspace: {}\n\nSend your coding task as the next message. Use /normal to exit coding mode.",
                workspace.display()
            )
        }
        Command::Model(args) => {
            let user_id = msg.from().map(|u| u.id.0).unwrap_or(0);
            let user_id_str = user_id.to_string();

            // When /model is called with no args, show the inline keyboard
            if args.trim().is_empty() {
                let response = match crate::commands::model::handle_model(
                    &state.model_store,
                    "telegram",
                    &user_id_str,
                    &args,
                ) {
                    Ok(resp) => resp,
                    Err(e) => format!("Error: {}", e),
                };

                let keyboard = crate::commands::model::provider_keyboard_json();
                let formatted = crate::channels::markdown::markdown_to_telegram(&response);
                let markup: teloxide::types::InlineKeyboardMarkup = serde_json::from_str(&keyboard).unwrap_or_default();
                let _ = bot
                    .send_message(teloxide::types::ChatId(chat_id), &formatted)
                    .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                    .reply_markup(markup)
                    .await;
                return Ok(());
            }

            // Text-based /model commands
            let response = match crate::commands::model::handle_model(
                &state.model_store,
                "telegram",
                &user_id_str,
                &args,
            ) {
                Ok(resp) => resp,
                Err(e) => format!("Error: {}", e),
            };
            send_markdown_message(&bot, chat_id, &response).await?;
            return Ok(());
        }
        Command::Mcp(args) => {
            let response = crate::commands::mcp::handle_mcp(&args, Some(&state.agent))
                .await
                .unwrap_or_else(|e| format!("Error: {}", e));
            send_markdown_message(&bot, chat_id, &response).await?;
            return Ok(());
        }
        Command::Token(args) => {
            // Check if the original message contains a secret - delete it
            if let Some(text) = msg.text() {
                if crate::commands::token::contains_secret(text) {
                    // Best-effort delete - don't fail the command if delete fails
                    let _ = bot.delete_message(teloxide::types::ChatId(chat_id), msg.id).await;
                }
            }
            let response = crate::commands::token::handle_token(&args)
                .unwrap_or_else(|e| format!("Error: {}", e));
            send_markdown_message(&bot, chat_id, &response).await?;
            return Ok(());
        }
        Command::Normal => {
            state.agent.clear_system_prompt_override(&format!("tg-code-{}", chat_id)).await;
            "Exited coding mode. Back to normal.".to_string()
        }
        Command::New => {
            let session_id = format!("tg:{}", chat_id);
            state.agent.clear_session(&session_id).await;
            state.clear_session(chat_id).await;
            "New session started".to_string()
        }
    };

    send_markdown_message(&bot, chat_id, &response).await?;
    Ok(())
}

/// Handle Telegram callback queries (inline keyboard button presses).
async fn handle_callback_query(
    bot: Bot,
    q: teloxide::types::CallbackQuery,
    state: Arc<TelegramState>,
) -> ResponseResult<()> {
    let chat_id = if let Some(msg) = &q.message {
        msg.chat.id.0
    } else {
        return Ok(());
    };

    let data = q.data.as_deref().unwrap_or("");

    // Only handle model:... callbacks
    if !data.starts_with("model:") {
        return Ok(());
    }

    let user_id = q.from.id.0;

    // Special handling for custom_subtype: set flow state instead of returning text instructions
    if data.starts_with("model:custom_subtype:") {
        let subtype = data.strip_prefix("model:custom_subtype:").unwrap_or("openai-compatible");
        let label = if subtype == "anthropic" { "Anthropic-style" } else { "OpenAI-compatible" };

        // Save the provider type
        let user_id_str = user_id.to_string();
        let get_result = state.model_store.get("telegram", &user_id_str);
        let mut ov = match get_result {
            Ok(Some(ov)) => ov,
            Ok(None) => crate::memory::model_store::UserModelOverride::default(),
            Err(e) => {
                tracing::error!("Model store get error: {e:?}");
                let _ = bot.answer_callback_query(q.id).text("Storage error").await;
                return Ok(());
            }
        };
        ov.provider_type = Some(format!("custom/{}", subtype));
        ov.base_url = None;
        ov.updated_at = crate::commands::model::now_secs();
        if let Err(e) = state.model_store.set("telegram", &user_id_str, &ov) {
            tracing::error!("Model store set error: {e:?}");
            let _ = bot.answer_callback_query(q.id).text("Storage error").await;
            return Ok(());
        }

        // Set flow state: waiting for endpoint URL + model ID
        state.pending_model_flows.write().await.insert(chat_id, ModelFlowState::WaitingForEndpoint {
            subtype: subtype.to_string(),
        });

        let _ = bot.answer_callback_query(q.id).await;
        let _ = bot.send_message(
            teloxide::types::ChatId(chat_id),
            format!(
                "{} API format selected.\n\nSend your endpoint URL and model ID together, like:\nhttps://your-api.example.com/v1 model-name\n\nI'll auto-detect which is which.",
                label
            ),
        ).await;

        return Ok(());
    }

    match crate::commands::model::handle_callback(
        &state.model_store,
        "telegram",
        &user_id.to_string(),
        data,
    ) {
        Ok((response, keyboard, done)) => {
            let markup_str = keyboard.as_deref().unwrap_or("");
            let send_result = if markup_str.is_empty() {
                bot.send_message(teloxide::types::ChatId(chat_id), &response)
                    .await
            } else {
                let markup: teloxide::types::InlineKeyboardMarkup =
                    serde_json::from_str(markup_str).unwrap_or_default();
                bot.send_message(teloxide::types::ChatId(chat_id), &response)
                    .reply_markup(markup)
                    .await
            };
            if let Err(ref e) = send_result {
                tracing::error!("Callback message send failed: {}", e);
            }
            if done {
                let _ = bot.answer_callback_query(q.id).await;
            }
        }
        Err(e) => {
            let _ = bot
                .answer_callback_query(q.id)
                .text(format!("Error: {}", e))
                .await;
        }
    }

    Ok(())
}

async fn handle_model_flow(
    bot: &Bot,
    state: &TelegramState,
    chat_id: i64,
    msg_id: teloxide::types::MessageId,
    text: &str,
    flow: ModelFlowState,
) -> anyhow::Result<()> {
    use crate::commands::model;
    let user_id = format!("{}", chat_id);
    let store = &*state.model_store;

    match flow {
        ModelFlowState::WaitingForEndpoint { subtype } => {
            // Parse URL + model ID from freeform text
            // Auto-detect: anything with /v1 (or http(s)://) is the URL, rest is model ID
            let tokens: Vec<&str> = text.split_whitespace().collect();
            let mut url: Option<&str> = None;
            let mut model_id: Option<&str> = None;

            for token in &tokens {
                if token.contains("/v1") || token.starts_with("http://") || token.starts_with("https://") {
                    url = Some(token);
                } else {
                    model_id = Some(token);
                }
            }

            let url = url.ok_or_else(|| anyhow::anyhow!(
                "Could not detect a URL. Send both like:\n`https://your-api.example.com/v1 model-name`"
            ))?;
            let model_id = model_id.ok_or_else(|| anyhow::anyhow!(
                "Could not detect a model ID. Send both like:\n`https://your-api.example.com/v1 model-name`"
            ))?;

            // Save endpoint + model
            let mut ov = store.get("telegram", &user_id)?.unwrap_or_default();
            ov.provider_type = Some(format!("custom/{}", subtype));
            ov.base_url = Some(url.to_string());
            ov.model_id = Some(model_id.to_string());
            ov.updated_at = model::now_secs();
            store.set("telegram", &user_id, &ov)?;

            // Move to next step: ask for key
            state.pending_model_flows.write().await.insert(chat_id, ModelFlowState::WaitingForKey);

            let _ = send_markdown_message(
                bot,
                chat_id,
                &format!(
                    "Endpoint saved: {}\nModel: {}\n\nNow send your API key (just the key, nothing else).\nI will delete your message after saving it.",
                    url, model_id
                ),
            ).await?;
        }
        ModelFlowState::WaitingForKey => {
            let key = text.trim();
            if key.is_empty() || key.len() < 4 {
                // Put the flow state back so they can retry
                state.pending_model_flows.write().await.insert(chat_id, ModelFlowState::WaitingForKey);
                let _ = send_markdown_message(bot, chat_id, "Key too short. Send your API key again.").await?;
                return Ok(());
            }

            // Delete the message containing the key immediately
            let _ = bot.delete_message(teloxide::types::ChatId(chat_id), msg_id).await;

            // Save encrypted key
            let mut ov = store.get("telegram", &user_id)?.unwrap_or_default();
            let encrypted = store.encrypt_key(key)?;
            ov.encrypted_api_key = Some(encrypted);
            ov.updated_at = model::now_secs();
            store.set("telegram", &user_id, &ov)?;

            // Clear flow state -- done
            state.pending_model_flows.write().await.remove(&chat_id);

            let masked = model::mask_key(key);
            let summary = model::build_summary_for_user("telegram", &user_id, store)?;
            let _ = send_markdown_message(
                bot,
                chat_id,
                &format!(
                    "API key saved: `{}`\n\n{}",
                    masked, summary
                ),
            ).await?;
        }
    }

    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, state: Arc<TelegramState>) -> ResponseResult<()> {
    let chat_id: i64 = msg.chat.id.0;
    let text = msg.text().unwrap_or("");
    if text.is_empty() {
        return Ok(());
    }

    // Track active chat for mid-task message delivery
    if let Some(ref adapter) = state.message_adapter {
        adapter.set_active_chat(chat_id);
    }

    // Intercept model setup flow BEFORE any other check so endpoint/key
    // messages aren't eaten by the secret scanner or treated as AI chat.
    {
        let mut flows = state.pending_model_flows.write().await;
        if let Some(flow) = flows.remove(&chat_id) {
            drop(flows);
            match handle_model_flow(&bot, &state, chat_id, msg.id, text, flow).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("Model flow error: {e:?}");
                    let _ = send_markdown_message(&bot, chat_id, &format!("Error: {}. Try /model again.", e)).await;
                    return Ok(());
                }
            }
        }
    }

    // Auto-delete messages containing secrets
    if crate::commands::token::contains_secret(text) {
        let _ = bot.delete_message(teloxide::types::ChatId(chat_id), msg.id).await;
        send_markdown_message(&bot, chat_id, "Your message was deleted for security (it contained a token/secret). Use `/token set <server> <KEY> <value>` to store tokens safely.").await?;
        return Ok(());
    }

    if text.trim_start().starts_with("/persona") {
        let response = handle_persona_command_text(text);
        send_markdown_message(&bot, chat_id, &response).await?;
        return Ok(());
    }

    // Check for /normal command to exit code mode
    if text.trim() == "/normal" {
        state.exit_code_mode(chat_id).await;
        state.agent.clear_system_prompt_override(&format!("tg:{}", chat_id)).await;
        send_markdown_message(&bot, chat_id, "Exited coding mode. Back to normal.").await?;
        return Ok(());
    }

    let _typing_guard = spawn_typing_loop(&bot, chat_id);
    let session = state.get_or_create_session(chat_id).await;
    // Route through code session if in code mode
    let session_id = if state.is_coding(chat_id).await {
        format!("tg-code-{}", chat_id)
    } else {
        format!("tg:{}", chat_id)
    };
    state.agent.set_session_context("telegram", &format!("{}", chat_id)).await;
    let response = state
        .agent
        .process(text, Some(&session_id))
        .await;
    state.update_session(chat_id, None).await;

    if session.voice_mode {
        let voice_response = format!("Voice mode response\n\n{}", response);
        send_markdown_message(&bot, chat_id, &voice_response).await?;
    } else {
        if let Err(err) = send_markdown_message(&bot, chat_id, &response).await {
            tracing::warn!("Telegram send error: {err:?}");
        }
    }
    Ok(())
}

async fn send_markdown_message(bot: &Bot, chat_id: i64, text: &str) -> ResponseResult<()> {
    for chunk in split_telegram_message(text, 3900) {
        let formatted = markdown_to_telegram(&chunk);
        let send_result = bot.send_message(ChatId(chat_id), &formatted)
            .parse_mode(teloxide::types::ParseMode::MarkdownV2)
            .await;
        if let Err(err) = send_result {
            tracing::warn!("MarkdownV2 send failed ({err:?}), retrying as plain text");
            let plain = strip_all_formatting(&chunk);
            bot.send_message(ChatId(chat_id), plain)
                .await?;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    Ok(())
}

fn strip_all_formatting(text: &str) -> String {
    text.to_string()
}

fn split_telegram_message(text: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return vec![text.to_string()];
    }

    chars
        .chunks(max_chars)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}
