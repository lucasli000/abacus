//! Interaction Map — DAG-based LLM position awareness
//!
//! Provides a runtime checkpoint DAG that tracks the LLM's position through
//! a conversation: where it has been (checkpoints), how it got there (edges),
//! and what decisions were made along the way.
//!
//! ## Components
//!
//! - [`InteractionMap`]: The core DAG — checkpoints + directed edges
//! - [`MapAnalyzer`]: Heuristic auto-checkpoint detection from turn data
//! - [`Checkpoint`]: A named position in the conversation (topic shift,
//!   decision, milestone, user correction, tool chain)
//! - [`CheckpointType`]: Seven variants distinguishing checkpoint semantics
//! - [`Edge`]: Directed relationships between checkpoints
//!
//! ## LLM-Facing Tools
//!
//! Four tools are registered at the CoreLoop level for LLM interaction:
//! - `interaction.status` (~20 tok): current position query
//! - `interaction.path`: full path + branches
//! - `interaction.recall`: recall a checkpoint by ID
//! - `interaction.mark`: manually create a checkpoint

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Semantic type of a checkpoint.
///
/// Determines how the checkpoint is displayed, filtered, and reasoned about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CheckpointType {
    /// User changed the conversation topic (Jaccard < 0.3 heuristic)
    TopicShift,
    /// A choice was made between alternatives
    Decision,
    /// A significant progress point reached
    Milestone,
    /// User explicitly corrected the assistant
    UserCorrection,
    /// Three or more consecutive tool calls formed a chain
    ToolChain,
    /// LLM manually marked via `interaction.mark`
    ManualMark,
    /// A sub-goal was reached
    Subgoal,
}

impl CheckpointType {
    /// Return the string representation (snake_case) for serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckpointType::TopicShift => "topic_shift",
            CheckpointType::Decision => "decision",
            CheckpointType::Milestone => "milestone",
            CheckpointType::UserCorrection => "user_correction",
            CheckpointType::ToolChain => "tool_chain",
            CheckpointType::ManualMark => "manual_mark",
            CheckpointType::Subgoal => "subgoal",
        }
    }
}

/// A single decision recorded at a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    /// What the decision was about
    pub description: String,
    /// Which option was chosen
    pub chosen: String,
    /// Why this option was chosen over alternatives
    pub rationale: String,
    /// Options that were considered but not chosen
    pub alternatives: Vec<String>,
}

/// A branch from a checkpoint — a sub-path that may be active, completed, or abandoned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointBranch {
    /// Human-readable label for this branch
    pub label: String,
    /// Target checkpoint ID
    pub checkpoint_id: u32,
    /// Whether this branch is active, completed, or abandoned
    pub status: BranchStatus,
}

/// Lifecycle status of a branch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BranchStatus {
    /// Branch is currently being explored
    Active,
    /// Branch was abandoned before completion
    Abandoned,
    /// Branch was completed
    Completed,
}

/// Record of a single tool call within a checkpoint's tool chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Tool ID that was called
    pub tool_id: String,
    /// Parameters passed to the tool
    pub params: Value,
    /// Result summary ("ok" or "error")
    pub result: String,
}

/// A named checkpoint in the interaction DAG.
///
/// Each checkpoint captures what happened, why, and what decisions/tools were involved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Auto-assigned unique ID
    pub id: u32,
    /// Human-readable label
    pub label: String,
    /// Semantic type
    pub type_: CheckpointType,
    /// Conversation turn number
    pub turn: u32,
    /// Approximate token offset in the conversation
    pub token_offset: u32,
    /// Short summary of what happened
    pub summary: String,
    /// Original user intent/input that led here
    pub intent: String,
    /// Decisions made at this checkpoint
    pub decisions: Vec<Decision>,
    /// Consecutive tool calls recorded here
    pub tool_chain: Vec<ToolCallRecord>,
    /// Branches originating from this checkpoint
    pub branches: Vec<CheckpointBranch>,
}

impl Checkpoint {
    /// Create a new checkpoint with the given properties.
    /// `id` is set to 0 and assigned by [`InteractionMap::add_checkpoint`].
    pub fn new(
        label: String,
        type_: CheckpointType,
        turn: u32,
        token_offset: u32,
        summary: String,
        intent: String,
        decisions: Vec<Decision>,
        tool_chain: Vec<ToolCallRecord>,
        branches: Vec<CheckpointBranch>,
    ) -> Self {
        Self { id: 0, label, type_, turn, token_offset, summary, intent, decisions, tool_chain, branches }
    }
}

/// Directed relationship between two checkpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EdgeKind {
    /// `from` happened before `to`
    Precedes,
    /// `from` caused `to`
    Causes,
    /// `to` depends on `from`
    DependsOn,
    /// `from` and `to` are alternative paths
    AlternativeTo,
    /// `to` corrects `from`
    Corrects,
}

/// An edge in the checkpoint DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    /// Source checkpoint ID
    pub from: u32,
    /// Target checkpoint ID
    pub to: u32,
    /// Relationship kind
    pub kind: EdgeKind,
}

/// Session-level DAG tracking LLM position awareness.
///
/// Maintains a list of checkpoints and directed edges between them.
/// Supports querying current position, full path, and per-checkpoint recall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionMap {
    /// All checkpoints in insertion order
    pub checkpoints: Vec<Checkpoint>,
    /// Directed edges between checkpoints
    pub edges: Vec<Edge>,
    /// Auto-incrementing ID counter
    pub next_id: u32,
    /// Optional session title
    pub session_title: String,
}

impl Default for InteractionMap {
    fn default() -> Self { Self::new() }
}

impl InteractionMap {
    /// Create an empty interaction map.
    pub fn new() -> Self {
        Self {
            checkpoints: Vec::new(),
            edges: Vec::new(),
            next_id: 1,
            session_title: String::new(),
        }
    }

    /// Add a checkpoint and assign it a unique ID. Returns the assigned ID.
    pub fn add_checkpoint(&mut self, mut cp: Checkpoint) -> u32 {
        let id = self.next_id;
        cp.id = id;
        self.next_id += 1;
        self.checkpoints.push(cp);
        id
    }

    /// Add a directed edge between two checkpoints.
    pub fn add_edge(&mut self, edge: Edge) {
        self.edges.push(edge);
    }

    /// Safe add: validate no cycle before inserting.
    pub fn try_add_edge(&mut self, edge: Edge) -> Result<(), String> {
        if self.would_create_cycle(edge.from, edge.to) {
            return Err(format!("edge {}→{} would create a cycle", edge.from, edge.to));
        }
        self.edges.push(edge);
        Ok(())
    }

    /// DFS cycle detection: true if adding `from→to` creates a cycle.
    pub fn would_create_cycle(&self, from: u32, to: u32) -> bool {
        if from == to {
            return true;
        }
        let mut adj: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
        for e in &self.edges {
            adj.entry(e.from).or_default().push(e.to);
        }
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![to];
        while let Some(node) = stack.pop() {
            if node == from {
                return true;
            }
            if !visited.insert(node) {
                continue;
            }
            if let Some(neighbors) = adj.get(&node) {
                stack.extend(neighbors);
            }
        }
        false
    }

    /// Get the most recent checkpoint, if any.
    pub fn current(&self) -> Option<&Checkpoint> {
        self.checkpoints.last()
    }

    /// Find the most recent checkpoint at the given turn.
    pub fn last_turn_checkpoint(&self, turn: u32) -> Option<&Checkpoint> {
        self.checkpoints.iter().rev().find(|c| c.turn == turn)
    }

    /// Find a checkpoint by its ID.
    pub fn checkpoint_by_id(&self, id: u32) -> Option<&Checkpoint> {
        self.checkpoints.iter().find(|c| c.id == id)
    }

    /// Return a compact one-line status string (~20 tokens).
    pub fn status_block(&self) -> String {
        let total = self.checkpoints.len();
        let current = self.current();
        let (idx, label, turn) = match current {
            Some(c) => (c.id, c.label.as_str(), c.turn),
            None => (0, "root", 0),
        };
        format!("[Map] Checkpoint #{idx}/{total} | turn: {turn} | current: {label}")
    }

    /// Return JSON describing the full interaction path.
    pub fn path_info(&self) -> Value {
        let completed: Vec<String> = self.checkpoints.iter()
            .map(|c| c.label.clone()).collect();
        serde_json::json!({
            "completed": completed,
            "total": self.checkpoints.len(),
            "branches_active": self.checkpoints.iter()
                .filter(|c| c.branches.iter().any(|b| b.status == BranchStatus::Active)).count(),
        })
    }

    /// Recall a specific checkpoint by ID. Returns JSON with full details.
    pub fn recall(&self, id: u32) -> Option<Value> {
        self.checkpoint_by_id(id).map(|cp| serde_json::json!({
            "checkpoint": cp.id,
            "label": cp.label,
            "type": cp.type_.as_str(),
            "summary": cp.summary,
            "turn": cp.turn,
            "decisions": cp.decisions,
            "tool_chain_count": cp.tool_chain.len(),
        }))
    }

    /// Return the most recent N distinct tool IDs from the interaction history.
    /// Used by Silent Router for session inertia signal.
    pub fn recent_tools(&self, n: usize) -> Vec<abacus_types::ToolId> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for cp in self.checkpoints.iter().rev() {
            for tc in cp.tool_chain.iter().rev() {
                if seen.insert(tc.tool_id.clone()) {
                    result.push(abacus_types::ToolId(tc.tool_id.clone()));
                    if result.len() >= n { return result; }
                }
            }
        }
        result
    }

    /// Serialize the map for persistence to Behavior Palace.
    pub fn to_persisted(&self) -> Value {
        serde_json::json!({
            "type": "behavior.interaction_map",
            "session_title": self.session_title,
            "checkpoint_count": self.checkpoints.len(),
            "structure": {
                "path": self.checkpoints.iter().map(|c| c.label.clone()).collect::<Vec<_>>(),
                "branches": self.branch_summary(),
            },
            "decisions_summary": self.decisions_summary(),
        })
    }

    fn branch_summary(&self) -> Vec<Value> {
        self.checkpoints.iter()
            .flat_map(|c| c.branches.iter().map(|b| {
                let status = match b.status {
                    BranchStatus::Active => "active",
                    BranchStatus::Abandoned => "abandoned",
                    BranchStatus::Completed => "completed",
                };
                serde_json::json!({
                    "at": c.label,
                    "label": b.label,
                    "status": status,
                })
            }))
            .collect()
    }

    fn decisions_summary(&self) -> Vec<Value> {
        self.checkpoints.iter()
            .flat_map(|c| c.decisions.iter().map(|d| serde_json::json!({
                "at": c.label,
                "chosen": d.chosen,
                "rationale": d.rationale,
            })))
            .collect()
    }
}

/// Heuristic auto-checkpoint detection from turn data.
///
/// Detects four checkpoint types from input/output/tool_chain analysis:
/// - UserCorrection: user corrected the assistant
/// - Decision: LLM made a choice
/// - ToolChain: >= 3 consecutive tool calls
/// - TopicShift: Jaccard similarity < 0.3 between previous intent and current input
pub struct MapAnalyzer;

impl MapAnalyzer {
    /// Analyze a turn and return a checkpoint if a transition is detected.
    ///
    /// Heuristics are checked in order: correction → decision → tool_chain → topic_shift.
    /// Only the first match is returned.
    pub fn analyze_turn(
        input: &str,
        output: &str,
        tool_chain: &[ToolCallRecord],
        turn: u32,
        map: &InteractionMap,
    ) -> Option<Checkpoint> {
        let lower_input = input.to_lowercase();
        let lower_output = output.to_lowercase();

        if Self::has_correction(&lower_input) {
            return Some(Checkpoint::new(
                "user_correction".into(),
                CheckpointType::UserCorrection,
                turn,
                0,
                Self::truncate(output, 100),
                input.to_string(),
                vec![],
                tool_chain.to_vec(),
                vec![],
            ));
        }

        if Self::has_decision(&lower_output) {
            let decisions = Self::extract_decisions(output);
            return Some(Checkpoint::new(
                "decision_point".into(),
                CheckpointType::Decision,
                turn,
                0,
                Self::truncate(output, 100),
                input.to_string(),
                decisions,
                tool_chain.to_vec(),
                vec![],
            ));
        }

        if tool_chain.len() >= 3 {
            let tools: Vec<String> = tool_chain.iter().map(|t| t.tool_id.clone()).collect();
            return Some(Checkpoint::new(
                format!("tool_chain: {}", tools.join("→")),
                CheckpointType::ToolChain,
                turn,
                0,
                Self::truncate(output, 100),
                input.to_string(),
                vec![],
                tool_chain.to_vec(),
                vec![],
            ));
        }

        if let Some(last) = map.current() {
            if Self::topic_shifted(&last.intent, input) {
                return Some(Checkpoint::new(
                    "topic_shift".into(),
                    CheckpointType::TopicShift,
                    turn,
                    0,
                    Self::truncate(output, 100),
                    input.to_string(),
                    vec![],
                    tool_chain.to_vec(),
                    vec![],
                ));
            }
        }

        None
    }

    /// Check if the user's input contains a correction pattern.
    fn has_correction(input: &str) -> bool {
        let patterns = ["no,", "not that", "i meant", "actually,", "wrong",
            "that's not", "don't", "stop", "instead", "correction"];
        patterns.iter().any(|p| input.contains(p))
    }

    /// Check if the LLM's output contains a decision pattern.
    fn has_decision(output: &str) -> bool {
        let patterns = ["choose", "selected", "option", "approach", "decided",
            "better to", "recommend", "prefer", "trade-off"];
        patterns.iter().any(|p| output.contains(p))
    }

    /// Extract decisions from LLM output.
    ///
    /// First tries structured `[Decision: ...]` markers in the output.
    /// Falls back to keyword-based heuristic extraction.
    fn extract_decisions(output: &str) -> Vec<Decision> {
        let mut decisions = Vec::new();

        // Try structured markers: [Decision: chosen=X, alternatives=[Y,Z], rationale="..."]
        let marker_re = regex::Regex::new(
            r#"\[Decision:\s*chosen=([^,\]]+)(?:,\s*alternatives=\[([^\]]*)\])?(?:,\s*rationale="([^"]*)")?\]"#
        ).unwrap();
        for cap in marker_re.captures_iter(output) {
            let chosen = cap.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            let alternatives = cap.get(2).map(|m| {
                m.as_str().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
            }).unwrap_or_default();
            let rationale = cap.get(3).map(|m| m.as_str().to_string()).unwrap_or_default();
            decisions.push(Decision {
                description: Self::truncate(output, 80),
                chosen,
                rationale,
                alternatives,
            });
        }

        if !decisions.is_empty() {
            return decisions;
        }

        // Fallback: keyword heuristic
        let lower = output.to_lowercase();
        let candidates = ["choose", "selected", "decided", "recommend", "prefer"];
        for kw in candidates {
            if let Some(pos) = lower.find(kw) {
                let mut start = pos.saturating_sub(20);
                while start > 0 && !output.is_char_boundary(start) { start -= 1; }
                let mut end = (pos + 30).min(output.len());
                while end < output.len() && !output.is_char_boundary(end) { end += 1; }
                let snippet = &output[start..end];
                decisions.push(Decision {
                    description: Self::truncate(output, 80),
                    chosen: Self::truncate(snippet, 50),
                    rationale: "heuristic: keyword match".into(),
                    alternatives: vec![],
                });
                break;
            }
        }
        decisions
    }

    /// Automatically infer edge kind between two consecutive checkpoints.
    pub fn infer_edge_kind(prev: &Checkpoint, current: &Checkpoint) -> EdgeKind {
        use CheckpointType::*;
        match (&prev.type_, &current.type_) {
            (_, UserCorrection) => EdgeKind::Corrects,
            (Decision, Decision) => EdgeKind::AlternativeTo,
            (UserCorrection, _) => EdgeKind::Precedes,
            (_, TopicShift) => EdgeKind::Precedes,
            (ToolChain, Milestone) => EdgeKind::Causes,
            (Milestone, _) => EdgeKind::Causes,
            _ => EdgeKind::Precedes,
        }
    }

    /// Create an edge between the previous checkpoint and the new one.
    /// Returns None if there is no previous checkpoint.
    pub fn create_edge_for(map: &InteractionMap, new_cp_id: u32) -> Option<Edge> {
        if map.checkpoints.len() < 2 {
            return None;
        }
        let prev = map.checkpoints.iter().rev().nth(1)?;
        let current = map.checkpoint_by_id(new_cp_id)?;
        let kind = Self::infer_edge_kind(prev, current);
        Some(Edge { from: prev.id, to: new_cp_id, kind })
    }

    /// Detect topic shift using Jaccard similarity on content words (>3 chars).
    fn topic_shifted(prev_intent: &str, current_input: &str) -> bool {
        let prev_set: std::collections::HashSet<&str> = prev_intent
            .split_whitespace().filter(|w| w.len() > 3).collect();
        let curr_set: std::collections::HashSet<&str> = current_input
            .split_whitespace().filter(|w| w.len() > 3).collect();

        if prev_set.is_empty() || curr_set.is_empty() {
            return false;
        }

        let intersection = prev_set.intersection(&curr_set).count();
        let min_len = prev_set.len().min(curr_set.len());
        let jaccard = intersection as f64 / min_len as f64;
        jaccard < 0.3
    }

    fn truncate(s: &str, max_chars: usize) -> String {
        match s.char_indices().nth(max_chars) {
            Some((i, _)) => format!("{}...", &s[..i]),
            None => s.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tc(tool_id: &str) -> ToolCallRecord {
        ToolCallRecord { tool_id: tool_id.into(), params: Value::Null, result: "ok".into() }
    }

    #[test]
    fn test_empty_map() {
        let map = InteractionMap::new();
        assert_eq!(map.checkpoints.len(), 0);
        assert!(map.status_block().contains("root"));
    }

    #[test]
    fn test_add_checkpoint() {
        let mut map = InteractionMap::new();
        map.add_checkpoint(Checkpoint::new(
            "test".into(), CheckpointType::Milestone, 1, 0,
            "summary".into(), "intent".into(), vec![], vec![], vec![],
        ));
        assert_eq!(map.checkpoints.len(), 1);
        assert_eq!(map.checkpoints[0].id, 1);
    }

    #[test]
    fn test_tool_chain_milestone() {
        let map = InteractionMap::new();
        let chain = vec![
            make_tc("fs_search"),
            make_tc("fs_read"),
            make_tc("fs_edit"),
        ];
        let r = MapAnalyzer::analyze_turn("edit file", "done", &chain, 1, &map);
        assert!(r.is_some());
        assert_eq!(r.unwrap().type_, CheckpointType::ToolChain);
    }

    #[test]
    fn test_user_correction() {
        let map = InteractionMap::new();
        let r = MapAnalyzer::analyze_turn("no, that's wrong", "ok", &[], 1, &map);
        assert!(r.is_some());
        assert_eq!(r.unwrap().type_, CheckpointType::UserCorrection);
    }

    #[test]
    fn test_recall() {
        let mut map = InteractionMap::new();
        let id = map.add_checkpoint(Checkpoint::new(
            "plan".into(), CheckpointType::Decision, 1, 0,
            "chose A".into(), "which".into(),
            vec![Decision {
                description: "A vs B".into(), chosen: "A".into(),
                rationale: "simpler".into(), alternatives: vec!["B".into()],
            }],
            vec![], vec![],
        ));
        let r = map.recall(id);
        assert!(r.is_some());
        assert_eq!(r.unwrap()["label"], "plan");
    }

    #[test]
    fn test_path_info() {
        let mut map = InteractionMap::new();
        map.add_checkpoint(Checkpoint::new(
            "analysis".into(), CheckpointType::TopicShift, 1, 0,
            "".into(), "".into(), vec![], vec![], vec![],
        ));
        map.add_checkpoint(Checkpoint::new(
            "impl".into(), CheckpointType::TopicShift, 2, 0,
            "".into(), "".into(), vec![], vec![], vec![],
        ));
        let info = map.path_info();
        assert_eq!(info["completed"].as_array().unwrap().len(), 2);
        assert_eq!(info["total"], 2);
    }

    #[test]
    fn test_topic_shift_detection() {
        let mut map = InteractionMap::new();
        map.add_checkpoint(Checkpoint::new(
            "code review".into(), CheckpointType::TopicShift, 1, 0,
            "".into(), "review the rust code in main.rs".into(),
            vec![], vec![], vec![],
        ));
        let r = MapAnalyzer::analyze_turn(
            "search web for rust async patterns",
            "found results",
            &[],
            2,
            &map,
        );
        assert!(r.is_some());
        assert_eq!(r.unwrap().type_, CheckpointType::TopicShift);
    }

    #[test]
    fn test_status_block_with_checkpoints() {
        let mut map = InteractionMap::new();
        map.add_checkpoint(Checkpoint::new(
            "analysis".into(), CheckpointType::TopicShift, 1, 0,
            "".into(), "".into(), vec![], vec![], vec![],
        ));
        let status = map.status_block();
        assert!(status.contains("#1/1"));
        assert!(status.contains("analysis"));
    }

    #[test]
    fn test_cycle_detection() {
        let mut map = InteractionMap::new();
        map.add_checkpoint(Checkpoint::new("a".into(), CheckpointType::Milestone, 1, 0, "".into(), "".into(), vec![], vec![], vec![]));
        map.add_checkpoint(Checkpoint::new("b".into(), CheckpointType::Milestone, 2, 0, "".into(), "".into(), vec![], vec![], vec![]));
        map.add_checkpoint(Checkpoint::new("c".into(), CheckpointType::Milestone, 3, 0, "".into(), "".into(), vec![], vec![], vec![]));
        let id_a = 1; let id_b = 2; let id_c = 3;

        // a → b → c: no cycle
        map.add_edge(Edge { from: id_a, to: id_b, kind: EdgeKind::Precedes });
        map.add_edge(Edge { from: id_b, to: id_c, kind: EdgeKind::Precedes });
        assert!(!map.would_create_cycle(id_a, id_c));

        // c → a: would create a cycle
        assert!(map.would_create_cycle(id_c, id_a));
        assert!(map.try_add_edge(Edge { from: id_c, to: id_a, kind: EdgeKind::Precedes }).is_err());
    }

    #[test]
    fn test_structured_decision_extraction() {
        let output = r#"I've analyzed the options. [Decision: chosen=Redis, alternatives=[Memcached, PostgreSQL], rationale="Best latency"]"#;
        let decisions = MapAnalyzer::extract_decisions(output);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].chosen, "Redis");
        assert_eq!(decisions[0].alternatives.len(), 2);
        assert_eq!(decisions[0].rationale, "Best latency");
    }

    #[test]
    fn test_keyword_decision_fallback() {
        let output = "After careful consideration, I recommend using Redis for caching.";
        let decisions = MapAnalyzer::extract_decisions(output);
        assert_eq!(decisions.len(), 1);
        assert!(decisions[0].chosen.contains("recommend"));
    }

    #[test]
    fn test_infer_edge_kind() {
        let milestone = Checkpoint::new("m".into(), CheckpointType::Milestone, 1, 0, "".into(), "".into(), vec![], vec![], vec![]);
        let next = Checkpoint::new("n".into(), CheckpointType::UserCorrection, 2, 0, "".into(), "".into(), vec![], vec![], vec![]);
        assert_eq!(MapAnalyzer::infer_edge_kind(&milestone, &next), EdgeKind::Corrects);
    }

    #[test]
    fn test_try_add_edge_cycle() {
        let mut map = InteractionMap::new();
        map.add_checkpoint(Checkpoint::new("x".into(), CheckpointType::Milestone, 1, 0, "".into(), "".into(), vec![], vec![], vec![]));
        map.add_edge(Edge { from: 1, to: 1, kind: EdgeKind::Precedes });
        assert!(map.would_create_cycle(1, 1));
        assert!(map.try_add_edge(Edge { from: 1, to: 1, kind: EdgeKind::Precedes }).is_err());
    }
}
