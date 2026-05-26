//! AUXLOCLAW - Ultra-High-Performance AI Agent Framework

mod agent;
mod auth;
mod capabilities;
mod channels;
mod checkpoints;
mod cli;
mod commands;
mod config;
mod context;
mod coordination;
mod error_recovery;
mod mcp;
mod memory;
mod orchestrator;
mod persona;
mod planner;
mod plugins;
mod providers;
mod runs;
mod scheduler;
mod skills;
mod streaming;
mod tools;

use crate::checkpoints::CheckpointManager;
use std::sync::Arc;
use std::time::Instant;

use crate::auth::{AuthConfig, AuthState};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;
use clap::Parser;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse_args();

    // Initialize logging
    let level = if args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(level))
        .init();

    // Handle commands
    match args.command {
        Commands::Gateway { port, host } => {
            run_gateway(&host, port).await?;
        }

        Commands::Chat {
            message,
            model,
            stream,
        } => {
            commands::chat::handle_chat(message, model, stream).await?;
        }

        Commands::Setup {
            quick,
            telegram,
            discord,
        } => {
            commands::setup::handle_setup(quick, telegram, discord)?;
        }

        Commands::Config { action } => {
            commands::config::handle_config(action)?;
        }

        Commands::Skill { action } => {
            commands::skill::handle_skill(action).await?;
        }

        Commands::Provider { action } => {
            commands::provider::handle_provider(action).await?;
        }

        Commands::Persona { action } => {
            commands::persona::handle_persona(action)?;
        }

        Commands::Status { delegation } => {
            commands::status::handle_status(delegation)?;
        }

        Commands::Code { task, project, session } => {
            commands::code::handle_code(task, project, session).await?;
        }

        Commands::Run {
            skill,
            args: run_args,
        } => {
            commands::run::handle_run(skill, run_args).await?;
        }

        Commands::Plan { goal, output } => {
            commands::plan::handle_plan(goal, output).await?;
        }

        Commands::RunPlan { path, db } => {
            commands::plan::handle_run_plan(path, db).await?;
        }

        Commands::Runs { action, db } => {
            commands::runs::handle_runs(action, db).await?;
        }

        Commands::Capabilities { json } => {
            commands::handle_capabilities(json);
        }

        Commands::Update => {
            let result = commands::update::handle_update().await;
            println!("{}", result);
        }

        Commands::Mcp { args } => {
            let args_str = args.join(" ");
            match crate::commands::mcp::handle_mcp(&args_str, None).await {
                Ok(result) => println!("{}", result),
                Err(e) => eprintln!("Error: {}", e),
            }
        }

        Commands::Token { args } => {
            let arg_str = args.join(" ");
            match crate::commands::token::handle_token(&arg_str) {
                Ok(resp) => println!("{}", resp),
                Err(e) => eprintln!("Error: {}", e),
            }
            return Ok(());
        }

        Commands::Stop => {
            commands::stop::handle_stop()?;
        }

        Commands::Model { model_id, base, key, reset, show } => {
            let session_db = dirs::home_dir()
                .map(|h| h.join(".auxloclaw/sessions"))
                .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?;
            let session_db_parent = session_db.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("~/.auxloclaw"));
            let model_store = memory::model_store::ModelStore::new(&session_db_parent)?;
            let user_id = "cli";
            let channel = "cli";
            if reset {
                if model_store.delete(channel, user_id)? {
                    println!("Model override cleared.");
                } else {
                    println!("No model override was set.");
                }
            } else if show || (model_id.is_none() && base.is_none() && key.is_none()) {
                let resp = commands::model::handle_model(&model_store, channel, user_id, "")?;
                println!("{}", resp);
            } else {
                let mut args = Vec::new();
                if let Some(ref m) = model_id { args.push(m.clone()); }
                if let Some(ref b) = base { args.push(format!("--base {}", b)); }
                if let Some(ref k) = key { args.push(format!("--key {}", k)); }
                let resp = commands::model::handle_model(&model_store, channel, user_id, &args.join(" "))?;
                println!("{}", resp);
            }
        }
    }

    Ok(())
}

#[derive(Clone)]
struct AppState {
    agent: Arc<agent::AgentCore>,
}

async fn run_gateway(host: &str, port: u16) -> anyhow::Result<()> {
    let auth_config = AuthConfig {
        api_key: std::env::var("AUXLOCLAW_API_KEY").ok(),
        require_auth: std::env::var("AUXLOCLAW_REQUIRE_AUTH")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false),
    };
    let auth_state = Arc::new(AuthState::new(auth_config));

    info!("🦞 AUXLOCLAW v{} initializing...", env!("CARGO_PKG_VERSION"));

    let start = Instant::now();

    // Load config
    let config_path = dirs::home_dir()
        .map(|h| h.join(".auxloclaw/config.toml"))
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?;
    let config =
        config::AppConfig::load(config_path.to_str().unwrap_or("~/.auxloclaw/config.toml"))?;

    // Expand tilde in database path
    let session_db = shellexpand::tilde(&config.memory.database_path).into_owned();
    let session_db_parent = std::path::Path::new(&session_db).parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("~/.auxloclaw"));

    // Initialize core components
    let memory = Arc::new(memory::MemoryEngine::new(&config.memory)?);
    let providers = Arc::new(providers::ProviderPool::new(config.providers.clone()));
    let mut raw_orchestrator = orchestrator::ToolOrchestrator::new();
    // Register coding workspace tools for /code mode
    raw_orchestrator.register_code_tools();
    let mut raw_plugins = plugins::PluginManager::new(config.plugins.clone());
    raw_plugins.set_tools(raw_orchestrator.list_tools());
    let plugins = Arc::new(raw_plugins);
    raw_orchestrator.set_plugins(plugins.clone());

    // Create message router for cross-platform proactive messaging
    let mut message_router = tools::MessageRouter::new();
    if config.channels.telegram.enabled {
        message_router.set_default_platform("telegram".to_string());
    }
    raw_orchestrator.register_send_message_tool(message_router.clone());

    // Create sub-agent coordinator placeholder (populated after agent init)
    let coordinator: Arc<tokio::sync::RwLock<Option<Arc<coordination::AgentCoordinator>>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    raw_orchestrator.register_subagent_tool(coordinator.clone());

    let orchestrator = Arc::new(raw_orchestrator);
    if config.mcp.enabled {
        let count = orchestrator.register_mcp_tools(&config.mcp).await?;
        info!("🔌 Registered {} MCP tools", count);
    }

    // Initialize persistent session store
    let session_store = Arc::new(memory::SessionStore::new(&session_db)?);
    let code_mode = Arc::new(memory::CodeModeStore::new(&session_db)?);
    let model_store = Arc::new(memory::model_store::ModelStore::new(&session_db_parent)?);
    let checkpoint_manager = Arc::new(CheckpointManager::new(&session_db)?);

    plugins.run_lifecycle(plugins::HookEvent::Startup).await;

    let agent = Arc::new(agent::AgentCore::new(
        memory,
        providers.clone(),
        orchestrator.clone(),
        config.clone(),
        session_store.clone(),
        code_mode.clone(),
        model_store.clone(),
        plugins.clone(),
        checkpoint_manager.clone(),
    )?);

    // Load persisted sessions
    agent.load_sessions().await?;
    
    // Now initialize the sub-agent coordinator with all dependencies
    let coordinator_instance = Arc::new(coordination::AgentCoordinator::new(
        agent.clone(),
        providers.clone(),
        orchestrator.clone(),
        session_store.clone(),
        config.clone(),
    ));
    {
        let mut coord_guard = coordinator.write().await;
        *coord_guard = Some(coordinator_instance);
    }
    tracing::info!("Sub-agent coordinator initialized");

    let _cron_scheduler =
        scheduler::CronScheduler::start(agent.clone(), config.scheduler.clone()).await?;

    info!("⚡ Core initialized in {:?}", start.elapsed());

    // Zombie child reaper - reap orphaned child processes every 30s
    #[cfg(unix)]
    tokio::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            unsafe {
                while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
            }
        }
    });

    // Spawn reflection monitor background task
    let monitor_agent = agent.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            // Check for sessions needing reflection
            let sessions = monitor_agent.get_sessions_needing_reflection().await;

            for session_key in sessions {
                tracing::info!(
                    "Auto-reflection triggered for inactive session: {}",
                    session_key
                );
                if let Some(reflection) = monitor_agent.run_reflection(&session_key).await {
                    tracing::info!(
                        "Auto-reflection complete: {} - {}",
                        reflection.reflection_type.to_string().to_lowercase(),
                        reflection.title
                    );
                }
            }
        }
    });

    // Start Telegram channel if enabled
    let model_store_discord = model_store.clone();
    let code_mode_discord = code_mode.clone();
    let _discord_handle = if config.channels.discord.enabled {
        let discord_agent = agent.clone();
        let discord_config = config.channels.discord.clone();
        info!("💬 Starting Discord gateway...");
        Some(tokio::spawn(async move {
            if let Err(e) = channels::discord::start(discord_agent, model_store_discord, code_mode_discord, Some(discord_config)).await {
                tracing::error!("Discord error: {}", e);
            }
        }))
    } else {
        None
    };

    let _tg_handle = if config.channels.telegram.enabled {
        let tg_agent = agent.clone();
        let tg_config = config.channels.telegram.clone();
        let tg_persona = config.persona.clone();
        let tg_router = Arc::new(message_router);
        info!("📱 Starting Telegram gateway...");
        Some(tokio::spawn(async move {
            if let Err(e) = channels::telegram::start(tg_agent, model_store.clone(), code_mode.clone(), Some(tg_config), tg_persona, Some(tg_router)).await {
                tracing::error!("Telegram error: {}", e);
            }
        }))
    } else {
        None
    };

    let state = AppState {
        agent: agent.clone(),
    };

    let auth_middleware_state = auth_state.clone();
    let auth_middleware = axum::middleware::from_fn(move |req: Request, next: Next| {
        let auth_state = auth_middleware_state.clone();
        async move {
            if let Err(e) = auth_state.verify_bearer_token(
                req.headers()
                    .get("authorization")
                    .and_then(|h| h.to_str().ok()),
            ) {
                return Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({"error": "Unauthorized", "message": e.to_string()})
                            .to_string(),
                    ))
                    .unwrap();
            }
            next.run(req).await
        }
    });

    // Build HTTP router
    let app = Router::new()
        .route("/health", axum::routing::get(|| async { "OK" }))
        .route("/chat", axum::routing::post(chat_handler))
        .route("/api/chat", axum::routing::post(chat_handler))
        .route("/api/status", axum::routing::get(status_handler))
        .route(
            "/api/capabilities",
            axum::routing::get(capabilities_handler),
        )
        .route("/api/skills", axum::routing::get(list_skills_handler))
        .route("/api/reflect", axum::routing::post(reflect_handler))
        .route(
            "/api/reflections",
            axum::routing::get(list_reflections_handler),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .layer(auth_middleware)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Start server
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("🌐 HTTP server listening on {}", addr);
    info!("✅ Ready in {:?}", start.elapsed());

    axum::serve(listener, app).await?;

    Ok(())
}

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Serialize)]
struct ChatResponse {
    response: String,
}

async fn chat_handler(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<ChatRequest>,
) -> axum::Json<ChatResponse> {
    let agent = state.agent;
    let response = agent.process(&req.message, None).await;
    axum::Json(ChatResponse { response })
}

async fn status_handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": "0.1.0",
        "uptime": "running"
    }))
}

async fn capabilities_handler(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    axum::Json(state.agent.capability_manifest().as_json())
}

async fn list_skills_handler() -> axum::Json<Vec<String>> {
    axum::Json(vec![
        "code-review".into(),
        "arxiv".into(),
        "fine-tuning-axolotl".into(),
    ])
}

#[derive(Deserialize)]
struct ReflectRequest {
    session_id: Option<String>,
}

async fn reflect_handler(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<ReflectRequest>,
) -> axum::Json<serde_json::Value> {
    let agent = state.agent;
    let session_key = req.session_id.unwrap_or_else(|| "default".to_string());

    match agent.run_reflection(&session_key).await {
        Some(reflection) => axum::Json(
            serde_json::to_value(&reflection)
                .unwrap_or_else(|_| serde_json::json!({"error": "Failed to serialize reflection"})),
        ),
        None => axum::Json(serde_json::json!({
            "error": "Reflection not triggered (check min_messages or cooldown)"
        })),
    }
}

async fn list_reflections_handler(
    State(state): State<AppState>,
) -> axum::Json<Vec<serde_json::Value>> {
    let agent = state.agent;
    match agent.get_all_reflections() {
        Some(reflections) => axum::Json(
            reflections
                .iter()
                .map(|r| serde_json::to_value(r).unwrap_or_default())
                .collect(),
        ),
        None => axum::Json(vec![]),
    }
}

mod streaming_agent;
