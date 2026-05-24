# Abacus 三模式架构总览

## 三种模式的定义

```
Mode 1 — 单 Agent 多步骤（Single Agent Multi-Step）
  本质：单人全能
  架构：CoreLoop.process_turn() → LLM → ToolCall → LLM → ... → 输出
  代码：abacus-engine/src/core/mod.rs（现有，完整实现）
  适用：自由对话、通用问题、单领域任务

Mode 2 — 主 Agent 指派 SubAgent（Leader Delegation）
  本质：项目经理带执行者
  架构：Leader 分解任务 → SubAgent[N] 分头执行 → Leader 汇总
  代码：abacus-orchestrator/src/team/ + subagent/（现有，骨架实现）
  适用：多步骤执行、任务分解、结构化产出

Mode 3 — AgentMeeting 专家斗法（Specialist Showdown）
  本质：主持人组织专家会诊
  架构：Host 路由需求 → Specialist[N] 并行推理 → Host 聚合结论
  代码：abacus-orchestrator/src/meeting/ + specialist/（新增，设计完成）
  适用：方案评审、多领域决策、架构审查
```

---

## 横向对比：13 个维度

| 维度 | Mode 1 (单Agent) | Mode 2 (指派) | Mode 3 (会诊) |
|------|-----------------|---------------|---------------|
| Agent角色数 | 1（单角色） | Leader + 1~N Sub（PM+执行者） | Host + 0~8 Specialist（主持人+领域专家） |
| Agent定义 | CoreLoop进程，无显式定义 | SubAgentBoundary(steps/tokens/tools) | Specialty(domain/guide/antipattern) |
| 创建方式 | 启动即存在 | Leader.dispatcher().dispatch(boundary) | MeetingRouter.invite().register(specialty) |
| Agent间关系 | 无 | Leader→SubAgent（单向委托） | Host↔Specialist（双向对话） |
| 上下文模型 | 单线历史，无隔离 | 继承父上下文，ContextScope隔离 | 三层：共享+角色+动态，每S独立prompt链 |
| 决策方式 | LLM自主决策 | Leader分解 + SubAgent执行 | MeetingRouter路由 + Specialist推理 + Host聚合 |
| 对话方向 | 用户↔Agent | 用户→Leader→SubAgent→→Leader→用户 | 用户↔Host↔Specialist[N]（群聊，多轮追问） |
| 并发控制 | 串行 | 串行（步骤依赖） | 并行推理（信号量4） |
| 生命周期 | Session：创建→对话→结束 | TeamSession：分解→执行→汇总 | MeetingSession：邀请→讨论→纪要 |
| 规范级联 | Kernel+Harness+Guide+Expert+Inject | Leader继承全部，SubAgent继承Kernel+Harness | 三层级联：会议Harness+Guide，角色Guide/AntiPattern |
| 记忆写入 | Session结束时一次写入 | Leader Session写 + SubAgent记录写 | 每轮Specialist产出写入 + 结束时双宫殿写入 |
| 记忆内容 | session_summary, tool_adoption, user_preference | team_task_completion, subagent_result | meeting_conclusion, meeting_consensus, meeting_controversy |
| 输出风格 | 连续对话（流式/单条） | 结构化报告（Leader汇总） | 分页Dashboard（主消息区+右侧面板） |
| 用户感知 | "一个AI在跟我聊" | "AI在分派子任务" | "一群专家在讨论" |
| TUI界面 | 聊天面板，无分页 | 任务看板（进度条+状态） | MeetingPane + DashboardPane |
| 架构实现 | 完整实现 | 骨架实现 | 设计完成待实现 |

---

## 共享架构层

三种模式不互斥——共享同一套底层基础设施，差异只在编排层：

```
共享层（不变）                              Mode 1     Mode 2     Mode 3
─────────────────────────────               ──────    ──────    ──────
L0 类型系统 (abacus-types)                    ✅         ✅         ✅
  AgentRole, ToolId, ModelId, KernelError

L1 核心 (abacus-core)                       ✅         ✅         ✅
  ConfigManager, SecretsManager

L2 引擎 (abacus-engine)                     ✅         ✅         ✅
  ├── CoreLoop.process_turn()               ✅         ✅         ✅
  │    所有模式最终都调用 CoreLoop 来执行一轮推理
  │    差别在于调用者的上下文和参数
  │
  ├── PromptAssembly                        ✅         ✅         ✅
  │    所有模式的 prompt 都经过相同优先级装配
  │    Mode 2/3 在此之上增加了层级封装
  │
  ├── MagChain (中间件链)                    ✅         ✅         ✅
  │    ├── CircuitBreaker, RateLimiter
  │    ├── AuditLogger, PIIRedactor
  │    └── ＋ EventLayer                    ─          ─         🆕
  │
  ├── SafetyGuard, SkillEngine              ✅         ✅         ✅
  ├── ToolRegistry                          ✅         ✅         ✅
  └── Memory Palace                         ✅         ✅         ✅
       ├── Behavior Palace                  写入       写入       写入+
       └── Knowledge Palace                 写入       写入       写入+

L3 编排 (abacus-orchestrator)               ──         🏗          🏗
  ├── team/ (TeamSession)                    ─         ✅         ─
  ├── subagent/ (SubAgent)                   ─         ✅         ─
  ├── plan/ (PlanModel)                      ─         ✅         ─
  ├── meeting/ (MeetingSession)              ─         ─         🏗
  ├── specialist/ (Specialist)               ─         ─         🏗
  └── debate/                               ─         ─        可选内嵌

L4 应用 (abacus-cli / abacus-tui)           ✅         🏗          🏗
  ├── CLI 命令                              chat      team        meeting
  └── TUI 界面                              聊面板     看板        MeetingPane+Dashboard
```

**关键观察**：所有模式都调用同一套 `CoreLoop.process_turn()`、`PromptAssembly.assemble()`、`MagChain.before/after()`。Mode 2/3 的区别不在于底层引擎，而在于**编排层如何组织多个 Agent 实例**以及**上下文如何在它们之间流动**。

---

## 模式切换

### 入口切换

```
Mode 1:  abacus chat "帮我写个排序算法"
         → 默认模式，直接启动 CoreLoop

Mode 2:  abacus team --goal "重构项目" --roles coder,reviewer
         → 进入 TeamSession，Leader 分解任务

Mode 3:  abacus meeting --topic "架构审查" --specialists coder,reviewer
         → 进入 MeetingSession，Host 邀请 Specialist
```

### 运行时切换

```
Mode 1 → Mode 2:
  "这个任务比较复杂，让团队来做"
  → CoreLoop 暂停 → 创建 TeamSession
  → Leader 继承当前上下文 → 分解任务 → 分派 SubAgent
  → 完成后恢复 Mode 1

Mode 1 → Mode 3:
  "开启专家会诊模式，评估一下这个方案"
  → CoreLoop 暂停 → 创建 MeetingSession
  → Host 继承需求 → MeetingRouter 匹配 Specialist → 邀请
  → 讨论完成后恢复 Mode 1

Mode 2 → Mode 3:
  "这个子任务需要多领域专家评审"
  → 子任务从 Mode 2 转入 Mode 3
  → MeetingSession 嵌入 TeamSession 的子步骤
  → 评审结论返回 Leader，继续 Mode 2 流程

所有切换通过相同机制实现：
  Session.state.mode = Mode::Meeting
  → PromptAssembler 检测模式 → 追加对应的规范层
  → Orchestrator 读取模式 → 调度对应的编排器
```

---

## 编排层架构映射

```
Orchestrator 模块结构

team/              ← Mode 2 (指派模式)
├── mod.rs: TeamSession, TeamManager, AgentRole
├── session.rs: SharedContext, PrivateContext
└── protocol.rs: TeamMessage (task_assign/task_update/...)

subagent/          ← Mode 2 (执行容器)
├── mod.rs: SubAgentInstance, SubAgentDispatcher
└── boundary.rs: SubAgentBoundary (steps/tokens/tools)

plan/              ← Mode 2 (计划编排)
├── mod.rs: PlanModel, PlanExecutor, Planner
└── step.rs: StepKind (ToolCall/LlmReason/SubAgentDelegate)

meeting/           ← Mode 3 (会诊模式) — 新增
├── session.rs: MeetingSession, MeetingStatus
├── router.rs: MeetingRouter, RoutingDecision
├── context.rs: ContextPool (shared + private)
└── minutes.rs: MeetingMinutesGenerator

specialist/        ← Mode 3 (专家角色) — 新增
├── mod.rs: SpecialistInstance, SpecialistStatus
├── specialty.rs: Specialty, EngagementLimit
├── opinion.rs: SpecialistOpinion, OpinonAggregator
└── hook.rs: MeetingStepRunner (Auto Hook 适配)

debate/            ← Mode 3 内嵌 (辩论协议)
├── protocol.rs: proposal→challenge→defense→verdict
└── arena.rs: DebateArena (参与者管理 + 轮次控制)

quality/           ← 全模式共享 (质量门禁)
├── obc.rs: Output-Boundary Check
├── efp.rs: Expected Failure Prediction
└── anti_hallucination.rs
```

---

## 语境卡片

每种模式 = 1 种语境卡片，注入到 PromptAssembly 的 Guide 层：

```
Mode 1 Guide Prompt:
  "你是全能助手。无需切换角色。
   你有所有工具和所有知识域的访问权限。
   直接回答用户问题，按需调用工具。"

Mode 2 Guide Prompt:
  "你是项目负责人（Leader）。
   你的任务：分解 → 分派 → 监控 → 汇总。
   你有 SubAgent 可以委派子任务。
   每步确保质量，处理升级。"

Mode 3 Guide Prompt:
  "你是会议主持人（Host）。
   你的角色：路由需求 → 协调专家 → 汇总结论。
   你有 0~8 个 Specialist 可以邀请。
   你只展示状态和结论，推理过程由 Specialist 处理。
   检测到矛盾时介入裁决。"
```

---

## 总结

```
架构分层决定复用，编排层决定差异

Mode 1 和 Mode 2/3 的边界：
  CoreLoop, MagChain, PromptAssembly, SafetyGuard
  → 三种模式共享，不需要对引擎层做任何修改

Mode 2 和 Mode 3 的边界：
  SubAgent = 执行容器（"去干"）
  Specialist = 领域专家（"来想"）
  TeamSession = 任务分解 + 进度追踪（"做完"）
  MeetingSession = 专家讨论 + 结论聚合（"议定"）
  PlanModel = 步骤驱动的执行流
  MeetingRouter = 语义驱动的路由流

四种可能的嵌套关系：
  M1 → M2：对话中遇到复杂任务 → 开启团队模式
  M1 → M3：对话中需要专家评审 → 开启会诊模式
  M2 → M3：子任务需要多领域决策 → 内嵌会诊模式
  M3 → M2：专家结论需要落地执行 → 指派 SubAgent 执行
```

这个三模式设计是正交的——三个模式共享 70% 的底层引擎代码，差异仅在编排层 `<abacus-orchestrator/src/{team,meeting}/` 的模块组织方式。模式切换通过 `Session.state.mode` 的语境卡片实现，核心是 `MeetingPromptAssembler` 在共享前缀上的多路复用。
