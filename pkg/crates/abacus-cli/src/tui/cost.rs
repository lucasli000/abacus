//! 模型费用估算 / 显示格式化
//!
//! V31: 重写为 CNY canonical
//! - 历史 V28.7 设计：USD 为基准货币，CNY 经写死汇率 7.2 折算 → 双重失真（USD 数字本身偏离 + 7.2 与实际 FX 不符）
//! - V31 设计：CNY 为基准（DeepSeek 官方计费货币），USD 经 fx_rate 现算
//! - 看板渲染主显 ¥，次显 $（符合中文用户实际付款货币）
//!
//! 引用关系：
//! - 生产者：run.rs 在 EngineResponse.stats 抵达时累加 SessionTokenStats.cost_cny / cost_usd
//! - 消费者：components::render_panel_overview 看板统计区
//! - 数据：abacus_types::lookup_model_or_default（按 model_id 精确查注册表）
//!
//! 生命周期：纯函数无状态

use abacus_types::{lookup_model_or_default, DEFAULT_CNY_TO_USD_RATE};

/// 估算单轮 turn 的费用（CNY canonical）
///
/// ## 引用关系
/// - 写入：run.rs 累加到 SessionTokenStats.cost_cny
/// - 数据：lookup_model_or_default(model).turn_cost_cny
pub fn estimate_turn_cost_cny(
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
) -> f64 {
    lookup_model_or_default(model).turn_cost_cny(prompt_tokens, completion_tokens, cached_tokens)
}

/// 估算单轮 turn 的费用（USD）—— 由 CNY 经 fx_rate 折算
///
/// fx_rate 单位：CNY per USD（7.10 = 1 USD ≈ 7.10 CNY）
/// fx_rate <= 0 时返回 0（防御）
pub fn estimate_turn_cost_usd(
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    fx_rate: f64,
) -> f64 {
    lookup_model_or_default(model).turn_cost_usd(prompt_tokens, completion_tokens, cached_tokens, fx_rate)
}

/// 默认 CNY/USD 汇率（V31: 改为 7.10，更接近 2026-05 实情）
/// 历史 USD_TO_CNY_RATE = 7.2（偏高 ~1.4%）
pub const DEFAULT_FX_RATE: f64 = DEFAULT_CNY_TO_USD_RATE;

/// 把 CNY 格式化为人类可读字符串
///
/// 显示策略：
/// - cny <= 0 → "¥0.00"
/// - 0 < cny < 0.0001 → "<¥0.0001"（极小值，避免显示为 0 误以为免费）
/// - 0.0001 ≤ cny < 0.01 → "¥0.0034"（4 位小数）
/// - cny ≥ 0.01 → "¥0.0234"（4 位小数）
pub fn format_cny(cny: f64) -> String {
    if cny <= 0.0 {
        "¥0.0000".to_string()
    } else if cny < 0.0001 {
        "<¥0.0001".to_string()
    } else {
        format!("¥{:.4}", cny)
    }
}

/// 把 USD 格式化为人类可读字符串：< $0.0001 显示 "<$0.0001"，否则保留 4 位小数
pub fn format_usd(usd: f64) -> String {
    if usd <= 0.0 {
        "$0.0000".to_string()
    } else if usd < 0.0001 {
        "<$0.0001".to_string()
    } else {
        format!("${:.4}", usd)
    }
}

/// 同时格式化 ¥ 主 + $ 次（看板首选格式）
///
/// 例：format_cny_with_usd(0.0234, 7.10) → "¥0.0234 ≈ $0.0033"
pub fn format_cny_with_usd(cny: f64, fx_rate: f64) -> String {
    let usd = if fx_rate > 0.0 { cny / fx_rate } else { 0.0 };
    format!("{} ≈ {}", format_cny(cny), format_usd(usd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cny_v4_pro_1m_each() {
        // V4-Pro 1M+1M cached=0 → ¥3 + ¥6 = ¥9
        let cost = estimate_turn_cost_cny("deepseek-v4-pro", 1_000_000, 1_000_000, 0);
        assert!((cost - 9.0).abs() < 1e-6, "got {}", cost);
    }

    #[test]
    fn cny_v4_flash_with_cache() {
        // V4-Flash 1M cached + 1M output → ¥0.02 + ¥2 = ¥2.02
        let cost = estimate_turn_cost_cny("deepseek-v4-flash", 1_000_000, 1_000_000, 1_000_000);
        assert!((cost - 2.02).abs() < 1e-6, "got {}", cost);
    }

    #[test]
    fn usd_via_fx_rate() {
        let usd = estimate_turn_cost_usd("deepseek-v4-pro", 1_000_000, 1_000_000, 0, 7.10);
        // ¥9 / 7.10 ≈ $1.268
        assert!((usd - 1.268).abs() < 0.01, "got {}", usd);
    }

    #[test]
    fn unknown_model_falls_back_v4_flash() {
        // 未知模型 → V4-Flash 价位 ¥1+¥2 = ¥3
        let cost = estimate_turn_cost_cny("gpt-99-mystery", 1_000_000, 1_000_000, 0);
        assert!((cost - 3.0).abs() < 1e-6, "got {}", cost);
    }

    #[test]
    fn format_cny_handles_tiny_values() {
        assert_eq!(format_cny(0.0), "¥0.0000");
        assert_eq!(format_cny(0.00005), "<¥0.0001");
        assert_eq!(format_cny(0.1234), "¥0.1234");
    }

    #[test]
    fn format_usd_handles_tiny_values() {
        assert_eq!(format_usd(0.0), "$0.0000");
        assert_eq!(format_usd(0.00005), "<$0.0001");
        assert_eq!(format_usd(0.1234), "$0.1234");
    }

    #[test]
    fn format_cny_with_usd_combined() {
        let s = format_cny_with_usd(0.0234, 7.10);
        assert!(s.contains("¥0.0234"));
        assert!(s.contains("$0.00"));
        assert!(s.contains("≈"));
    }
}
