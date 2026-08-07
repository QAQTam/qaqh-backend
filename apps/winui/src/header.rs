//! XAML 原生标题栏（P0）— ThreadHeader 的壳侧承载（PLAN-NATIVE-UI.md）。
//!
//! 布局：
//!   TitleBar（SetTitleBar 拖拽区，host 自动接线 host.rs:277-288）
//!   ├── title 槽：TextBlock（会话标题 / 视图名，shell.header 推送）
//!   └── footer 槽：hstack( ①workspace ②location ③console ┃ ④info ⑤stats ⑥undo ⑦compact )
//!        —— ⑧pet 不渲染（壳 stub 恒 false，规划决策）
//!
//! 状态：timer 轮询 `core.header_snapshot()` rev（同 sidebar 500ms 模式，
//! 经 `shell::poll_rev` helper，P-4）。
//! 动作：①②③ 壳直接处理（目录对话框 / 系统 shell / DevTools）；
//!       ④-⑦ emit `shell.headerAction` 回传 Web 执行（状态单一数据源在 Web）。

use std::sync::Arc;
use std::time::Duration;

use windows_reactor::*;

use crate::bridge::{Bridge, HeaderFlag, HeaderState};

/// 标题栏高度（PLAN-NATIVE-UI.md 布局：row 0 = 48px）。
pub const HEADER_HEIGHT: f64 = 48.0;

/// 图标按钮（subtle；active = accent 高亮）。
///
/// reactor 未封装 ToolTipService → icon-only，与侧栏 `icon_button` 视觉
/// 语言一致（偏差 D6，WORKFLOW §3）。
fn action_button(
    icon: Icon,
    enabled: bool,
    active: bool,
    on_click: impl Fn() + 'static,
) -> Element {
    let mut btn = button("")
        .icon(icon)
        .subtle()
        .enabled(enabled)
        .on_click(on_click);
    if active {
        btn = btn.accent();
    }
    btn.into()
}

/// XAML 标题栏组件（放入外层 Grid row 0；host 检测到 TitleBar 自动接线）。
pub fn header(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (state, set_state) = cx.use_state::<HeaderState>(HeaderState::default());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);

    // 首次挂载：500ms rev 轮询（同 sidebar 模式；shell::poll_rev helper）。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_state = set_state.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        move || {
            crate::shell::poll_rev(
                timer,
                last_rev,
                Duration::from_millis(500),
                move || bridge.core().header_snapshot(),
                move |s| set_state.call(s),
            );
        }
    });

    // 系统主题：reactor 由 ActualThemeChanged 驱动更新（engine.rs:733），
    // WebView 移除后无需回传 Web——轮询与 emit_theme_changed 一并删除。

    // ── 点击分发（①②③ 壳直接；④-⑦ 直连动作，协议请求 Rust 直发）──
    let on_workspace = {
        let bridge = bridge.clone();
        move || match bridge.pick_workspace_directory() {
            // 取消 → Ok(null) → 不动作；选择 → 直发 workspace.set（不再回传 Web）。
            Ok(serde_json::Value::String(path)) => {
                bridge.spawn_workspace_set(path);
            }
            _ => {}
        }
    };
    let on_location = {
        let bridge = bridge.clone();
        let workspace = state.workspace.clone();
        move || {
            if !workspace.is_empty() {
                let _ = bridge.open_path(&workspace);
            }
        }
    };
    let on_info = {
        let bridge = bridge.clone();
        move || bridge.toggle_header_flag(HeaderFlag::Info)
    };
    let on_stats = {
        let bridge = bridge.clone();
        move || bridge.toggle_header_flag(HeaderFlag::Stats)
    };
    let on_undo = {
        let bridge = bridge.clone();
        move || bridge.spawn_undo_last_turn()
    };
    let on_compact = {
        let bridge = bridge.clone();
        move || {
            bridge.spawn_conversation_command(serde_json::json!({ "type": "conversation_compact" }))
        }
    };

    // ── footer 槽：7 个 subtle 图标按钮（⑧pet 隐藏，规划决策）──
    let divider: Element = border(text_block(""))
        .width(1.0)
        .height(18.0)
        .background(ThemeRef::DividerStroke)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    let footer: Element = hstack((
        // 图标映射（bindings Symbol 枚举裁剪版，WORKFLOW §6.1 记录）：
        // ①OpenLocal ②OpenFile ┃ ③ContactInfo(替代 Info) ④FourBars(替代 Diagnostic)
        // ⑤Undo ⑥Clear(替代 Compress)（⑧pet 隐藏；console 随 WebView 移除）。
        action_button(Icon::symbol(Symbol::OpenLocal), true, false, on_workspace),
        action_button(Icon::symbol(Symbol::OpenFile), true, false, on_location),
        divider,
        action_button(Icon::symbol(Symbol::ContactInfo), true, state.info_open, on_info),
        action_button(Icon::symbol(Symbol::FourBars), true, state.stats_open, on_stats),
        action_button(Icon::symbol(Symbol::Undo), !state.undo_disabled, false, on_undo),
        action_button(
            Icon::symbol(Symbol::Clear),
            !(state.compacting || state.compact_disabled),
            false,
            on_compact,
        ),
    ))
    .spacing(4.0)
    .vertical_alignment(VerticalAlignment::Center)
    .into();

    // ── TitleBar：title 槽 = 会话标题 / 视图名 ─────────────────
    TitleBar::new(&state.title)
        .footer(footer)
        .tall(false)
        .into()
}
