# Abacus 工具特征矩阵与类型定义

**探索日期**: 2026-05-27  
**覆盖范围**: filengine (15 工具), orchestrate (2 工具), kb (3 工具), db (6 工具), code_exec (1 工具)  
**总计**: 27 个内置工具

---

## 第一部分：工具特征矩阵

### 1. Filengine 工具集 (15 工具)

| # | 工具名 | 粒度 | Token成本 | 延迟 | 输出类型 | 副作用 | 确认需求 | 风险等级 | 内置限制 |
|---|-------|------|----------|------|--------|--------|--------|---------|---------|
| 1 | filengine_fs_read | file/lines | 64 | 10ms | JSON (文件内容) | 无 | 否 | low | 支持单 path 或 paths 数组 (≤20) |
| 2 | filengine_fs_write | file | 64 | 10ms | JSON (message) | 是 | 是 | medium | 创建或覆盖；带 undo logger 支持 |
| 3 | filengine_fs_edit | file/lines | 96 | 15ms | JSON (message) | 是 | 否 | medium | old_string 必须精确唯一匹配 |
| 4 | filengine_fs_move | file/dir | 48 | 10ms | JSON (message) | 是 | 是 | medium | 防符号链接越界；destination 自动创建 parent |
| 5 | filengine_fs_info | file/dir | 32 | 5ms | JSON (stat) | 无 | 否 | low | 返回大小/权限/修改时间/是否目录 |
| 6 | filengine_fs_search | dir/file | 48 | 50ms | JSON (paths) | 无 | 否 | low | Glob 模式匹配；遍历深度 ≤5 |
| 7 | filengine_fs_ls | dir | 32 | 5ms | JSON (entries) | 无 | 否 | low | 单层列表或递归树 (max 5 层) |
| 8 | filengine_fs_mkdir | dir | 32 | 5ms | JSON (message) | 是 | 是 | low | 递归创建所有父目录；路径已存在时幂等 |
| 9 | filengine_fs_grep | file/lines | 96 | 500ms | JSON (matches) | 无 | 否 | low | 正则搜索；max_results ≤100；context ∈[0,5] |
| 10 | filengine_fs_cwd | 无 | - | <1ms | JSON (cwd) | 无 | 否 | low | 获取 session 当前工作目录 |
| 11 | filengine_fs_status | 无 | - | <1ms | JSON (summary) | 无 | 否 | low | 返回 session 文件活动摘要 (recent/modified) |
| 12 | web_fetch | page | 128 | 1s | JSON (content) | 无 | 否 | low | HTTP GET；timeout ≤60s；自动重试 |
| 13 | web_search | page+ | 128 | 2s | JSON (results) | 无 | 否 | low | 搜索引擎；count ≤20；timeout ≤60s |
| 14 | filengine_bash_exec | 命令行 | 96 | 1s | JSON (stdout/stderr) | 是 | 是 | medium | 白名单限制；timeout ∈[0,120]s；default 30s |
| 15 | filengine_fs_read_multiple | files | - | - | - | 无 | 否 | - | 已合并至 fs_read (paths 数组)；executor 向后兼容 |

**说明**: 
- filengine 前缀命名约定：保持下划线形式避免 sanitize 链路
- web_* 工具无 filengine_ 前缀（通用 web 工具）
- 路径越界检查：NativeFilengine::resolve() 拒绝 `..` + 边界检验
- Session 状态：FilengineSession 持有 cwd/recent_files/modified/open_context/undo_logger

---

### 2. Orchestrate 工具 (2 工具)

| # | 工具名 | 粒度 | Token成本 | 延迟 | 输出类型 | 副作用 | 确认需求 | 风险等级 | 内置限制 |
|---|-------|------|----------|------|--------|--------|--------|---------|---------|
| 1 | orchestrate_assess | 任务描述 | 32 | 5ms | JSON (level/agents) | 无 | 否 | low | 规则评估置信度阈值 0.6；四维评分 (file_scope/op_type/certainty/cost) |
| 2 | orchestrate_upgrade | 级别信号 | 16 | 1ms | JSON (action/toLevel) | 否 | 否 | low | Level 3 时 escalate_to_human；支持 failedStep 上报 |

**说明**:
- assess: 启发式规则优先 (<5ms)，低置信度时标注 "rule_low_confidence"
- upgrade: from_level/to_level ∈ [1,3]；action ∈ ["upgrade", "escalate_to_human"]
- 敏感检测: password/token/secret/api_key/.env 关键词
- 破坏性检测: delete/remove/drop/truncate/覆盖/rm -rf/force 关键词

---

### 3. Knowledge Base 工具 (3 工具)

| # | 工具名 | 粒度 | Token成本 | 延迟 | 输出类型 | 副作用 | 确认需求 | 风险等级 | 内置限制 |
|---|-------|------|----------|------|--------|--------|--------|---------|---------|
| 1 | kb_ingest | file | 48 | 200ms | JSON (status) | 是 | 否 | low | chunk + FTS5 索引；force 覆盖 hash 校验 |
| 2 | kb_query | query | 64 | 50ms | JSON (results+degradation) | 无 | 否 | low | BM25 + trigram；topK ≤20；degradation ∈ [Normal/WeakSignal/ZeroHit] |
| 3 | kb_search | query | 96 | 100ms | JSON (merged+degradation) | 无 | 否 | low | 多源合并 (KB+Memory Palace)；limit ≤20；score 降序 |

**说明**:
- 降级信号 (DegradationLevel): Normal (score ≥0.2) / WeakSignal (0.2>score) / ZeroHit (无结果)
- Palace 结果加权: 0.5 * (sm2_ease/2.5).min(1.0)
- 依赖: KnowledgeStore (后端) + DualPalaceMemory (内存层)

---

### 4. Database 工具 (6 工具)

| # | 工具名 | 粒度 | Token成本 | 延迟 | 输出类型 | 副作用 | 确认需求 | 风险等级 | 内置限制 |
|---|-------|------|----------|------|--------|--------|--------|---------|---------|
| 1 | db_info | DB | 16 | 5ms | JSON (path/size/tableCount) | 无 | 否 | low | 获取数据库元信息；默认 ~/.abacus/memory.db |
| 2 | db_list_tables | DB | 32 | 5ms | JSON (name/sql) | 无 | 否 | low | 排除 sqlite_* 系统表；按 name 排序 |
| 3 | db_table_schema | table | 32 | 5ms | JSON (columns[]) | 无 | 否 | low | PRAGMA table_info；cid/type/notNull/defaultValue/primaryKey |
| 4 | db_query | SQL | 64 | 50ms | JSON (rows[]) | 无 | 否 | medium | 参数化 SQL；禁止 ATTACH/DETACH；按列名返回对象 |
| 5 | db_mutate | SQL | 48 | 10ms | JSON (message/rowsAffected) | 是 | 是 | medium | op ∈ [create/update/delete]；conditions 必填 (update/delete) |
| 6 | db_read_records | table | 48 | 10ms | JSON (rows[]) | 无 | 否 | low | 条件查询 + 分页；limit/offset 可选；AND 连接条件 |

**说明**:
- db_mutate: 合并 create/update/delete → 1 个 schema (减少 ~150 tokens)
- 旧 tool_id (db_create/update/delete) 在 executor 仍可路由（向后兼容）；schema 层不注册
- 数据库连接: WAL mode + synchronous=NORMAL + busy_timeout=5000ms
- Expand tilde: ~/foo → $HOME/foo；SQL 标识符转义 `"` → `""`

---

### 5. Code Executor 工具 (1 工具)

| # | 工具名 | 粒度 | Token成本 | 延迟 | 输出类型 | 副作用 | 确认需求 | 风险等级 | 内置限制 |
|---|-------|------|----------|------|--------|--------|--------|---------|---------|
| 1 | code_execute | 脚本 | 0 | 5ms | JSON (result) | 可能 | 否 | low | Rhai 脚本引擎；max_size_mb=1；input 变量可选 |

**说明**:
- Rhai: 轻量级脚本语言；feature "serde" 支持 JSON 序列化
- 输入: script (必填) + input (可选 JSON 变量)
- 无 token 消耗 (本地计算)；idempotent=false (副作用)

---

## 第二部分：类型定义

### ToolSecurity 结构体

```rust
pub struct ToolSecurity {
    pub allowed_paths: Option<Vec<String>>,  // 白名单路径 (None=无限制)
    pub max_size_mb: Option<u32>,             // 最大处理大小 (None=无限制)
    pub confirm_required: bool,               // 是否需要用户确认
    pub needs_sandbox: bool,                  // 是否需要沙箱隔离
}
```

**使用示例**:
- `fs_read`: `allowed_paths=None, max_size_mb=None, confirm_required=false, needs_sandbox=false` (开放)
- `fs_write`: `confirm_required=true` (需确认)
- `bash_exec`: `confirm_required=true, needs_sandbox=false` (需确认但无沙箱)
- `db_mutate`: `confirm_required=true` (需确认 update/delete)

---

### ToolCost 结构体

```rust
pub struct ToolCost {
    pub tokens: u32,      // 该工具调用的平均 token 消耗
    pub latency: String,  // 执行延迟 (如 "10ms", "1s")
    pub risk: String,     // 风险等级: "low" / "medium" / "high"
}
```

**风险等级语义**:
- `low`: 只读操作、查询、信息获取 (fs_read, fs_info, web_fetch, db_query)
- `medium`: 写操作、命令执行、数据变更 (fs_write, fs_edit, bash_exec, db_mutate)
- `high`: 高危操作 (预留，当前未使用)

---

### ToolEffectiveness 结构体

```rust
pub struct ToolEffectiveness {
    pub tool_id: ToolId,
    pub composite_score: f64,           // 综合有效性评分 ∈ [0.0, 1.0]
    pub tier: VisibilityTier,           // 可见性分级: S/A/B/C/D
    pub cooldown_remaining: u32,        // 冷却剩余轮次
    pub blocked_by_env: bool,           // 是否被环境限制
    pub insufficient_data: bool,        // 是否数据不足
}

pub enum VisibilityTier {
    S,  // 最高可见 (核心工具)
    A,  // 高可见 (默认新工具)
    B,  // 中可见
    C,  // 低可见
    D,  // 最低可见
}
```

**评分逻辑**:
- 默认初始化: `composite_score=0.6, tier=A, cooldown_remaining=0, insufficient_data=true`
- 工具用发历史: 成功率 ↑ 升级 tier；失败率 ↑ 降级 tier

---

### ToolSchema 结构体

```rust
pub struct ToolSchema {
    pub name: String,                              // 工具名 (如 "fs_read")
    pub description: String,                       // 描述文本
    pub parameters: serde_json::Value,            // JSON Schema 参数定义
    pub returns: Option<serde_json::Value>,       // 返回值 schema (可选)
    pub security: Option<ToolSecurity>,           // 安全策略
    pub cost: Option<ToolCost>,                   // 成本估计
    pub examples: Vec<ToolExample>,               // 调用示例 (Phase β-C)
    pub applicable_task_kinds: Option<Vec<String>>, // 任务类型白名单 (Phase β-D)
    pub idempotent: bool,                         // 是否幂等 (Phase β-G)
}

pub struct ToolExample {
    pub description: String,           // 示例描述
    pub params: serde_json::Value,    // 参数 JSON
    pub expected_output: Option<serde_json::Value>, // 期望输出
}
```

**idempotent 标记**:
- `true`: fs_read, fs_info, fs_search, fs_ls, fs_grep, db_read_records, db_info, db_list_tables, db_table_schema
- `false`: fs_write, fs_edit, fs_move, fs_mkdir, bash_exec, kb_ingest, db_mutate, code_execute

---

## 第三部分：工具目录与 Prompt 注入

### Tool Catalog 生成

**位置**: `crate::llm::tool_catalog::generate_catalog()`

**输出格式**:
```
[Available Tools — call any by name]
builtin: filengine_fs_read, filengine_fs_write, ..., orchestrate_assess, kb_query, ...
mcp(server1): tool_a, tool_b
plugin(sqlite): db_query
skill(workflow1): step_x, step_y
```

**设计目标**:
- 按 provider 分组 (BuiltIn/Mcp/Plugin/Skill)
- 紧凑格式: ~200-300 tokens/100 tools (vs 全量 schema 5000-10000 tokens)
- LLM 通过目录知道全集；完整 schema 通过 LlmRequest.tools 另行发送

**注入层**:
- **Layer 180** (Prompt Assembly): tool_catalog 注入 system prompt (见下文)

---

### Prompt Assembly 工具注入层级

**位置**: `crate::core::prompt_assembly::PromptAssembly`

**9 层优先级架构**:
```
Layer 255 (Kernel):           核心行为规则
Layer 230 (abacusbr core):    用户行为规范（稳定，不随任务变化）
Layer 200 (Strategy):         策略 + 反模式告警
Layer 190 (Constraints):      约束/限制说明
Layer 188 (Completion):       任务完成摘要规则（稳定）
Layer 185 (Subscenes):        任务相关子场景（动态，随 TaskKind 变化）
Layer 180 (Knowledge):        知识库 + injector 动态段 ← TOOL_CATALOG 在此处或附近
Layer 160 (Deduction):        推演告警
Layer 155 (Preflight):        静默自审报告
Layer 90  (Skills):           活跃 skill prompts (Phase 1 已删除)
Layer 20  (Interaction):      交互地图状态 (Phase 1 已删除)
```

**Tool 相关注入**:
- **工具列表** (tool_catalog): Layer 180 (Knowledge 层)
- **完整 Schema**: 通过 LlmRequest.tools 字段传递，不在 system prompt 中
- **示例** (examples): 嵌入 tool schema 末尾 (ToolExample 结构体)

**设计原则**:
- 稳定前缀 (Layer 230-200): 跨 turn 字节稳定，支撑 KV cache
- 动态块 (Layer 185-20): 任务/推演/交互状态变化时更新

---

## 第四部分：Skill 命令与生态

### Skill 定义示例位置

**当前状态**: `~/.abacus/` 下无 .yaml skill 定义文件

**Skill 定义结构** (engine.rs):
```rust
pub struct SkillDef {
    pub id: SkillId,
    pub version: String,
    pub triggers: SkillTriggers,      // keywords/regex/domain
    pub workflow: Vec<SkillStep>,     // 顺序执行步骤
    pub prompt: String,               // Skill 提示词
    pub knowledge_refs: Vec<String>,  // 关联知识库
}

pub struct SkillTriggers {
    pub keywords: Vec<String>,  // 关键词匹配
    pub regex: Vec<String>,     // 正则匹配
    pub domain: Vec<String>,    // 领域分类
}

pub struct SkillStep {
    pub id: String,
    pub description: String,
    pub tool: String,           // 工具名
    pub params: serde_json::Value,
    pub depends_on: Option<Vec<String>>,
    pub condition: Option<String>,
    pub fallback: Option<String>,
}
```

**Skill 注册路径**:
- 被 `crate::skill::mod.rs::SkillEngine::load()` 调用
- 每个 step 作为工具注册到 ToolRegistry
- 生命周期: 随 SkillEngine 创建/销毁

---

### abacus-cli Skill 命令

**位置**: `crate::skill::mod.rs` / 待查 `abacus-cli/src/commands/skill.rs`

**预期功能** (根据系统提示推断):
- `abacus skill list`: 列出所有已加载 skill
- `abacus skill load <file.yaml>`: 加载外部 skill 定义
- `abacus skill run <skill_id>`: 执行 skill 工作流
- `abacus skill inspect <skill_id>`: 查看 skill 详情

---

## 第五部分：Security & Cost Policies

### 工具安全策略配置

**位置**: `crate::tool::subsystem_policy` (推断)

**典型配置** (来自 schema):
```
fs_read:       allowed_paths=None, max_size_mb=None
fs_write:      confirm_required=true, max_size_mb=None
bash_exec:     confirm_required=true, needs_sandbox=false
web_fetch:     max_size_mb=None, timeout ≤ 60s
db_mutate:     confirm_required=true
```

**确认流程** (MCIP):
- `confirm_required=true` 时，ToolOutput.failure_kind = "Authorization"
- 等待 session.request_permission() 用户授权
- 授权后重新调用工具

---

### 成本预算

**预期成本** (基于 ToolCost):

| 工具类型 | 典型 tokens | 示例 |
|---------|-----------|-----|
| 查询 | 16-64 | fs_info (32), fs_search (48), kb_query (64) |
| 读取 | 64-96 | fs_read (64), fs_grep (96) |
| 网络 | 128 | web_fetch (128), web_search (128) |
| 写入 | 48-96 | fs_write (64), bash_exec (96) |
| 数据库 | 16-64 | db_info (16), db_query (64) |
| 脚本 | 0 | code_execute (0 — 本地计算) |

**总 session 预算**: 假设 ~8000 token context 窗口，工具+结果占 ~1000-2000 tokens

---

## 第六部分：幂等性与去重

### Idempotent 工具

**幂等** (多次调用同参数 → 同结果，无副作用):
```
fs_read, fs_info, fs_search, fs_ls, fs_grep, web_fetch, web_search
db_read_records, db_info, db_list_tables, db_table_schema
kb_query, kb_search
orchestrate_assess
```

**非幂等** (有副作用或非确定性):
```
fs_write, fs_edit, fs_move, fs_mkdir        (修改文件系统)
bash_exec                                   (命令执行，可能有外部效果)
kb_ingest                                   (修改知识库)
db_mutate                                   (修改数据库)
code_execute                                (脚本执行，可能有副作用)
```

**管道优化** (Phase β-G):
- 多个 idempotent 工具可并行执行（加速 latency）
- 非 idempotent 工具强制串行

---

## 第七部分：工具执行上下文

### ExecutionContext 设计

**位置**: `crate::tool::mod.rs::ExecutionContext`

```rust
pub struct ExecutionContext {
    pub session_id: String,
    pub filengine: Arc<RwLock<FilengineSession>>,
    pub turn_number: u32,
    pub bash_default_timeout: u64,
    pub bash_max_timeout: u64,
    pub tool_default_timeout: u64,
}

impl ExecutionContext {
    pub fn noop(session_id: impl Into<String>) -> Self { ... }  // 测试用
}
```

**生命周期**:
- 创建: TurnPipeline::execute_loop() 进入工具分发时
- 消费: ToolRegistry::execute() → ToolExecutor::execute()
- 销毁: 工具返回后随 `&` 引用一同 drop

**per-session 状态**:
- `filengine`: FilengineSession (cwd/recent_files/modified/undo_logger)
- `turn_number`: 当前轮次 (undo logger 记录)
- `bash_*_timeout`: bash 超时策略

---

## 第八部分：完整工具矩阵 CSV 导出

```csv
tool_name,granularity,token_cost,latency,output_type,has_side_effect,confirm_required,risk_level,provider,idempotent,max_size_mb
filengine_fs_read,file,64,10ms,JSON,false,false,low,builtin,true,null
filengine_fs_write,file,64,10ms,JSON,true,true,medium,builtin,false,null
filengine_fs_edit,file,96,15ms,JSON,true,false,medium,builtin,false,null
filengine_fs_move,file,48,10ms,JSON,true,true,medium,builtin,false,null
filengine_fs_info,file,32,5ms,JSON,false,false,low,builtin,true,null
filengine_fs_search,dir,48,50ms,JSON,false,false,low,builtin,true,null
filengine_fs_ls,dir,32,5ms,JSON,false,false,low,builtin,true,null
filengine_fs_mkdir,dir,32,5ms,JSON,true,true,low,builtin,false,null
filengine_fs_grep,file,96,500ms,JSON,false,false,low,builtin,true,null
filengine_fs_cwd,none,0,1ms,JSON,false,false,low,builtin,true,null
filengine_fs_status,none,0,1ms,JSON,false,false,low,builtin,true,null
web_fetch,page,128,1s,JSON,false,false,low,builtin,true,null
web_search,page+,128,2s,JSON,false,false,low,builtin,true,null
filengine_bash_exec,command,96,1s,JSON,true,true,medium,builtin,false,null
kb_ingest,file,48,200ms,JSON,true,false,low,builtin,false,10
kb_query,query,64,50ms,JSON,false,false,low,builtin,true,null
kb_search,query,96,100ms,JSON,false,false,low,builtin,true,null
db_info,database,16,5ms,JSON,false,false,low,builtin,true,null
db_list_tables,database,32,5ms,JSON,false,false,low,builtin,true,null
db_table_schema,table,32,5ms,JSON,false,false,low,builtin,true,null
db_query,sql,64,50ms,JSON,false,false,medium,builtin,true,null
db_mutate,sql,48,10ms,JSON,true,true,medium,builtin,false,null
db_read_records,table,48,10ms,JSON,false,false,low,builtin,true,null
orchestrate_assess,task,32,5ms,JSON,false,false,low,builtin,true,null
orchestrate_upgrade,signal,16,1ms,JSON,false,false,low,builtin,false,null
code_execute,script,0,5ms,JSON,true,false,low,builtin,false,1
```

---

## 附录：路径安全与 NativeFilengine::resolve()

### 路径解析策略 (F-BUG-2 修复)

**三类输入处理**:
1. **路径已存在** → canonicalize 全路径，解析 symlink + 大小写规范化
2. **路径不存在** → 向上找最近存在的祖先，对祖先 canonicalize，再字面拼回剩余 segments
3. **包含 `..`** → 直接拒绝

**安全检查**:
```
1. 拒绝 `..` (防层级穿越)
2. allowed_roots() 验证 HOME (绝对路径 + ≥3 components + ≠ "/")
3. 非根目录 absolute path 约束
4. 字面拼接安全 (无相对路径)
```

**调用方** (10 个):
- fs.read, fs.write, fs.edit, fs.move, fs.info, fs.search, fs.ls, fs.tree, fs.mkdir, fs.grep, bash.exec(workdir)

---

## 附录：工具效果评估 (ToolEffectiveness 计分)

### 综合评分因子

**composite_score** 计算**:
- 成功率 (success_rate): 0.3 权重
- 平均延迟 vs 预期延迟: 0.3 权重
- 用户反馈 (feedback): 0.2 权重
- 冷却状态: 0.2 权重 (cooldown 中为 0)

**Tier 升降规则**:
- success_rate ≥ 0.9 ∧ avg_latency ≤ predicted_latency → 升级
- success_rate < 0.5 → 降级
- 默认新工具: A 级

**冷却机制**:
- 失败 3 次 → cooldown_remaining = N (N 根据失败类型)
- 每 turn 递减 cooldown_remaining
- cooldown_remaining > 0 时工具不可见 (failure_kind="Cooldown")

---

**文档版本**: v1.0 (2026-05-27)  
**作者**: Code Explorer  
**状态**: 完整探索完成，可用于架构决策和文档维护
