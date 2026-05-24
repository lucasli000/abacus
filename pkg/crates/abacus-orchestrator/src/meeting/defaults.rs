//! # 默认 Specialist 注册配置
//!
//! ## 场景
//! 仅当用户未提供 YAML 配置文件时的回退方案。
//! 生产环境应通过 `--config meeting.yaml` 提供领域特定专家。
//!
//! ## 配置文件示例
//! ```bash
//! abacus meeting --topic "设计评审" --specialist ux_designer,pm --config examples/meeting-product-design.yaml
//! ```
//!
//! ## 边界
//! - 硬编码的 Chinese 默认值仅用于 demo，不适用于生产
//! - 配置 YAML 在 `examples/` 目录下:
//!   - meeting-code-review.yaml (代码审查)
//!   - meeting-finance.yaml (投资策略)
//!   - meeting-product-design.yaml (产品设计)

use crate::specialist::SpecialistRegistration;

/// 返回默认专家列表（demo 用途）
///
/// 当用户未通过 --config 提供配置时，注册一个通用 fallback 专家。
/// 用户应通过 YAML 配置提供领域专家。
pub fn default_specialists() -> Vec<SpecialistRegistration> {
    vec![]
}
