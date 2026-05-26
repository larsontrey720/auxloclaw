//! Cross-platform message sending tool.
//!
//! Allows the agent to proactively send messages to the user on any
//! connected platform. This enables mid-task progress updates, milestone
//! notifications, and status reports without waiting for the agent's
//! response cycle to complete.
//!
//! Architecture:
//! - `MessageRouter` holds references to all connected platform adapters
//! - `SendMessageTool` implements the `Tool` trait, dispatches through router
//! - Router is injected into tool orchestrator at gateway startup
//! - Supports text messages with optional markdown formatting
//! - Messages are auto-chunked at platform character limits
//!
//! Integration:
//! 1. Create `MessageRouter` before channel tasks spawn
//! 2. Register `SendMessageTool` with the orchestrator
//! 3. Pass `Arc<MessageRouter>` to channel start functions
//! 4. Channels register their adapters after creating their bot client

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::channels::markdown::markdown_to_telegram;
use crate::orchestrator::{Tool, ToolResult};

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// A message to be delivered to a platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub platform: String,
    pub target: Option<String>,
    pub text: String,
    pub parse_mode: Option<String>,
}

/// Result of a message delivery attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub success: bool,
    pub platform: String,
    pub target: String,
    pub message_id: Option<String>,
    pub error: Option<String>,
}

/// A registered platform target (channel, chat, user).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageTarget {
    pub platform: String,
    pub id: String,
    pub name: String,
    pub kind: String,
    pub is_default: bool,
}

// ---------------------------------------------------------------------------
// Platform adapter trait
// ---------------------------------------------------------------------------

/// Trait for platform-specific message delivery.
///
/// Each connected platform implements this trait. The `MessageRouter`
/// dispatches through the appropriate adapter based on the platform name.
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    /// Human-readable platform name.
    fn platform_name(&self) -> &str;

    /// Send a text message to a target.
    async fn send_message(
        &self,
        target: &str,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<DeliveryResult>;

    /// List available message targets on this platform.
    async fn list_targets(&self) -> Vec<MessageTarget>;

    /// Check if this adapter is currently connected and operational.
    async fn is_connected(&self) -> bool;
}

// ---------------------------------------------------------------------------
// Telegram adapter
// ---------------------------------------------------------------------------

/// Telegram platform adapter.
///
/// Wraps the teloxide Bot to send messages through the Telegram Bot API.
/// Supports markdown formatting, chat IDs, and topic/thread replies.
pub struct TelegramAdapter {
    bot: teloxide::Bot,
    config_chat_id: Option<i64>,
    active_chat_id: Arc<parking_lot::RwLock<Option<i64>>>,
}

impl TelegramAdapter {
    pub fn new(bot: teloxide::Bot, default_chat_id: Option<i64>) -> Self {
        Self {
            bot,
            config_chat_id: default_chat_id,
            active_chat_id: Arc::new(parking_lot::RwLock::new(default_chat_id)),
        }
    }

    /// Called by the Telegram channel on every incoming message so the adapter
    /// always knows the most recent chat to deliver mid-task messages to.
    pub fn set_active_chat(&self, chat_id: i64) {
        *self.active_chat_id.write() = Some(chat_id);
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    fn platform_name(&self) -> &str {
        "telegram"
    }

    async fn send_message(
        &self,
        target: &str,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<DeliveryResult> {
        use teloxide::prelude::*;
        use teloxide::types::ParseMode;

        // Resolve chat ID: prefer explicit target, fall back to active/config default
        let chat_id: i64 = if target.is_empty() || target == "default" {
            self.active_chat_id.read()
                .ok_or_else(|| anyhow!("No Telegram chat ID available. No active session and no default configured."))?
        } else {
            target.parse()
                .map_err(|_| anyhow!("Invalid Telegram chat ID: '{}'. Expected numeric ID.", target))?
        };

        // Convert markdown to Telegram MarkdownV2 if requested, otherwise plain text
        let (final_text, tg_parse_mode) = match parse_mode.as_deref() {
            Some("markdown") | Some("MarkdownV2") => {
                (markdown_to_telegram(text), Some(ParseMode::MarkdownV2))
            }
            Some("html") => {
                (text.to_string(), Some(ParseMode::Html))
            }
            _ => (text.to_string(), None),
        };

        // Split long messages (Telegram limit: 4096 chars)
        let chunks = split_message(&final_text, 4096);
        let mut last_msg_id = None;

        for chunk in &chunks {
            let mut send = self.bot.send_message(ChatId(chat_id), chunk);
            if let Some(mode) = tg_parse_mode {
                send = send.parse_mode(mode);
            }

            match send.await {
                Ok(msg) => {
                    last_msg_id = Some(msg.id.0.to_string());
                    debug!(
                        chat_id = chat_id,
                        msg_id = msg.id.0,
                        "Telegram message sent"
                    );
                }
                Err(e) => {
                    warn!(error = %e, chat_id = chat_id, "Telegram send failed");
                    return Ok(DeliveryResult {
                        success: false,
                        platform: "telegram".into(),
                        target: chat_id.to_string(),
                        message_id: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        Ok(DeliveryResult {
            success: true,
            platform: "telegram".into(),
            target: chat_id.to_string(),
            message_id: last_msg_id,
            error: None,
        })
    }

    async fn list_targets(&self) -> Vec<MessageTarget> {
        let mut targets = Vec::new();
        if let Some(chat_id) = *self.active_chat_id.read() {
            targets.push(MessageTarget {
                platform: "telegram".into(),
                id: chat_id.to_string(),
                name: "Active channel".into(),
                kind: "dm".into(),
                is_default: true,
            });
        }
        targets
    }

    async fn is_connected(&self) -> bool {
        use teloxide::prelude::Requester;
        self.bot.get_me().await.is_ok()
    }
}

// ---------------------------------------------------------------------------
// Discord adapter
// ---------------------------------------------------------------------------

/// Discord platform adapter.
///
/// Uses serenity's Http client to send messages to Discord channels or DMs.
/// Tracks the most recently active channel/user for mid-task delivery.
pub struct DiscordAdapter {
    http: Arc<serenity::http::Http>,
    /// (channel_id, user_id) of the last incoming message
    active_target: Arc<parking_lot::RwLock<Option<(u64, u64)>>>,
}

impl DiscordAdapter {
    pub fn new(http: Arc<serenity::http::Http>) -> Self {
        Self {
            http,
            active_target: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// Called by the Discord channel on every incoming message so the adapter
    /// always knows the most recent channel/user to deliver mid-task messages to.
    pub fn set_active_target(&self, channel_id: u64, user_id: u64) {
        *self.active_target.write() = Some((channel_id, user_id));
    }
}

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    fn platform_name(&self) -> &str {
        "discord"
    }

    async fn send_message(
        &self,
        target: &str,
        text: &str,
        _parse_mode: Option<&str>,
    ) -> Result<DeliveryResult> {
        use serenity::model::id::ChannelId;

        // Resolve channel ID
        let channel_id: u64 = if target.is_empty() || target == "default" {
            // Read and drop the guard before any .await
            let user_id = {
                let guard = self.active_target.read();
                let (_, uid) = guard.ok_or_else(|| anyhow!(
                    "No Discord target available. No active session and no default configured."
                ))?;
                uid
            };
            // For DMs, create/get the DM channel
            let user = serenity::model::id::UserId::new(user_id)
                .to_user(&self.http).await?;
            let dm_channel = user.create_dm_channel(&self.http).await?;
            dm_channel.id.get()
        } else {
            target.parse()
                .map_err(|_| anyhow!("Invalid Discord channel ID: '{}'. Expected numeric ID.", target))?
        };

        let ch = ChannelId::new(channel_id);

        // Split long messages (Discord limit: 2000 chars)
        let chunks = split_message(text, 2000);
        let mut last_msg_id = None;

        for chunk in &chunks {
            match ch.say(&self.http, chunk).await {
                Ok(msg) => {
                    last_msg_id = Some(msg.id.get().to_string());
                    debug!(channel_id = channel_id, "Discord message sent");
                }
                Err(e) => {
                    warn!(error = %e, channel_id = channel_id, "Discord send failed");
                    return Ok(DeliveryResult {
                        success: false,
                        platform: "discord".into(),
                        target: channel_id.to_string(),
                        message_id: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        Ok(DeliveryResult {
            success: true,
            platform: "discord".into(),
            target: channel_id.to_string(),
            message_id: last_msg_id,
            error: None,
        })
    }

    async fn list_targets(&self) -> Vec<MessageTarget> {
        let mut targets = Vec::new();
        if let Some((channel_id, user_id)) = *self.active_target.read() {
            targets.push(MessageTarget {
                platform: "discord".into(),
                id: channel_id.to_string(),
                name: format!("Active channel (user {})", user_id),
                kind: "dm".into(),
                is_default: true,
            });
        }
        targets
    }

    async fn is_connected(&self) -> bool {
        // If we have an HTTP client, assume connected
        // (serenity manages the websocket separately)
        true
    }
}

// ---------------------------------------------------------------------------
// Message Router
// ---------------------------------------------------------------------------

/// Central message router that dispatches to platform adapters.
///
/// Holds references to all connected platform adapters and routes
/// messages to the correct one based on the platform name.
/// Thread-safe and cloneable for use across async tasks.
#[derive(Clone)]
pub struct MessageRouter {
    adapters: Arc<RwLock<HashMap<String, Arc<dyn PlatformAdapter>>>>,
    default_platform: Option<String>,
}

impl MessageRouter {
    pub fn new() -> Self {
        Self {
            adapters: Arc::new(RwLock::new(HashMap::new())),
            default_platform: None,
        }
    }

    /// Register a platform adapter.
    pub async fn register(&self, adapter: Arc<dyn PlatformAdapter>) {
        let name = adapter.platform_name().to_string();
        let mut adapters = self.adapters.write().await;
        adapters.insert(name, adapter);
    }

    /// Set the default platform for messages without an explicit target.
    pub fn set_default_platform(&mut self, platform: String) {
        self.default_platform = Some(platform);
    }

    /// Send a message through the appropriate platform adapter.
    ///
    /// Attempts the send directly -- connection errors are caught from
    /// the adapter's send_message call rather than pre-checking
    /// is_connected(), which avoids an extra HTTP round-trip on every send.
    pub async fn send(&self, msg: &OutgoingMessage) -> Result<DeliveryResult> {
        let platform = if msg.platform.is_empty() {
            self.default_platform.as_deref()
                .ok_or_else(|| anyhow!("No platform specified and no default configured"))?
        } else {
            &msg.platform
        };

        let adapters = self.adapters.read().await;
        let adapter = adapters.get(platform)
            .ok_or_else(|| anyhow!(
                "Platform '{}' not connected. Available: {}",
                platform,
                adapters.keys().cloned().collect::<Vec<_>>().join(", ")
            ))?;

        let target = msg.target.as_deref().unwrap_or("default");
        adapter.send_message(target, &msg.text, msg.parse_mode.as_deref()).await
    }

    /// List all available targets across all connected platforms.
    pub async fn list_targets(&self) -> Vec<MessageTarget> {
        let adapters = self.adapters.read().await;
        let mut all_targets = Vec::new();
        for adapter in adapters.values() {
            if adapter.is_connected().await {
                all_targets.extend(adapter.list_targets().await);
            }
        }
        all_targets
    }

    /// Get names of connected platforms.
    pub async fn connected_platforms(&self) -> Vec<String> {
        let adapters = self.adapters.read().await;
        let mut connected = Vec::new();
        for (name, adapter) in adapters.iter() {
            if adapter.is_connected().await {
                connected.push(name.clone());
            }
        }
        connected
    }
}

impl Default for MessageRouter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Send Message Tool
// ---------------------------------------------------------------------------

/// Tool for sending messages to connected platforms.
///
/// Registered in the tool orchestrator. The agent calls this tool
/// to send progress updates, status reports, or notifications to
/// the user on any connected platform.
pub struct SendMessageTool {
    router: MessageRouter,
}

impl SendMessageTool {
    pub fn new(router: MessageRouter) -> Self {
        Self { router }
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a message to the user on any connected platform (e.g. Telegram). \
         Use this for mid-task progress updates, milestone notifications, or asking \
         the user a question while continuing work. Call with action='list' to see \
         available targets. Default is plain text; use parse_mode='markdown' for formatting."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["send", "list"],
                    "description": "'send' sends a message. 'list' returns available targets."
                },
                "platform": {
                    "type": "string",
                    "description": "Target platform: 'telegram'. Defaults to the session's platform."
                },
                "target": {
                    "type": "string",
                    "description": "Delivery target (chat ID). Omit to use the default/home channel."
                },
                "message": {
                    "type": "string",
                    "description": "The message text to send."
                },
                "parse_mode": {
                    "type": "string",
                    "enum": ["markdown", "html"],
                    "description": "Optional: 'markdown' converts to Telegram MarkdownV2. Omit for plain text."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let action = args["action"].as_str().unwrap_or("send");

        let result = match action {
            "list" => self.handle_list().await,
            "send" => self.handle_send(args).await,
            _ => Ok(ToolResult {
                tool_name: self.name().to_string(),
                success: false,
                output: serde_json::json!({"error": format!("Unknown action: '{}'. Use 'send' or 'list'.", action)}),
                error: Some(format!("Unknown action: '{}'", action)),
                duration_ms: start.elapsed().as_millis() as u64,
            }),
        };

        result.map(|mut r| {
            r.duration_ms = start.elapsed().as_millis() as u64;
            r
        })
    }
}

impl SendMessageTool {
    async fn handle_list(&self) -> Result<ToolResult> {
        let targets = self.router.list_targets().await;
        let connected = self.router.connected_platforms().await;

        Ok(ToolResult {
            tool_name: self.name().to_string(),
            success: true,
            output: serde_json::json!({
                "connected_platforms": connected,
                "targets": targets,
            }),
            error: None,
            duration_ms: 0,
        })
    }

    async fn handle_send(&self, args: serde_json::Value) -> Result<ToolResult> {
        let message = args["message"].as_str().unwrap_or("");
        if message.is_empty() {
            return Ok(ToolResult {
                tool_name: self.name().to_string(),
                success: false,
                output: serde_json::json!({"error": "No message provided"}),
                error: Some("Message is required for 'send' action".into()),
                duration_ms: 0,
            });
        }

        let parse_mode = args["parse_mode"].as_str().map(String::from);

        let msg = OutgoingMessage {
            platform: args["platform"].as_str().unwrap_or("").to_string(),
            target: args["target"].as_str().map(String::from),
            text: message.to_string(),
            parse_mode,
        };

        let result = self.router.send(&msg).await?;

        Ok(ToolResult {
            tool_name: self.name().to_string(),
            success: result.success,
            output: serde_json::to_value(&result).unwrap_or_default(),
            error: result.error,
            duration_ms: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split a long message into chunks that fit within a platform's character limit.
/// Tries to split at paragraph or sentence boundaries.
pub fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        // Find a good split point (paragraph > sentence > word > hard cut)
        let split_at = find_split_point(&remaining[..max_len]);
        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk.to_string());
        remaining = rest.trim_start();
    }

    chunks
}

/// Find the best split point within a string, preferring natural boundaries.
fn find_split_point(s: &str) -> usize {
    // Try double newline (paragraph boundary)
    if let Some(pos) = s.rfind("\n\n") {
        if pos > s.len() / 4 {
            return pos + 2;
        }
    }
    // Try single newline
    if let Some(pos) = s.rfind('\n') {
        if pos > s.len() / 4 {
            return pos + 1;
        }
    }
    // Try sentence boundary (. followed by space)
    if let Some(pos) = s.rfind(". ") {
        if pos > s.len() / 4 {
            return pos + 2;
        }
    }
    // Try word boundary
    if let Some(pos) = s.rfind(' ') {
        if pos > s.len() / 4 {
            return pos + 1;
        }
    }
    // Hard cut
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_short_message() {
        let chunks = split_message("Hello world", 100);
        assert_eq!(chunks, vec!["Hello world"]);
    }

    #[test]
    fn test_split_at_paragraph() {
        let text = "First paragraph.\n\nSecond paragraph that is longer.";
        let chunks = split_message(text, 30);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("First paragraph"));
    }

    #[test]
    fn test_split_at_word_boundary() {
        let text = "word ".repeat(100);
        let chunks = split_message(&text, 50);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= 50, "Chunk too long: {} chars", chunk.len());
        }
    }

    #[test]
    fn test_split_preserves_content() {
        let text = "a".repeat(100);
        let chunks = split_message(&text, 30);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }
}
