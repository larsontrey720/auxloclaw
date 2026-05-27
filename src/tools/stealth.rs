use async_trait::async_trait;
use anyhow::{anyhow, Result};
use std::process::Command;
use std::time::Instant;

use crate::orchestrator::{Tool, ToolResult};

pub struct StealthFetchTool;

const HELPER_SCRIPT: &str = "/usr/local/share/auxloclaw/stealth_fetch_helper.py";

#[async_trait]
impl Tool for StealthFetchTool {
    fn name(&self) -> &str { "stealth_fetch" }

    fn description(&self) -> &str {
        "Fetch a URL that blocks normal HTTP clients (Vercel Security Checkpoint, Cloudflare bot detection, JS challenges). \
         Uses Scrapling's real-browser automation with TLS fingerprint spoofing to bypass protections that block curl, httpx, and standard fetch tools. \
         Use when: web_fetch returns 403/redirect loops, you see 'Checking your browser...' pages, \
         or the target is behind Cloudflare/Vercel protection. \
         Modes: 'stealth' (default, bypasses Cloudflare/Vercel), 'simple' (fast, TLS-only), 'dynamic' (full JS rendering)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "mode": {
                    "type": "string",
                    "description": "Fetch mode: 'stealth' (bypass Cloudflare/Vercel, default), 'simple' (fast TLS fingerprint), 'dynamic' (full JS render)",
                    "enum": ["stealth", "simple", "dynamic"],
                    "default": "stealth"
                },
                "method": {
                    "type": "string",
                    "description": "HTTP method: GET (default) or POST",
                    "enum": ["GET", "POST"],
                    "default": "GET"
                },
                "body": {
                    "type": "string",
                    "description": "Request body for POST requests (JSON string or form data)"
                },
                "headers": {
                    "type": "string",
                    "description": "Additional headers as JSON object string, e.g. '{\"Authorization\": \"Bearer token\"}'"
                },
                "css_selector": {
                    "type": "string",
                    "description": "Optional CSS selector to extract specific elements from the page"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let url = args.get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing required parameter: url"))?;

        let mode = args.get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("stealth");

        let method = args.get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET");

        let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let headers_json = args.get("headers").and_then(|v| v.as_str()).unwrap_or("{}");
        let css_selector = args.get("css_selector").and_then(|v| v.as_str()).unwrap_or("");

        tracing::info!("stealth_fetch: {} {} (mode={}, selector={:?})", method, url, mode, css_selector);

        check_scrapling()?;

        let mut cmd = Command::new("python3");
        cmd.arg(HELPER_SCRIPT)
            .arg("--mode").arg(mode)
            .arg("--method").arg(method)
            .arg("--url").arg(url);

        if !body.is_empty() {
            cmd.arg("--body").arg(body);
        }
        if headers_json != "{}" {
            cmd.arg("--headers").arg(headers_json);
        }
        if !css_selector.is_empty() {
            cmd.arg("--selector").arg(css_selector);
        }

        let output = cmd.output()
            .map_err(|e| anyhow!("Failed to run stealth fetch: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let duration_ms = start.elapsed().as_millis() as u64;

        if !output.status.success() {
            let err_msg = if !stderr.is_empty() {
                stderr.to_string()
            } else {
                format!("stealth_fetch exited with code {:?}", output.status.code())
            };
            return Ok(ToolResult {
                tool_name: "stealth_fetch".to_string(),
                success: false,
                output: serde_json::Value::String(err_msg),
                error: Some(format!("Exit code {:?}", output.status.code())),
                duration_ms,
            });
        }

        let result = stdout.trim().to_string();
        let max_chars = 50_000;
        let display = if result.len() > max_chars {
            format!("{}...\n\n[Truncated: {} total chars, showing first {}]",
                &result[..max_chars], result.len(), max_chars)
        } else {
            result
        };

        Ok(ToolResult {
            tool_name: "stealth_fetch".to_string(),
            success: true,
            output: serde_json::Value::String(display),
            error: None,
            duration_ms,
        })
    }
}

fn check_scrapling() -> Result<()> {
    let output = Command::new("python3")
        .args(["-c", "import scrapling; print(scrapling.__version__)"])
        .output()
        .map_err(|e| anyhow!("Python3 not available: {}", e))?;

    if !output.status.success() {
        return Err(anyhow!(
            "Scrapling not installed. Run: pip install 'scrapling[all]>=0.4.7' && scrapling install"
        ));
    }
    Ok(())
}
