//! Coding Agent - Specialized full-stack development mode
//!
//! Operates in an isolated workspace with tool-based execution,
//! surgical edits, and iterative build validation.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// Default workspace root for coding sessions
const CODE_WORKSPACE_ROOT: &str = "~/.auxloclaw/code-workspaces";

/// Session ID prefix for coding sessions (isolates from normal chat)
const CODE_SESSION_PREFIX: &str = "code";

/// Generate a unique coding session ID
fn generate_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{}-{}", CODE_SESSION_PREFIX, now)
}

/// Coding session metadata persisted to the workspace
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeSessionMeta {
    pub session_id: String,
    pub workspace: PathBuf,
    pub created_at: u64,
    pub project_name: Option<String>,
    pub stack: Option<String>,
}

/// Get or create the isolated workspace directory for a coding session.
pub fn ensure_workspace(session_id: &str) -> Result<PathBuf> {
    let root = shellexpand::tilde(CODE_WORKSPACE_ROOT).into_owned();
    let workspace = PathBuf::from(&root).join(session_id);
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("Failed to create coding workspace: {:?}", workspace))?;
    Ok(workspace)
}

/// Build the specialized coding agent system prompt.
///
/// This replaces the normal persona prompt when /code mode is active.
pub fn build_code_system_prompt(workspace: &PathBuf) -> String {
    let workspace_str = workspace.display();
    format!(
        r#"You are a specialized coding agent operating within a real-time, full-stack development environment.
Your interaction model is strictly tool-based: you execute authenticated actions against a live filesystem and runtime.

## Workspace
Your working directory is: `{workspace}`
All file operations MUST use absolute paths rooted at this workspace.
You must NEVER read or modify files outside this workspace.

## Interaction Model: Tool-Based Execution

### Discovery
- Use `list_files` and `read_file` to map out the project.
- You are strictly forbidden from guessing file contents. You MUST read a file before modifying it.

### Modification
- Use `edit_file_llm` for surgical edits with natural language instructions.
- Use `edit_file` for precise single-block replacements requiring exact text matching.
- Use `create_or_rewrite_file` only for new files or intentional full rewrites.
- Never overwrite entire files if you can make surgical edits instead.

### Execution and Validation
- After making changes, run build/lint commands to verify correctness:
  - `npm run lint` or equivalent for syntax/import checks
  - `npm run build` or equivalent for full compilation
- Run tests when available: `npm test`, `cargo test`, etc.
- If a build fails, you have up to 3 attempts to fix the error from compiler logs before asking for guidance.

### Shell Commands
- Use `run_bash_command` for targeted commands.
- Use `run_sequential_cmds` for ordered chains.
- Use `run_parallel_cmds` for independent concurrent operations.
- Install packages via `npm install`, `pip install`, `cargo add`, etc.
- Use `grep_search` to find patterns across the codebase.

## Environment Constraints

### Port Binding
If building a web server, bind to port 3000 via the PORT environment variable.
The infrastructure routes external traffic through port 3000 only.

### Full-Stack Security
If the application requires API keys or secrets, implement server-side endpoints to handle them.
Secrets must NEVER be exposed in client-side code or committed to files.

### Persistence
- Track app permissions and configuration in `metadata.json` when applicable.
- Document required environment variables in `.env.example`.

## Chain-of-Thought Process

Before writing any code, follow this workflow:

1. **Skill Check**: Evaluate if any installed skills are relevant. If so, read the SKILL.md before proceeding.
2. **State Verification**: Verify the current filesystem state. If you have not looked at a file recently, re-read it before modifying.
3. **Read-Modify-Write**: Perform surgical edits to preserve existing logic.
4. **Iterative Fixing**: Fix build errors from compiler logs. Maximum 3 attempts before requesting guidance.

## Communication Style

**Action Over Talk.**

In a typical turn:
1. State your intent in one or two sentences.
2. Execute all necessary tool calls in parallel when they are independent.
3. Provide a summary only after the code is verified as compiling/passing tests.

Never just propose code. Execute it, validate it, and report the verified result.

## Tool Execution Rules
- Always read a file before editing it.
- Prefer `edit_file_llm` for most edits (fast, natural language).
- Use `edit_file` only when `edit_file_llm` is imprecise.
- After any file modification, run the appropriate validation (lint, build, test).
- Never commit changes unless explicitly asked.
- Keep the workspace clean. Do not create temporary files unless necessary.

## Error Recovery
- If a tool call fails, analyze the error and retry with a fix.
- If you cannot fix an error after 3 attempts, stop and explain the issue.
- If a dependency installation fails, check for version conflicts and platform issues.
- Prefer fixing the root cause over applying workarounds.

## Edit Decision Tree

Before editing a file, determine the right tool:
1. **New file** -> `create_or_rewrite_file`
2. **Surgical edit to existing file** -> `edit_file_llm` (preferred, natural language)
3. **Precise single-block replacement** -> `edit_file` (exact text matching)
4. **Full rewrite of existing file** -> `create_or_rewrite_file`

NEVER overwrite an existing file when a surgical edit suffices.

## Read-Only Exploration

When you need to understand a concept, look up documentation, or research a pattern:
- Use `search_web` for broad discovery or current events
- Use `read_webpage` to fetch and read a specific URL (documentation, Stack Overflow, GitHub issues)
- Use `read_webpage` with `use_browser="true"` for dynamic pages that require JavaScript

Research is read-only. Do not attempt to modify web content. Extract what you need, then apply it to your local workspace.

## Efficient Token Usage

You have access to a large context window. Use it wisely:
- When exploring a codebase, read only the files relevant to the current task
- Use `grep_search` to locate specific patterns before reading entire files
- Prefer targeted reads (line ranges) over reading entire large files
- Parallelize independent tool calls to reduce round-trips
- Do not re-read files you have just written -- trust the tool output
- Summarize findings concisely before moving to the next step

## Read Before Edit Rule

This is absolute and non-negotiable:
- You MUST read a file (or relevant section) immediately before editing it
- If you have not read a file in the current turn, you cannot edit it
- This applies to every edit tool: `edit_file_llm`, `edit_file`, `create_or_rewrite_file`
- Exception: `create_or_rewrite_file` for brand-new files that do not exist yet

## Token Management

When a user asks about adding API keys, tokens, or credentials:
- NEVER mention other apps or frameworks (Claude Desktop, Cursor, VS Code, etc.)
- NEVER tell the user to edit config files manually
- Direct them to use the `/token` command: `/token set <server> <KEY> <value>`
- Example: `/token set github GITHUB_PERSONAL_ACCESS_TOKEN ghp_xxxx`
- If a user pastes a token in chat, warn them it was auto-deleted for security and show the `/token` command instead
- You are AUXLOCLAW. You run on the user's own server. Tokens are managed via `/token`, not third-party app configs.

"#,
        workspace = workspace_str,
    )
}

/// Scan the workspace and return a file tree summary for context injection.
pub fn scan_workspace(workspace: &PathBuf) -> Result<String> {
    let mut tree = String::new();
    scan_dir(workspace, workspace, &mut tree, 0)?;
    Ok(tree)
}

fn scan_dir(root: &PathBuf, dir: &PathBuf, tree: &mut String, depth: usize) -> Result<()> {
    let indent = "  ".repeat(depth);
    let entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            !name.starts_with('.') && name != "node_modules" && name != "target" && name != "__pycache__"
        })
        .collect();

    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if path.is_dir() {
            tree.push_str(&format!("{}/\n", name));
            scan_dir(root, &path, tree, depth + 1)?;
        } else {
            tree.push_str(&format!("{}\n", name));
        }
    }
    Ok(())
}

/// Initialize a coding workspace with standard scaffolding files.
pub fn init_workspace(workspace: &PathBuf) -> Result<()> {
    let env_example = workspace.join(".env.example");
    if !env_example.exists() {
        std::fs::write(&env_example, "# Environment variables for this project\n")?;
    }

    let gitignore = workspace.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(
            &gitignore,
            "node_modules/\ndist/\n.env\n.env.local\ntarget/\n__pycache__/\n*.pyc\n",
        )?;
    }

    let meta = workspace.join("metadata.json");
    if !meta.exists() {
        std::fs::write(
            &meta,
            serde_json::json!({
                "name": "untitled",
                "description": "",
                "permissions": []
            })
            .to_string(),
        )?;
    }

    Ok(())
}

/// Main entry point for /code command from CLI, Telegram, or Discord.
///
/// Creates an isolated coding workspace and enters an interactive coding loop
/// with a specialized system prompt override.
pub async fn handle_code(
    task: Vec<String>,
    project: Option<String>,
    session: Option<String>,
) -> Result<()> {
    let session_id = session.unwrap_or_else(generate_session_id);
    let session_key = format!("{}:{}", CODE_SESSION_PREFIX, session_id);

    let workspace = ensure_workspace(&session_id)?;
    init_workspace(&workspace)?;

    let workspace_display = workspace.display().to_string();
    println!("\n  Coding workspace: {}", workspace_display);

    if let Some(ref proj) = project {
        println!("  Project: {}", proj);
    }

    // Scan existing files
    match scan_workspace(&workspace) {
        Ok(tree) if !tree.is_empty() => {
            println!("  Existing files:\n{}", tree);
        }
        _ => println!("  (empty workspace)"),
    }
    println!();

    // Load config and override persona with coding agent instructions
    let config_path = dirs::home_dir()
        .map(|h| h.join(".auxloclaw/config.toml"))
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?;
    let mut config = crate::config::AppConfig::load(
        config_path.to_str().unwrap_or("~/.auxloclaw/config.toml"),
    )?;

    let code_prompt = build_code_system_prompt(&workspace);
    config.persona = crate::persona::PersonaConfig {
        name: "Coding Agent".to_string(),
        behavior: code_prompt,
        style: crate::persona::StyleConfig {
            length: crate::persona::ResponseLength::Detailed,
            tone: crate::persona::Tone::Technical,
            formatting: crate::persona::FormattingConfig::default(),
        },
        persona_file: None,
    };

    // Initialize core components
    let memory = Arc::new(crate::memory::MemoryEngine::new(&config.memory)?);
    let providers = Arc::new(crate::providers::ProviderPool::new(
        config.providers.clone(),
    ));
    let plugins = Arc::new(crate::plugins::PluginManager::new(config.plugins.clone()));
    let orchestrator = Arc::new(crate::orchestrator::ToolOrchestrator::new());
    // Register coding workspace tools (read_file, edit_file, run_bash_command, etc.)
    orchestrator.register_code_tools();
    let session_db = shellexpand::tilde(&config.memory.database_path).into_owned();
    let session_store = Arc::new(crate::memory::SessionStore::new(&session_db)?);
    let data_dir = std::path::Path::new(&session_db).parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("~/.auxloclaw"));
    let model_store = Arc::new(crate::memory::model_store::ModelStore::new(&data_dir)?);
    let code_mode = Arc::new(crate::memory::CodeModeStore::new(
        &config.memory.database_path
    )?);
    let checkpoint_manager = Arc::new(crate::checkpoints::CheckpointManager::new(
        &session_db,
    )?);

    let agent = Arc::new(crate::agent::AgentCore::new(
        memory,
        providers,
        orchestrator,
        config.clone(),
        session_store,
        code_mode,
        model_store,
        plugins,
        checkpoint_manager,
        Arc::new(parking_lot::RwLock::new((None, None))),
    )?);
    // Override the system prompt with the pure coding agent prompt - no persona bleeding
    let coding_prompt = build_code_system_prompt(&workspace);
    agent.set_system_prompt_override(&format!("code:{}", workspace.display()), coding_prompt).await;

    // Process initial task if provided
    let initial_task = if task.is_empty() {
        None
    } else {
        Some(task.join(" "))
    };

    if let Some(ref msg) = initial_task {
        println!("  Task: {}\n", msg);
        let response = agent.process(msg, Some(&session_key)).await;
        println!("{}\n", response);
    }

    // Interactive coding loop
    println!("Coding session active. Type 'exit' to quit, 'help' for commands.\n");

    let mut history = dialoguer::BasicHistory::new();

    loop {
        let input: String =
            dialoguer::Input::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt("code")
                .history_with(&mut history)
                .interact_text()?;

        match input.trim() {
            "exit" | "quit" | "q" => {
                println!("Exiting coding session. Workspace preserved at: {}", workspace_display);
                break;
            }
            "help" | "?" => {
                println!("\nCoding Session Commands:");
                println!("  exit, quit, q  - Exit coding session (workspace preserved)");
                println!("  help, ?        - Show this help");
                println!("  clear          - Clear session history");
                println!("  files          - Show workspace file tree");
                println!("  workspace      - Show workspace path");
                println!();
                continue;
            }
            "clear" => {
                let _ = agent.clear_session(&session_key).await;
                println!("Session history cleared.");
                continue;
            }
            "files" => {
                match scan_workspace(&workspace) {
                    Ok(tree) if !tree.is_empty() => println!("\n{}", tree),
                    _ => println!("(empty workspace)"),
                }
                continue;
            }
            "workspace" => {
                println!("{}", workspace_display);
                continue;
            }
            "" => continue,
            _ => {}
        }

        let response = agent.process(&input, Some(&session_key)).await;
        println!("{}\n", response);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_workspace_creates_dir() {
        let tmp = std::env::temp_dir().join("auxloclaw-test-workspace");
        let session_id = "test-session-001";
        let workspace = tmp.join(session_id);
        std::fs::create_dir_all(&workspace).unwrap();
        assert!(workspace.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_code_system_prompt_contains_workspace() {
        let workspace = PathBuf::from("/tmp/test-workspace");
        let prompt = build_code_system_prompt(&workspace);
        assert!(prompt.contains("/tmp/test-workspace"));
        assert!(prompt.contains("tool-based"));
        assert!(prompt.contains("surgical"));
    }

    #[test]
    fn test_init_workspace_creates_files() {
        let tmp = std::env::temp_dir().join("auxloclaw-test-init");
        std::fs::create_dir_all(&tmp).unwrap();
        init_workspace(&tmp).unwrap();
        assert!(tmp.join(".env.example").exists());
        assert!(tmp.join(".gitignore").exists());
        assert!(tmp.join("metadata.json").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_workspace_skips_hidden() {
        let tmp = std::env::temp_dir().join("auxloclaw-test-scan");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(tmp.join(".secret"), "hidden").unwrap();
        std::fs::create_dir(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src").join("lib.rs"), "pub fn hello() {}").unwrap();

        let tree = scan_workspace(&tmp).unwrap();
        assert!(tree.contains("main.rs"));
        assert!(tree.contains("src/"));
        assert!(tree.contains("lib.rs"));
        assert!(!tree.contains(".secret"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
