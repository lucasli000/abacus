//! Quant Agent 自定义看板 PoC — 验证 abacus-ui-kit 跨 crate Section 扩展能力
//!
//! ## 用途
//!
//! 演示**外部 Agent 应用**如何通过 [`abacus_ui_kit::Section`] trait 注入自定义看板区块,
//! **不修改 abacus-cli 内部代码**。此 example 模拟一个量化交易 Agent 想在看板里展示:
//!
//! - **持仓盈亏 Section** —— 当前持仓 + 当日浮盈
//! - **风控告警 Section** —— 实时风险敞口
//! - **行情 sparkline Section** —— 用 ui-kit 的 `SpinnerFrame` 类似 helper 显示行情走势
//!
//! ## 运行
//!
//! ```bash
//! cargo run -p abacus-cli --example quant_panel
//! ```
//!
//! 这个 demo 不连接真实交易系统, 只渲染一帧到 stdout 然后退出。生产场景下 Agent
//! 应用会把这些 Section 注入到 abacus-cli 主 TUI 的 `state.section_registry` 中。
//!
//! ## 类型边界验证（关键）
//!
//! - 此 example 只依赖 `abacus-ui-kit` + `ratatui` —— **不引入 abacus-cli 任何符号**
//! - 因此证明 abacus-ui-kit 是真正的"跨 crate UI 契约 crate"
//! - 第三方 Agent 应用照此模式可在自己的 crate 实现 Section, 然后在 binary 启动时注册

use abacus_ui_kit::{Section, SectionContext, SectionRegistry, Theme};
use ratatui::Frame;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// QuantContext —— 自定义 SectionContext, 携带量化业务数据
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Debug, Clone)]
struct Position {
    symbol: String,
    qty: i32,
    avg_cost: f64,
    last_price: f64,
}

impl Position {
    fn pnl(&self) -> f64 {
        (self.last_price - self.avg_cost) * self.qty as f64
    }
}

struct QuantContext {
    theme: Theme,
    positions: Vec<Position>,
    risk_alerts: Vec<String>,
    /// 行情序列（最近 N 个 tick 价格）
    price_series: Vec<f64>,
}

impl SectionContext for QuantContext {
    fn theme(&self) -> &Theme {
        &self.theme
    }

    /// 关键: 把自己的指针类型擦除暴露, downcast helper 反查
    fn ext_type_id(&self) -> Option<std::any::TypeId> {
        Some(std::any::TypeId::of::<QuantContext>())
    }

    fn ext(&self) -> Option<*const ()> {
        Some(self as *const QuantContext as *const ())
    }
}

/// 内部 helper —— 安全 downcast 拿 &QuantContext
fn downcast_quant<'a>(ctx: &'a dyn SectionContext) -> Option<&'a QuantContext> {
    if ctx.ext_type_id() != Some(std::any::TypeId::of::<QuantContext>()) {
        return None;
    }
    let ptr = ctx.ext()?;
    // SAFETY: ext_type_id 校验类型, 生命周期 'a 关联到 ctx
    Some(unsafe { &*(ptr as *const QuantContext) })
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// PositionsSection —— 持仓盈亏
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

struct PositionsSection;

impl Section for PositionsSection {
    fn id(&self) -> &str {
        "com.example.quant.positions"
    }

    fn title(&self) -> &str {
        "持仓盈亏"
    }

    fn min_height(&self) -> u16 {
        2
    }

    fn preferred_height(&self, _: u16) -> u16 {
        5
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(qc) = downcast_quant(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);
        let mut lines = vec![Line::from(vec![
            Span::styled("  ─ 持仓盈亏 ", dim),
            Span::styled(
                format!("({} 标的)", qc.positions.len()),
                Style::default().fg(theme.muted),
            ),
        ])];
        for p in &qc.positions {
            let pnl = p.pnl();
            let (sign, color) = if pnl >= 0.0 {
                ("+", theme.success)
            } else {
                ("", theme.error)
            };
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled(
                    format!("{:<6} ×{:<4}", p.symbol, p.qty),
                    Style::default().fg(theme.text),
                ),
                Span::styled(
                    format!(" @{:.2} → {:.2}  ", p.avg_cost, p.last_price),
                    Style::default().fg(theme.muted),
                ),
                Span::styled(
                    format!("{}{:.2}", sign, pnl),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// RiskAlertsSection —— 风控告警
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

struct RiskAlertsSection;

impl Section for RiskAlertsSection {
    fn id(&self) -> &str {
        "com.example.quant.risk"
    }

    fn title(&self) -> &str {
        "风控告警"
    }

    fn min_height(&self) -> u16 {
        2
    }

    fn preferred_height(&self, _: u16) -> u16 {
        4
    }

    fn visible(&self, ctx: &dyn SectionContext) -> bool {
        // 只有有告警时才显示（演示 visible() 用法）
        downcast_quant(ctx)
            .map(|qc| !qc.risk_alerts.is_empty())
            .unwrap_or(false)
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(qc) = downcast_quant(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let mut lines = vec![Line::from(vec![Span::styled(
            "  ─ ⚠ 风控告警 ",
            Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
        )])];
        for alert in &qc.risk_alerts {
            lines.push(Line::from(vec![
                Span::styled("    ! ", Style::default().fg(theme.error)),
                Span::styled(alert.clone(), Style::default().fg(theme.text)),
            ]));
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// PriceSparklineSection —— 行情走势 sparkline
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

struct PriceSparklineSection;

impl Section for PriceSparklineSection {
    fn id(&self) -> &str {
        "com.example.quant.sparkline"
    }

    fn title(&self) -> &str {
        "行情走势"
    }

    fn min_height(&self) -> u16 {
        2
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(qc) = downcast_quant(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);

        let spark_chars = "▁▂▃▄▅▆▇█";
        let values = &qc.price_series;
        let max_val = values.iter().cloned().fold(f64::MIN, f64::max);
        let min_val = values.iter().cloned().fold(f64::MAX, f64::min);
        let range = (max_val - min_val).max(0.01);
        let target_width = (area.width as usize).saturating_sub(6).min(values.len());
        let spark: String = values.iter().rev().take(target_width).rev().map(|v| {
            let idx = ((v - min_val) / range * 7.0) as usize;
            spark_chars.chars().nth(idx.min(7)).unwrap_or('▁')
        }).collect();

        let last = values.last().copied().unwrap_or(0.0);
        let first = values.first().copied().unwrap_or(0.0);
        let change = last - first;
        let (sign, color) = if change >= 0.0 {
            ("+", theme.success)
        } else {
            ("", theme.error)
        };

        let lines = vec![
            Line::from(vec![Span::styled("  ─ 行情走势", dim)]),
            Line::from(vec![
                Span::styled("    ", dim),
                Span::styled(spark, Style::default().fg(color)),
                Span::styled(
                    format!("  {:.2} ({}{:.2})", last, sign, change),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// main —— 注册 + 渲染一帧
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn main() {
    // ── 第三方 Agent 应用准备业务数据 ──
    let ctx = QuantContext {
        theme: Theme::brand(),
        positions: vec![
            Position { symbol: "AAPL".into(), qty: 100, avg_cost: 175.0, last_price: 182.5 },
            Position { symbol: "TSLA".into(), qty: 50, avg_cost: 240.0, last_price: 225.3 },
            Position { symbol: "NVDA".into(), qty: 20, avg_cost: 800.0, last_price: 905.0 },
        ],
        risk_alerts: vec![
            "TSLA 单标的占比超 30% 风险阈值".into(),
        ],
        price_series: vec![
            100.0, 102.0, 101.5, 103.0, 105.5, 104.0, 106.5, 108.0, 107.5, 109.0,
            110.5, 109.5, 111.0, 112.5, 114.0, 113.0, 115.5, 117.0, 116.5, 118.0,
        ],
    };

    // ── 注册自定义 Section ──
    let mut registry = SectionRegistry::new();
    registry.register(Box::new(PositionsSection));
    registry.register(Box::new(RiskAlertsSection));
    registry.register(Box::new(PriceSparklineSection));

    // 启用列表 — 顺序由 Agent 应用决定
    let layout = vec![
        "com.example.quant.positions",
        "com.example.quant.risk",
        "com.example.quant.sparkline",
    ];

    // ── 渲染一帧到 TestBackend, 打印结果 ──
    let backend = TestBackend::new(60, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| {
        let stack = registry.build_stack(&layout);
        let area = Rect::new(0, 0, 60, 16);
        stack.render(f, &ctx, area);
    }).unwrap();

    // 打印 backend buffer 内容（验证渲染成功）
    let buffer: &Buffer = terminal.backend().buffer();
    println!("═══ Quant Agent 自定义看板渲染输出 ═══");
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    println!("═══ 完成 ═══");
    println!("\n✓ 跨 crate 扩展验证通过");
    println!("  - 3 个自定义 Section 注册成功");
    println!("  - 通过 SectionStack 自动布局");
    println!("  - 通过 SectionContext::ext + ext_type_id 安全访问业务数据");
    println!("  - RiskAlertsSection.visible() 控制条件显示");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> QuantContext {
        QuantContext {
            theme: Theme::brand(),
            positions: vec![Position {
                symbol: "TEST".into(),
                qty: 10,
                avg_cost: 100.0,
                last_price: 110.0,
            }],
            risk_alerts: vec![],
            price_series: vec![1.0, 2.0, 3.0],
        }
    }

    #[test]
    fn position_pnl_calculation() {
        let p = Position {
            symbol: "X".into(),
            qty: 10,
            avg_cost: 100.0,
            last_price: 110.0,
        };
        assert_eq!(p.pnl(), 100.0);
    }

    #[test]
    fn downcast_quant_returns_some_for_quant_ctx() {
        let ctx = make_ctx();
        let dyn_ctx: &dyn SectionContext = &ctx;
        let qc = downcast_quant(dyn_ctx);
        assert!(qc.is_some());
        assert_eq!(qc.unwrap().positions.len(), 1);
    }

    #[test]
    fn risk_alerts_section_hidden_when_no_alerts() {
        let ctx = make_ctx();
        let s = RiskAlertsSection;
        assert!(!s.visible(&ctx)); // 无告警 → 隐藏
    }

    #[test]
    fn risk_alerts_section_visible_with_alerts() {
        let mut ctx = make_ctx();
        ctx.risk_alerts.push("test".into());
        let s = RiskAlertsSection;
        assert!(s.visible(&ctx));
    }

    #[test]
    fn registry_builds_stack_with_3_custom_sections() {
        let mut registry = SectionRegistry::new();
        registry.register(Box::new(PositionsSection));
        registry.register(Box::new(RiskAlertsSection));
        registry.register(Box::new(PriceSparklineSection));

        let stack = registry.build_stack(&[
            "com.example.quant.positions",
            "com.example.quant.risk",
            "com.example.quant.sparkline",
        ]);
        assert_eq!(stack.len(), 3);
    }
}
