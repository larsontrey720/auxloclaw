//! Chat command handler

use crate::checkpoints::CheckpointManager;
use crate::plugins::PluginManager;
use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, History, Input};
use std::sync::Arc;

pub async fn handle_chat(
    message: Option<String>,
    _model: Option<String>,
    _stream: bool,
) -> Result<()> {
    // Load config
    let config_path = dirs::home_dir()
        .map(|h| h.join(".auxloclaw/config.toml"))
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?;
    let config =
        crate::config::AppConfig::load(config_path.to_str().unwrap_or("~/.auxloclaw/config.toml"))?;

    // Initialize components
    let memory = Arc::new(crate::memory::MemoryEngine::new(&config.memory)?);
    let providers = Arc::new(crate::providers::ProviderPool::new(
        config.providers.clone(),
    ));
    let plugins = Arc::new(PluginManager::new(config.plugins.clone()));
    let orchestrator = Arc::new(crate::orchestrator::ToolOrchestrator::new());

    // Initialize session store
    let session_db = shellexpand::tilde(&config.memory.database_path).into_owned();
    let session_store = Arc::new(crate::memory::SessionStore::new(&session_db)?);
        let data_dir = std::path::Path::new(&session_db).parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("~/.auxloclaw"));
    let model_store = Arc::new(crate::memory::model_store::ModelStore::new(&data_dir)?);
    let code_mode = Arc::new(crate::memory::CodeModeStore::new(
            &config.memory.database_path
        )?);
    let checkpoint_manager = Arc::new(CheckpointManager::new(&session_db)?);

    let agent = Arc::new(crate::agent::AgentCore::new(
        memory,
        providers,
        orchestrator,
        config.clone(),
        session_store,
        code_mode,
        model_store,
        plugins.clone(),
        checkpoint_manager.clone(),
        Arc::new(parking_lot::RwLock::new((None, None))),
    )?);

    match message {
        Some(msg) => {
            // One-shot mode (no history)
            let response = agent.process(&msg, None).await;
            println!("{}", response);
        }
        None => {
            // Interactive mode with history
            println!("\n🦞 AUXLOCLAW Chat (type 'exit' to quit, 'help' for commands)\n");

            let mut history = dialoguer::BasicHistory::new();

            loop {
                let input: String =
                    dialoguer::Input::with_theme(&dialoguer::theme::ColorfulTheme::default())
                        .with_prompt("You")
                        .history_with(&mut history)
                        .interact_text()?;

                match input.trim() {
                    "exit" | "quit" | "q" => {
                        println!("Goodbye!");
                        break;
                    }
                    "help" | "?" => {
                        println!("\nCommands:");
                        println!("  exit, quit, q  - Exit chat");
                        println!("  help, ?        - Show this help");
                        println!("  clear          - Clear history");
                        println!("  status         - Show session status");
                        println!();
                        continue;
                    }
                    "clear" => {
                        history = dialoguer::BasicHistory::new();
                        // Also clear agent session
                        let _ = agent.clear_session("cli:default").await;
                        println!("History cleared.");
                        continue;
                    }
                    "status" => {
                        println!("\nSession Status:");
                        println!("  Model: {}", config.agent.default_model);
                        println!();
                        continue;
                    }
                    "" => continue,
                    _ => {}
                }

                // Process message with CLI session (history enabled)
                let response = agent.process(&input, None).await;
                println!("\n{}\n", response);
            }
        }
    }

    Ok(())
}
