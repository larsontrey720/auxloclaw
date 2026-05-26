//! Coordination Module - Sub-agent orchestration and task delegation
//! Based on Claude Code's multi-agent architecture

pub mod sub_agent;
pub mod cost_aware_delegation;
pub mod task_delegation;

pub use sub_agent::{SubAgent, SubAgentConfig, SubAgentResult};
pub use task_delegation::{TaskDelegator, DelegatedTask, TaskPriority};

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::HashMap;

use crate::config::AppConfig;
use crate::providers::ProviderPool;
use crate::orchestrator::ToolOrchestrator;
use crate::memory::SessionStore;
use crate::error_recovery::{AgentError, RecoveryAction};

pub use cost_aware_delegation::{TokenBudget, CostAwareDelegator, ComplexityAnalyzer, TaskComplexity, DelegationDecision, DelegationStats};

/// Main agent coordinator
pub struct AgentCoordinator {
    /// Main agent instance
    main_agent: Arc<crate::agent::AgentCore>,
    /// Active sub-agents
    sub_agents: RwLock<HashMap<String, Arc<SubAgent>>>,
    /// Task delegator
    delegator: TaskDelegator,
    /// Cost-aware delegator (wraps budget and complexity analysis)
    cost_aware_delegator: RwLock<CostAwareDelegator>,
    /// Provider pool (shared)
    providers: Arc<ProviderPool>,
    /// Tool orchestrator (shared)
    orchestrator: Arc<ToolOrchestrator>,
    /// Session store
    session_store: Arc<SessionStore>,
    /// Configuration
    config: AppConfig,
}

/// Coordinator event for monitoring
#[derive(Debug, Clone)]
pub enum CoordinatorEvent {
    /// Task delegated to sub-agent
    TaskDelegated { task_id: String, agent_type: String },
    /// Sub-agent started
    SubAgentStarted { agent_id: String, task: String },
    /// Sub-agent completed
    SubAgentCompleted { agent_id: String, result: SubAgentResult },
    /// Sub-agent failed
    SubAgentFailed { agent_id: String, error: AgentError },
    /// All sub-agents done, aggregating results
    AggregatingResults { completed: usize, failed: usize },
    /// Final response ready
    ResponseReady { response: String },
}

impl AgentCoordinator {
    pub fn new(
        main_agent: Arc<crate::agent::AgentCore>,
        providers: Arc<ProviderPool>,
        orchestrator: Arc<ToolOrchestrator>,
        session_store: Arc<SessionStore>,
        config: AppConfig,
    ) -> Self {
        Self {
            main_agent,
            sub_agents: RwLock::new(HashMap::new()),
            delegator: TaskDelegator::new(),
            cost_aware_delegator: RwLock::new(CostAwareDelegator::new(TokenBudget::default())),
            providers,
            orchestrator,
            session_store,
            config,
        }
    }

    /// Spawn a sub-agent with isolated context
    pub async fn spawn_sub_agent(&self, config: SubAgentConfig) -> Result<Arc<SubAgent>> {
        let sub_agent = Arc::new(SubAgent::new(
            config,
            self.providers.clone(),
            self.orchestrator.clone(),
            self.session_store.clone(),
            self.config.clone(),
        )?);

        let agent_id = sub_agent.id().to_string();
        self.sub_agents.write().await.insert(agent_id, sub_agent.clone());

        Ok(sub_agent)
    }

    /// Delegate a task to appropriate sub-agent
    pub async fn delegate_task(&self, task: &str, agent_type: Option<&str>) -> Result<String> {
        // Analyze task to determine best agent type
        let agent_type = agent_type
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.delegator.classify_task(task));

        let config = SubAgentConfig {
            agent_type: agent_type.clone(),
            task: task.to_string(),
            isolated_context: true,
            timeout_secs: 60,
            max_tools: 10,
            override_model: None,
            override_base_url: None,
            override_api_key: None,
        };

        let sub_agent = self.spawn_sub_agent(config).await?;
        let result = sub_agent.execute().await?;

        // Store result in main context
        self.main_agent.add_to_history(
            &format!("sub_agent_{}", agent_type),
            "system",
            &format!("Sub-agent result: {}", result.summary()),
        ).await;

        Ok(result.response)
    }

    /// Execute multiple sub-agents in parallel
    pub async fn execute_parallel_sub_agents(
        &self,
        tasks: Vec<(String, String)>, // (task, agent_type)
    ) -> Vec<String> {
        let mut handles = Vec::new();

        for (task, agent_type) in tasks {
            let coordinator = Arc::new(self.clone_self());
            let task = task.clone();
            let agent_type = agent_type.clone();

            handles.push(tokio::spawn(async move {
                coordinator.delegate_task(&task, Some(&agent_type)).await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(e)) => results.push(format!("Error: {}", e)),
                Err(e) => results.push(format!("Join error: {}", e)),
            }
        }

        results
    }

    /// Get sub-agent by ID
    pub async fn get_sub_agent(&self, id: &str) -> Option<Arc<SubAgent>> {
        self.sub_agents.read().await.get(id).cloned()
    }

    /// List active sub-agents
    pub async fn list_sub_agents(&self) -> Vec<String> {
        self.sub_agents.read().await.keys().cloned().collect()
    }

    /// Terminate a sub-agent
    pub async fn terminate_sub_agent(&self, id: &str) -> Result<()> {
        if let Some(agent) = self.sub_agents.write().await.remove(id) {
            agent.terminate().await;
        }
        Ok(())
    }

    /// Clone self for spawn
    fn clone_self(&self) -> Self {
        Self {
            main_agent: self.main_agent.clone(),
            sub_agents: RwLock::new(HashMap::new()),
            delegator: TaskDelegator::new(),
            cost_aware_delegator: RwLock::new(CostAwareDelegator::new(TokenBudget::default())),
            providers: self.providers.clone(),
            orchestrator: self.orchestrator.clone(),
            session_store: self.session_store.clone(),
            config: self.config.clone(),
        }
    }

    /// Process with cost-aware delegation
    /// Returns the response and whether delegation occurred
    pub async fn process_with_cost_aware_delegation(
        &self,
        message: &str,
        session_id: Option<&str>,
    ) -> (String, bool) {
        // Get delegation decision with cost analysis
        let decision = self.cost_aware_delegator.write().await.should_delegate(message);
        
        tracing::info!(
            "Delegation decision: delegate={}, reason={}, complexity={}",
            decision.delegate,
            decision.reason,
            decision.complexity.score
        );

        if decision.delegate {
            let agent_type = self.delegator.classify_task(message);
            match self.delegate_task(message, Some(&agent_type)).await {
                Ok(result) => {
                    // Record usage
                    self.cost_aware_delegator.write().await.record_usage(
                        decision.complexity.estimated_sub_agent_tokens,
                        true
                    );
                    (result, true)
                }
                Err(e) => {
                    tracing::warn!("Sub-agent failed, falling back to main: {}", e);
                    (self.main_agent.process(message, session_id).await, false)
                }
            }
        } else {
            // Process with main agent
            (self.main_agent.process(message, session_id).await, false)
        }
    }

    /// Enable or disable sub-agents
    pub async fn set_sub_agents_enabled(&self, enabled: bool) {
        self.cost_aware_delegator.write().await.set_sub_agents_enabled(enabled);
    }

    /// Set minimum complexity threshold for delegation
    pub async fn set_min_delegation_complexity(&self, min: u32) {
        self.cost_aware_delegator.write().await.set_min_complexity(min);
    }

    /// Get delegation statistics
    pub async fn delegation_stats(&self) -> DelegationStats {
        self.cost_aware_delegator.read().await.stats()
    }

    /// Process with automatic delegation
    pub async fn process_with_delegation(
        &self,
        message: &str,
        session_id: Option<&str>,
    ) -> String {
        // Check if task should be delegated
        if self.delegator.should_delegate(message) {
            let agent_type = self.delegator.classify_task(message);
            match self.delegate_task(message, Some(&agent_type)).await {
                Ok(result) => result,
                Err(e) => {
                    // Fallback to main agent
                    tracing::warn!("Sub-agent failed, falling back to main: {}", e);
                    self.main_agent.process(message, session_id).await
                }
            }
        } else {
            // Process normally with main agent
            self.main_agent.process(message, session_id).await
        }
    }
}
