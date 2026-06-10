//! Pipeline — 并行 DAG 步骤执行器

/// Pipeline 执行状态
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineState {
    Pending,
    Running,
    Completed,
    Failed(String),
}

/// 单步执行结果
#[derive(Debug, Clone)]
pub struct StepResult {
    pub step_id: String,
    pub success: bool,
    pub output: String,
}

/// 步骤类型
#[derive(Debug, Clone)]
pub enum StepKind {
    /// 原生脚本（当前用 tokio::process::Command 执行）
    Script(String),
    /// 工具调用（未来：通过 ToolRegistry 执行）
    ToolCall { tool: String, params: String },
    /// 条件分支（当前占位）
    Condition(String),
    /// 子 Pipeline
    SubPipeline(String),
}

/// 工作流中的一个步骤
#[derive(Debug, Clone)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    pub depends_on: Vec<String>,
    pub timeout_secs: u64,
}

impl Step {
    pub fn new(id: impl Into<String>, kind: StepKind) -> Self {
        Self {
            id: id.into(),
            kind,
            depends_on: Vec::new(),
            timeout_secs: 300,
        }
    }

    pub fn with_deps(mut self, deps: Vec<String>) -> Self {
        self.depends_on = deps;
        self
    }
}

/// Pipeline — 有序步骤序列
#[derive(Clone)]
pub struct Pipeline {
    pub id: String,
    pub steps: Vec<Step>,
    /// Optional tool registry for ToolCall steps
    pub tool_registry: Option<std::sync::Arc<crate::tool::ToolRegistry>>,
    /// Sub-pipelines (referenced by StepKind::SubPipeline)
    pub sub_pipelines: std::collections::HashMap<String, Pipeline>,
}

/// Pipeline 执行结果
#[derive(Debug, Clone)]
pub struct PipelineRunResult {
    pub pipeline_id: String,
    pub state: PipelineState,
    pub results: Vec<StepResult>,
}

impl Pipeline {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            steps: Vec::new(),
            tool_registry: None,
            sub_pipelines: std::collections::HashMap::new(),
        }
    }

    /// Attach a tool registry for executing ToolCall steps.
    pub fn with_tool_registry(mut self, registry: std::sync::Arc<crate::tool::ToolRegistry>) -> Self {
        self.tool_registry = Some(registry);
        self
    }

    /// Register a sub-pipeline.
    pub fn add_sub_pipeline(&mut self, pipeline: Pipeline) {
        self.sub_pipelines.insert(pipeline.id.clone(), pipeline);
    }

    pub fn add_step(&mut self, step: Step) {
        self.steps.push(step);
    }

    /// 执行所有步骤（按依赖关系拓扑排序，检测环形依赖死锁，独立步骤并发执行）
    pub fn run(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = PipelineRunResult> + Send + '_>> {
        self.run_inner()
    }

    // 递归 DAG 执行体：必须用 Box::pin 签名而非 async fn，使递归调用保持 Send
    fn run_inner(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = PipelineRunResult> + Send + '_>> {
        Box::pin(async move {
        let mut results: Vec<StepResult> = Vec::new();
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();

        while results.len() < self.steps.len() {
            let ready: Vec<&Step> = self.steps.iter()
                .filter(|s| !done.contains(s.id.as_str()))
                .filter(|s| s.depends_on.iter().all(|d| done.contains(d.as_str())))
                .collect();

            if ready.is_empty() {
                return PipelineRunResult {
                    pipeline_id: self.id.clone(),
                    state: PipelineState::Failed("circular dependency detected".into()),
                    results,
                };
            }

            let futs: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = StepResult> + Send>>> = ready.iter().map(|step| {
                let step_id = step.id.clone();
                let timeout_secs = step.timeout_secs;
                let timeout = std::time::Duration::from_secs(timeout_secs);
                let registry = self.tool_registry.clone();
                let pipeline_id = self.id.clone();

                let fut: std::pin::Pin<Box<dyn std::future::Future<Output = StepResult> + Send>> = match &step.kind {
                    StepKind::Script(script) => {
                        let script = script.clone();
                        Box::pin(async move {
                            let cmd_future = tokio::process::Command::new("sh")
                                .arg("-c").arg(&script).output();
                            match tokio::time::timeout(timeout, cmd_future).await {
                                Ok(Ok(o)) => StepResult {
                                    step_id, success: o.status.success(),
                                    output: String::from_utf8_lossy(&o.stdout).to_string(),
                                },
                                Ok(Err(e)) => StepResult {
                                    step_id, success: false, output: format!("exec error: {e}"),
                                },
                                Err(_) => StepResult {
                                    step_id, success: false,
                                    output: format!("timeout after {}s", timeout_secs),
                                },
                            }
                        })
                    }
                    StepKind::ToolCall { tool, params } => {
                        let tool = tool.clone();
                        let params = params.clone();
                        Box::pin(async move {
                            if let Some(ref registry) = registry {
                                let tool_id = abacus_types::ToolId(tool.clone());
                                let params_val: serde_json::Value = serde_json::from_str(&params)
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                                let exec_ctx = crate::tool::ExecutionContext::noop(&pipeline_id);
                                let exec_future = registry.execute(&tool_id, params_val, &exec_ctx);
                                match tokio::time::timeout(timeout, exec_future).await {
                                    Ok(Ok(o)) => StepResult {
                                        step_id, success: o.success, output: o.output.to_string(),
                                    },
                                    Ok(Err(e)) => StepResult {
                                        step_id, success: false, output: format!("tool error: {e}"),
                                    },
                                    Err(_) => StepResult {
                                        step_id, success: false,
                                        output: format!("tool timeout after {}s: {tool}", timeout_secs),
                                    },
                                }
                            } else {
                                StepResult {
                                    step_id, success: false,
                                    output: format!("no tool registry configured for: {tool}"),
                                }
                            }
                        })
                    }
                    StepKind::Condition(cond) => {
                        let cond = cond.clone();
                        Box::pin(async move {
                            StepResult {
                                step_id, success: true,
                                output: format!("condition evaluated: {cond}"),
                            }
                        })
                    }
                    StepKind::SubPipeline(sub_id) => {
                        let sub = self.sub_pipelines.get(sub_id).cloned();
                        let sub_id = sub_id.clone();
                        Box::pin(async move {
                            if let Some(sub_pipeline) = sub {
                                // 在独立 tokio task 中递归执行，确保 Send
                                let sub_result = tokio::task::spawn(async move {
                                    sub_pipeline.run_inner().await
                                }).await.unwrap_or(PipelineRunResult {
                                    pipeline_id: sub_id.clone(),
                                    state: PipelineState::Failed("spawn error".into()),
                                    results: Vec::new(),
                                });
                                StepResult {
                                    step_id,
                                    success: sub_result.state == PipelineState::Completed,
                                    output: format!("sub-pipeline {}: {:?}", sub_id, sub_result.state),
                                }
                            } else {
                                StepResult {
                                    step_id, success: false,
                                    output: format!("sub-pipeline not found: {sub_id}"),
                                }
                            }
                        })
                    }
                };
                fut
            }).collect();

            let parallel_results = futures_util::future::join_all(futs).await;
            for result in parallel_results {
                if !result.success {
                    return PipelineRunResult {
                        pipeline_id: self.id.clone(),
                        state: PipelineState::Failed(result.output.clone()),
                        results,
                    };
                }
                done.insert(result.step_id.clone());
                results.push(result);
            }
        }

        PipelineRunResult {
            pipeline_id: self.id.clone(),
            state: PipelineState::Completed,
            results,
        }
    })
    }
}
