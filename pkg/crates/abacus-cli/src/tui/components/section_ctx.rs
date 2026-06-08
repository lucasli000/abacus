//! AppContext —— SectionContext 的 abacus-cli 内置实现
//!
//! 包裹 [`&AppState`](crate::tui::state::AppState), 让所有内置 Section 通过
//! [`SectionContext::ext`] 反查到完整 state 类型。
//!
//! ## 设计意图
//!
//! `abacus-ui-kit::SectionContext` 故意只暴露 theme + focus_pulsing + anim_tick 三个 getter,
//! 避免业务字段（如 `session_tokens` / `tool_records`）外溢到公开 trait 签名。
//! 内置 Section 需要更多字段时, 通过下面的 [`downcast_app_state`] helper 拿到 `&AppState`。
//!
//! 第三方 Agent 应用的 Section impl 一般只用 trait 默认 getter; 需要扩展数据时定义自己
//! 的 SectionContext 实现 + 自己的 downcast helper。
//!
//! ## 生命周期
//!
//! `AppContext<'a>` 是借用 `&'a AppState` 的瞬时包装, 每帧 render 时构造一次, 不跨帧持有。
//!
//! ## 类型擦除安全契约
//!
//! [`AppContext::ext`] 返回 `*const AppState`（cast 为 `*const ()` 后存储）。
//! [`downcast_app_state`] 内部先校验 `ext_type_id == TypeId::of::<AppState>()`, 然后才
//! reinterpret 指针。校验失败返回 None, 不 unsafe。

use std::any::TypeId;

use abacus_ui_kit::{SectionContext, Theme};

use crate::tui::state::AppState;

/// abacus-cli 内置 SectionContext —— 借用 AppState 的瞬时适配器
pub struct AppContext<'a> {
    pub state: &'a AppState,
}

impl<'a> AppContext<'a> {
    pub fn new(state: &'a AppState) -> Self {
        Self { state }
    }
}

impl<'a> SectionContext for AppContext<'a> {
    fn theme(&self) -> &Theme {
        &self.state.theme
    }

    fn focus_pulsing(&self) -> bool {
        self.state.focus_pulsing()
    }

    fn anim_tick(&self) -> u64 {
        self.state.anim_tick.get()
    }

    fn ext_type_id(&self) -> Option<TypeId> {
        // 用 AppState 自身的 TypeId 作为身份证, 内置 Section downcast 时核对此值
        Some(TypeId::of::<AppState>())
    }

    fn ext(&self) -> Option<*const ()> {
        // 把 &AppState 转为 *const () 类型擦除指针
        // SAFETY: 指针生命周期与 self 一致, 调用方通过 downcast_app_state 安全获取
        Some(self.state as *const AppState as *const ())
    }
}

/// 内置 Section helper —— 从 dyn SectionContext 反查 &AppState
///
/// ## 用法（内置 Section 标准模板）
/// ```ignore
/// fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
///     let Some(state) = downcast_app_state(ctx) else { return; };
///     // 直接访问 state.session_tokens / state.tool_records ...
/// }
/// ```
///
/// ## 失败语义
///
/// 当 ctx 不是 `AppContext` 时返回 None。内置 Section 应直接 early return（视为渲染跳过, 不 panic）。
/// 这种情况只在第三方 Agent 错误地把内置 Section 注入到自己的 registry 时发生 —— 容错处理。
///
/// ## 安全契约
///
/// 内部通过 `ext_type_id == TypeId::of::<AppState>()` 校验后才 cast 指针, 不会发生
/// 跨类型 UB。返回引用生命周期被 ctx 借用约束（'a 关联到 &'a dyn SectionContext）。
pub(crate) fn downcast_app_state<'a>(ctx: &'a dyn SectionContext) -> Option<&'a AppState> {
    if ctx.ext_type_id() != Some(TypeId::of::<AppState>()) {
        return None;
    }
    let ptr = ctx.ext()?;
    // SAFETY:
    // 1. ext_type_id 已校验类型匹配
    // 2. 指针来源是 `&AppState` cast (见 AppContext::ext), 非空
    // 3. 'a 生命周期与 ctx 一致, 反查得到的引用不会比 ctx 活得更久
    let state: &'a AppState = unsafe { &*(ptr as *const AppState) };
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::AbacusMode;

    #[test]
    fn app_context_implements_section_context() {
        let state = AppState::new(AbacusMode::Clarify);
        let ctx = AppContext::new(&state);
        // SectionContext trait 方法可调
        let _theme: &Theme = ctx.theme();
        let _: bool = ctx.focus_pulsing();
        let _: u64 = ctx.anim_tick();
    }

    #[test]
    fn downcast_returns_some_for_app_context() {
        let state = AppState::new(AbacusMode::Clarify);
        let ctx = AppContext::new(&state);
        let ctx_dyn: &dyn SectionContext = &ctx;
        let downcasted = downcast_app_state(ctx_dyn);
        assert!(downcasted.is_some());
        // 反查得到的引用应与原 state 是同一对象（指针相同）
        let s = downcasted.unwrap();
        assert!(std::ptr::eq(s, &state));
    }

    #[test]
    fn downcast_returns_none_for_other_context() {
        // 模拟第三方 ctx 实现 (不实现 ext_type_id / ext, 用默认 None)
        struct ForeignCtx {
            theme: Theme,
        }
        impl SectionContext for ForeignCtx {
            fn theme(&self) -> &Theme {
                &self.theme
            }
        }
        let foreign = ForeignCtx { theme: Theme::brand() };
        let ctx_dyn: &dyn SectionContext = &foreign;
        let downcasted = downcast_app_state(ctx_dyn);
        assert!(downcasted.is_none());
    }

    #[test]
    fn downcast_rejects_mismatched_type_id() {
        // 模拟一个第三方 ctx 实现 ext 但 type_id 不匹配
        struct WrongIdCtx {
            theme: Theme,
            dummy: u64,
        }
        impl SectionContext for WrongIdCtx {
            fn theme(&self) -> &Theme {
                &self.theme
            }
            fn ext_type_id(&self) -> Option<TypeId> {
                Some(TypeId::of::<u64>()) // 故意错的
            }
            fn ext(&self) -> Option<*const ()> {
                Some(&self.dummy as *const u64 as *const ())
            }
        }
        let ctx = WrongIdCtx { theme: Theme::brand(), dummy: 42 };
        let ctx_dyn: &dyn SectionContext = &ctx;
        let downcasted = downcast_app_state(ctx_dyn);
        assert!(downcasted.is_none(), "type_id 不匹配应返回 None, 不应误 cast");
    }
}
