//! Diff 弹层（V7）：turn 末尾「查看详情」→ 全屏覆盖层面板。
//!
//! 「写端组件 / 读端覆盖层」分离模式（静态槽 + 轮询）不变，弹层本体从
//! **ContentDialog 改为全屏 grid 覆盖层**（与 interaction / diagram_zoom
//! 同款模式）：
//! - 尺寸真正可控：ContentDialog 模板硬编码 MaxWidth=548 / MaxHeight=756
//!   （决策对话框的设计约束），diff 浏览这类大内容塞进去会被钳制（实测
//!   面板 546×753，70% 缩放完全失效）；覆盖层 grid 无模板钳制，规则完整生效
//! - 模态遮罩自绘（半透明黑，拦截主界面输入；点击遮罩不关闭，保留
//!   ContentDialog 语义）
//! - Esc（KeyboardAccelerator）/ ✕ 按钮关闭
//!
//! 历史：V4 transition（隐式动画挂载不播 → 灰屏）→ V5 帧驱动软件动画
//! （16ms 全量重渲染 → 15fps 卡顿感）→ V6 原生 ContentDialog → V6.1
//! （常驻 + 尺寸/阴影修复）→ **V7 覆盖层**：V6 系列的遮罩残留竞态与模板
//! 钳制均为 ContentDialog 架构固有，弃用后不再存在。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use deepx_client::{TimelineBlockKind, TimelineTurn};
use markdown_winui::{DiffFile, diff_file_view};
use windows_reactor::*;

use crate::bridge::Bridge;

/// 弹层数据：Diff 文件列表（工具块 ToolBody::Diff 消费后的视图数据）
/// 或子代理运行记录（面板按需拉取 timeline 渲染，关闭即释放）。
#[derive(Clone, PartialEq)]
pub enum DrawerRequest {
    /// 文件 diff 浏览（原 DiffDrawerRequest 语义）。
    Diff {
        turn_id: String,
        files: Vec<DrawerFile>,
    },
    /// 子代理运行记录：`seed` = 子代理 Ringing seed（timeline 数据源），
    /// `name` = 子代理名（头部显示）。
    Subagent {
        seed: String,
        name: String,
    },
}

/// 单文件条目：路径 + 统计 + 状态 + 已解析 diff（渲染用）。
#[derive(Clone, PartialEq)]
pub struct DrawerFile {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// 对应工具失败（✕ 红；diff 可能为空）。
    pub failed: bool,
    /// 已解析的单文件 diff（path 与其 display_path 一致）。
    pub file: DiffFile,
}

/// 静态槽（写端组件 / 读端覆盖层分离）。
pub static DRAWER_SLOT: Mutex<Option<DrawerRequest>> = Mutex::new(None);

/// 写端：打开弹层（chat_view 总结行 / 子代理胶囊调用；overlay 轮询消费）。
pub fn open_diff_drawer(req: DrawerRequest) {
    if let Ok(mut slot) = DRAWER_SLOT.lock() {
        *slot = Some(req);
    }
}

// ── 弹层尺寸常量 ──

/// 弹层 = 主窗口 70%（宽高同比例 → 保持纵横比）。
const DRAWER_RATIO: f64 = 0.70;
const DRAWER_W_MIN: f64 = 640.0;
const DRAWER_W_MAX: f64 = 1280.0;
const DRAWER_H_MIN: f64 = 480.0;
const DRAWER_H_MAX: f64 = 920.0;
const FILE_LIST_WIDTH: f64 = 280.0;

/// 轮询间隔：仅作「请求出现」检测（无动画帧驱动，80ms 足够轻）。
const POLL_INTERVAL: Duration = Duration::from_millis(80);

/// 子代理面板刷新间隔：按行刷新即可（无 token 级实时需求），
/// 500ms 轮询拉最新快照重建行视图，成本可忽略。
const SUBAGENT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 遮罩不透明度（与 diagram_zoom 同款）。
const SCRIM_ALPHA: u8 = 140;

/// 读端覆盖层：轮询静态槽 → 全屏 grid + 半透明遮罩 + 居中面板。
///
/// 无请求/未打开时返回空 grid（无背景 → 不参与命中测试，点击穿透）。
/// `bridge` 供子代理面板按需拉取 timeline（开面板才拉，关面板即释放）。
pub fn diff_drawer_overlay(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (req, set_req) = cx.use_state::<Option<DrawerRequest>>(None);
    let (open, set_open) = cx.use_state::<bool>(false);
    let (selected, set_selected) = cx.use_state::<usize>(0);
    // 子代理面板数据：最近一次拉取的回合行（渲染用；关闭即清空）。
    let (sub_turns, set_sub_turns) = cx.use_state::<Vec<TimelineTurn>>(Vec::new());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    // 子代理刷新轮询（500ms）：面板打开期间拉取最新快照按行刷新。
    let sub_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    // 当前面板的目标子代理 seed（timer 回调读取；关闭即清）。
    let active_sub_seed = cx.use_ref::<Option<String>>(None);
    let win = cx.use_inner_size();

    cx.use_effect((), {
        let set_req = set_req.clone();
        let set_open = set_open.clone();
        let timer = timer.clone();
        move || {
            if timer.borrow().is_some() {
                return;
            }
            if let Ok(t) = DispatcherTimer::new(POLL_INTERVAL, move || {
                if let Ok(slot) = DRAWER_SLOT.lock()
                    && let Some(slot_req) = slot.as_ref()
                {
                    let slot_req = slot_req.clone();
                    drop(slot);
                    set_req.call(Some(slot_req));
                    set_open.call(true);
                }
            }) {
                *timer.borrow_mut() = Some(t);
            }
        }
    });

    // 子代理面板数据流：打开 Subagent 时启动轮询（立即拉一次 + 每 500ms 刷新），
    // 换 Diff/关闭时停止并清空缓存（数据移出渲染内存）。
    cx.use_effect((req.clone(), open), {
        let bridge = bridge.clone();
        let set_sub_turns = set_sub_turns.clone();
        let sub_timer = sub_timer.clone();
        let active_sub_seed = active_sub_seed.clone();
        let req = req.clone();
        move || {
            let is_subagent = matches!(&req, Some(DrawerRequest::Subagent { .. }));
            if is_subagent && open {
                if let Some(DrawerRequest::Subagent { seed, .. }) = &req {
                    bridge.core().spawn_fetch_subagent_timeline(seed);
                    *active_sub_seed.borrow_mut() = Some(seed.clone());
                }
                if sub_timer.borrow().is_none() {
                    if let Ok(t) = DispatcherTimer::new(SUBAGENT_POLL_INTERVAL, {
                        let bridge = bridge.clone();
                        let set_sub_turns = set_sub_turns.clone();
                        let active_sub_seed = active_sub_seed.clone();
                        move || {
                            // 只消费目标子代理的快照（seed 匹配防串台）。
                            if let Some((cached_seed, snap)) = bridge.core().subagent_timeline_peek()
                                && active_sub_seed.borrow().as_deref() == Some(cached_seed.as_str())
                            {
                                set_sub_turns.call(snap.turns.clone());
                            }
                            if let Some(seed) = active_sub_seed.borrow().clone() {
                                bridge.core().spawn_fetch_subagent_timeline(&seed);
                            }
                        }
                    }) {
                        *sub_timer.borrow_mut() = Some(t);
                    }
                }
            } else {
                // 关闭 / 切换到 Diff：停止轮询 + 清空缓存与渲染数据。
                if let Some(t) = sub_timer.borrow_mut().take() {
                    let _ = t.stop();
                }
                *active_sub_seed.borrow_mut() = None;
                bridge.core().subagent_timeline_consume();
                set_sub_turns.call(Vec::new());
            }
        }
    });

    // 弹层尺寸：主窗口 70%（覆盖层 grid 无 ContentDialog 模板钳制，规则完整生效）。
    let drawer_w = (win.width * DRAWER_RATIO).clamp(DRAWER_W_MIN, DRAWER_W_MAX);
    let drawer_h = (win.height * DRAWER_RATIO).clamp(DRAWER_H_MIN, DRAWER_H_MAX);

    // 关闭（✕ / Esc 共用）：清 open + 清静态槽（防轮询重开）。弃用
    // ContentDialog 后无生命周期竞态——轮询的 set_open(true) 幂等（相同值
    // 短路），不存在「关闭动画中重开 → 遮罩卡死」路径。
    // 注：`Callback<()>` 宏展开为 `Fn(())`（带一个 () 参数），闭包须为
    // `move |_: ()|`；Esc 处再包装成零参数闭包供 KeyboardAccelerator 使用。
    let on_close: Callback<()> = Callback::new({
        let set_open = set_open.clone();
        let bridge = bridge.clone();
        let set_sub_turns = set_sub_turns.clone();
        let sub_timer = sub_timer.clone();
        let active_sub_seed = active_sub_seed.clone();
        move |_: ()| {
            set_open.call(false);
            if let Ok(mut slot) = DRAWER_SLOT.lock() {
                *slot = None;
            }
            // 子代理数据随关闭释放（停轮询 + 清缓存 + 清渲染行）。
            if let Some(t) = sub_timer.borrow_mut().take() {
                let _ = t.stop();
            }
            *active_sub_seed.borrow_mut() = None;
            bridge.core().subagent_timeline_consume();
            set_sub_turns.call(Vec::new());
        }
    });

    // ⚠ hooks 全部在条件分支之前（React 规则：提前 return 时顺序不变）。
    let Some(req) = req else {
        return grid(()).into();
    };
    if !open {
        return grid(()).into();
    }

    let content: Element = match &req {
        DrawerRequest::Diff { turn_id, files } => memo(
            drawer_content,
            DrawerContentProps {
                turn_id: turn_id.clone(),
                files: files.clone(),
                selected,
                drawer_w,
                drawer_h,
                on_select: set_selected.clone(),
                on_close: on_close.clone(),
            },
        )
        .into(),
        DrawerRequest::Subagent { seed, name } => memo(
            subagent_content,
            SubagentContentProps {
                seed: seed.clone(),
                name: name.clone(),
                turns: sub_turns.clone(),
                drawer_w,
                drawer_h,
                on_close: on_close.clone(),
            },
        )
        .into(),
    };

    let esc = KeyboardAccelerator::new(
        VirtualKey::Escape,
        VirtualKeyModifiers::None,
        {
            let on_close = on_close.clone();
            move || on_close.invoke(())
        },
    );
    let card: Element = border(content)
        .background(ThemeRef::SolidBackground)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(8.0)
        .keyboard_accelerator(esc)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .with_key("diff-drawer-card")
        .into();

    grid((card,))
        .rows([GridLength::STAR])
        .columns([GridLength::STAR])
        // 半透明遮罩：拦截主界面输入（模态）；点击遮罩不关闭（ContentDialog 语义）。
        .background(Color {
            a: SCRIM_ALPHA,
            r: 0,
            g: 0,
            b: 0,
        })
        .with_key("diff-drawer-overlay")
        .into()
}

/// 弹层内容 memo props。
///
/// ⚠ PartialEq **忽略回调字段**：`SetState`/`Callback` 是稳定 Rc 引用，内容
/// 依赖项只有 turn_id/files/selected/drawer_w/drawer_h——选中切换才重建。
#[derive(Clone)]
struct DrawerContentProps {
    turn_id: String,
    files: Vec<DrawerFile>,
    selected: usize,
    drawer_w: f64,
    drawer_h: f64,
    on_select: SetState<usize>,
    on_close: Callback<()>,
}

impl PartialEq for DrawerContentProps {
    fn eq(&self, other: &Self) -> bool {
        self.turn_id == other.turn_id
            && self.files == other.files
            && self.selected == other.selected
            && self.drawer_w == other.drawer_w
            && self.drawer_h == other.drawer_h
    }
}

/// 弹层内容：头部（标题+合计+✕）+ 主体（左文件列表 / 右单列 diff）+ 底部合计。
///
/// 根 grid 固定 Width/Height（= 窗口 70%）：覆盖层内 STAR 行有明确约束，
/// diff 滚动区占满剩余高度（不再有 ContentDialog Auto 区退化问题）。
fn drawer_content(props: &DrawerContentProps, _cx: &mut RenderCx) -> Element {
    // 文件列表（左列）：状态 + 路径 + ±N；选中高亮。
    let file_rows: Vec<Element> = props
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (marker, fg) = if f.failed {
                ("✕", ThemeRef::SystemCritical)
            } else {
                ("✓", ThemeRef::SystemSuccess)
            };
            let is_selected = i == props.selected;
            let row = border(
                hstack((
                    text_block(marker).font_size(12.0).foreground(fg),
                    text_block(&f.path)
                        .font_size(12.0)
                        .text_trimming(TextTrimming::CharacterEllipsis)
                        .foreground(if is_selected {
                            ThemeRef::AccentText
                        } else {
                            ThemeRef::PrimaryText
                        })
                        .max_width(props.drawer_w - FILE_LIST_WIDTH - 96.0),
                    text_block(format!("+{} −{}", f.added, f.removed))
                        .font_size(11.0)
                        .foreground(ThemeRef::SecondaryText),
                ))
                .spacing(8.0)
                .padding(Thickness::xy(10.0, 7.0)),
            )
            .background(if is_selected {
                ThemeRef::AccentSecondary
            } else {
                ThemeRef::LayerFill
            })
            .corner_radius(4.0)
            .on_tapped({
                let on_select = props.on_select.clone();
                move || on_select.call(i)
            })
            .with_key(format!("drawer-file-{i}"))
            .into();
            row
        })
        .collect();
    let total_added: usize = props.files.iter().map(|f| f.added).sum();
    let total_removed: usize = props.files.iter().map(|f| f.removed).sum();

    // 右列：选中文件的单列 unified diff。
    let current = props.files.get(props.selected).cloned();
    let diff_panel: Element = match &current {
        Some(f) if !f.file.rows.is_empty() => diff_file_view(
            &f.file,
            "ms-appx:///Assets/fonts/CascadiaCode.ttf#Cascadia Code",
            &format!("drawer-diff-{}-{}", props.turn_id, f.path),
        ),
        _ => text_block(if current.as_ref().map(|f| f.failed).unwrap_or(false) {
            "该文件无 diff（工具执行失败）"
        } else {
            "无 diff 数据"
        })
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText)
        .into(),
    };

    grid((
        // 头部
        hstack((
            text_block("Diff 详情").font_size(14.0).semibold(),
            text_block(format!(
                "{} 个文件  +{}  −{}",
                props.files.len(),
                total_added,
                total_removed
            ))
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
            button("✕ 关闭")
                .on_click(props.on_close.clone())
                .horizontal_alignment(HorizontalAlignment::Right),
        ))
        .spacing(12.0)
        .padding(Thickness::xy(16.0, 12.0))
        .grid_row(0),
        // 主体：左文件列表 + 右 diff
        grid((
            border(
                vstack(file_rows)
                    .spacing(2.0)
                    .padding(Thickness::xy(10.0, 8.0)),
            )
            .background(ThemeRef::LayerFill)
            .grid_column(0),
            border(diff_panel)
                .padding(Thickness::xy(16.0, 8.0))
                .grid_column(1),
        ))
        .columns([GridLength::Pixel(FILE_LIST_WIDTH), GridLength::STAR])
        .rows([GridLength::STAR])
        .grid_row(1),
        // 底部：合计
        hstack((
            text_block(format!("合计  +{}  −{}", total_added, total_removed))
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            text_block("Esc 或 ✕ 关闭")
                .font_size(11.0)
                .foreground(ThemeRef::TertiaryText)
                .horizontal_alignment(HorizontalAlignment::Right),
        ))
        .spacing(12.0)
        .padding(Thickness::xy(16.0, 10.0))
        .grid_row(2),
    ))
    .rows([GridLength::Auto, GridLength::STAR, GridLength::Auto])
    .width(props.drawer_w)
    .height(props.drawer_h)
    .into()
}

// ── 子代理运行记录面板 ───────────────────────────────────────────

/// 子代理内容 memo props（PartialEq 忽略回调字段，同 DrawerContentProps 约定）。
#[derive(Clone)]
struct SubagentContentProps {
    seed: String,
    name: String,
    turns: Vec<TimelineTurn>,
    drawer_w: f64,
    drawer_h: f64,
    on_close: Callback<()>,
}

impl PartialEq for SubagentContentProps {
    fn eq(&self, other: &Self) -> bool {
        self.seed == other.seed
            && self.name == other.name
            && self.turns == other.turns
            && self.drawer_w == other.drawer_w
            && self.drawer_h == other.drawer_h
    }
}

/// 子代理面板内容：头部（名称 + 状态 + ✕）+ 滚动区（回合行视图）。
/// 行视图按 turn 渲染：任务行（user_text）→ 各 round 的文本/思考/工具行。
/// 按行渲染即可（子代理无需 token 级实时），数据随面板关闭释放。
fn subagent_content(props: &SubagentContentProps, _cx: &mut RenderCx) -> Element {
    let rows: Vec<Element> = props
        .turns
        .iter()
        .flat_map(|turn| {
            let mut rows: Vec<Element> = Vec::new();
            // 任务行：turn 的 user_text（首行截断）。
            let task = turn.user_text.lines().next().unwrap_or("").trim();
            let task_label = if task.is_empty() {
                format!("回合 {}", turn.turn_id)
            } else {
                format!("任务: {task}")
            };
            rows.push(
                text_block(task_label)
                    .font_size(12.0)
                    .semibold()
                    .wrap()
                    .foreground(ThemeRef::PrimaryText)
                    .padding(Thickness::xy(4.0, 6.0))
                    .with_key(format!("subagent-turn-{}", turn.turn_id))
                    .into(),
            );
            for round in &turn.rounds {
                for block in &round.blocks {
                    let line = block.text.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let (prefix, fg) = match block.kind {
                        TimelineBlockKind::Reasoning => ("思考", ThemeRef::SecondaryText),
                        TimelineBlockKind::Tool => ("工具", ThemeRef::AccentText),
                        _ => ("", ThemeRef::PrimaryText),
                    };
                    // 行截断：单行最长 ~800 字符（细节不值得滚动）。
                    let mut text = line.replace('\n', " ");
                    if text.chars().count() > 800 {
                        let cut: String = text.chars().take(800).collect();
                        text = format!("{cut}…");
                    }
                    let label = if prefix.is_empty() {
                        text
                    } else {
                        format!("[{prefix}] {text}")
                    };
                    rows.push(
                        text_block(label)
                            .font_size(11.5)
                            .wrap()
                            .foreground(fg)
                            .padding(Thickness::xy(4.0, 2.0))
                            .with_key(format!("subagent-block-{}-{}", turn.turn_id, block.block_id))
                            .into(),
                    );
                }
            }
            rows
        })
        .collect();

    let status = if props.turns.is_empty() {
        "等待子代理事件…".to_string()
    } else if props.turns.last().map(|t| t.sealed).unwrap_or(false) {
        "已完成".to_string()
    } else {
        "运行中…".to_string()
    };

    grid((
        hstack((
            text_block(format!("子代理 · {}", props.name)).font_size(14.0).semibold(),
            text_block(format!("{status} · {}", props.seed))
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            button("✕ 关闭")
                .on_click(props.on_close.clone())
                .horizontal_alignment(HorizontalAlignment::Right),
        ))
        .spacing(12.0)
        .padding(Thickness::xy(16.0, 12.0))
        .grid_row(0),
        border(scroll_viewer(vstack(rows).spacing(2.0).padding(Thickness::xy(12.0, 8.0))))
            .background(ThemeRef::LayerFill)
            .grid_row(1),
    ))
    .rows([GridLength::Auto, GridLength::STAR])
    .width(props.drawer_w)
    .height(props.drawer_h)
    .into()
}
