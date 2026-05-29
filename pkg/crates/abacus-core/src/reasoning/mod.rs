//! Reasoning 子系统（P2）
//!
//! ## 模块
//! - `consistency`: Self-Consistency 投票器（Wang et al. 2022）
//! - `tot`: Tree of Thoughts 规划搜索（Yao et al. 2023）
//!
//! ## 使用时机
//! - `consistency`：高风险推理任务（数学/调试），ThinkingIntent=High
//! - `tot`：规划/架构设计任务，需要探索多条可能路径

pub mod consistency;
pub mod tot;
