//! XAML 原生交互模态覆盖层（交互迁移块第一块：permission + ask）
//! — Web `PermissionPrompt` / `AskUserPrompt` 的壳侧承载。
//!
//! 布局（P-6 覆盖层模式，同 splash：`kind="none"` 时空 grid 无背景 →
//! 点击穿透；有交互时半透明遮罩拦截命中）：
//! ```text
//! ┌ 全窗遮罩（半透明黑）──────────────────────────────────────────┐
//! │  ┌ 卡片（LayerFill + 圆角 8px，420px，垂直居中）─────────────┐  │
//! │  │ 需要授权 / 问题                                 (eyebrow) │  │
//! │  │ 工具名                            [category · risk]      │  │
//! │  │ 原因 / 后果 / 路径列表（mono pill）                        │  │
//! │  │ [信任此目录]（high + paths 时）                            │  │
//! │  │                                     [拒绝] [批准并执行]    │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! └────────────────────────────────────────────────────────────────┘
//! ```
//!
//! 数据源：`bridge.core().interaction_snapshot()`——Web `shell.setInteraction`
//! 投影（`kind` = "none" | "permission" | "ask"；"plan" 保持 Web 渲染，
//! 本覆盖层只接管前两种，flag 部分接管）。250ms rev 轮询（同 main.rs
//! view timer，交互响应优先于 500ms 侧栏节奏）。
//!
//! 动作：按钮/选项点击 emit `shell.interactionAction` 回传 Web 执行既有
//! handler（respondToPermission / submitAsk / dismissAsk）——协议请求
//! （interaction.*）仍在 Web 侧发起，状态单一数据源不变（对齐 D2 执行权
//! 原则：壳只渲染，不持有状态）。表单本地状态（trust / 选项选中 / 自定义
//! 输入）仅存在于壳渲染层，随投影 key（kind+id）变化重置。
//!
//! 复刻偏差（对齐项目既有偏差记录风格）：
//! - Web 内嵌面板（InteractionModal 嵌于流式区）→ 原生模态遮罩：权限/提问
//!   本就 gate composer（hasPendingGate），模态化不改变语义，且安全敏感
//!   交互更聚焦；
//! - Web approval 按钮：low/medium 反色实心、high 红色实心 → Fluent accent
//!   统一按钮 + risk 徽标语义色（SystemCritical/Caution/Success）保留风险
//!   层级信息；
//! - ask 选项 Web 按钮组 → RadioButton（单选语义一致）；自定义输入
//!   TextBox 同步保留（allow_custom / 无选项时）；
//! - 无 hover/输入补间动画（Fluent 2 即时切换规范，同 info_panel）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use windows_reactor::*;

use crate::bridge::{AskAnswer, Bridge, InteractionState};

/// 快照轮询间隔（交互响应优先；同 main.rs view timer）。
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// 模态卡片宽度（对齐 Web `.interaction-modal-card` 语义宽度）。
const CARD_WIDTH: f64 = 420.0;
/// 等宽字体（路径列表；同 info_panel `MONO_FONT`）。
const MONO_FONT: &str = deepx_fluent::tokens::CODE_FONT_FAMILY;
/// 遮罩透明度（半透明黑 scrim，拦截命中 + 保留上下文可见）。
const SCRIM_ALPHA: u8 = 120;

/// 诊断日志（同 main.rs log_diag 约定：GUI 子系统无控制台，写文件）。
fn log_diag(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var("DEEPX_WINUI_LOG").unwrap_or_else(|_| ".deepx-winui.log".into()))
    {
        let _ = writeln!(f, "[interaction_overlay] {msg}");
    }
}

/// 小标题（eyebrow：11px 600 muted，同 info_panel `section_heading`）。
fn eyebrow(text: &str) -> Element {
    text_block(text)
        .font_size(11.0)
        .semibold()
        .foreground(ThemeRef::SecondaryText)
        .into()
}

/// risk → Fluent 语义色（Web `approval-*` 色系的语义等价，徽标用）。
fn risk_color(risk: &str) -> ThemeRef {
    match risk {
        "high" => ThemeRef::SystemCritical,
        "medium" => ThemeRef::SystemCaution,
        _ => ThemeRef::SystemSuccess,
    }
}

/// 批准按钮文案（对齐 Web `approvalLabel`：high+exec → 批准并执行等）。
fn approval_label(risk: &str, category: &str) -> &'static str {
    if risk == "high" {
        if category == "exec" {
            return "批准并执行";
        }
        return "批准并继续";
    }
    "批准"
}

/// XAML 交互模态覆盖层（追加进 main.rs 覆盖层 grid，与 splash 同 cell）。
pub fn interaction_overlay(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (state, set_state) = cx.use_state::<InteractionState>(InteractionState::default());
    // HookRef 修改不触发重渲染（SetState 同值短路，engine.rs make_state_setter）：
    // 表单回调（选项选中 / 自定义输入 / checkbox）改 HookRef 后必须 bump 递增强制
    // 重渲染，使 all_answered / approve_label 等派生值重算。
    // （此前无此机制：轮询只在快照 rev 变化时 set_state，用户交互不改变快照 →
    //  确认按钮永远灰 / 批准文案不更新——占位实现的遗留 bug。）
    let (_, bump) = cx.use_reducer::<u64>(0);
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    // ask 本地表单：选项选中 / 自定义输入（qid → 值）。仅壳渲染层持有。
    let selected = cx.use_ref::<HashMap<String, String>>(HashMap::new());
    let custom = cx.use_ref::<HashMap<String, String>>(HashMap::new());
    // ask 横向切换：当前题索引（多题时单题视图 + 上一题/下一题导航）。
    let cur_q = cx.use_ref::<usize>(0);
    // permission 本地表单：信任此目录。
    let trust = cx.use_ref::<bool>(false);
    // plan 本地表单：拒绝理由 / 目标模式。
    let feedback = cx.use_ref::<String>(String::new());
    let autonomous = cx.use_ref::<bool>(false);
    // 投影 key（kind:id）变化时重置本地表单（新交互到来清残留）。
    let last_key = cx.use_ref::<String>(String::new());

    // 250ms rev 轮询（shell::poll_rev helper，同 header.rs 模式）。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_state = set_state.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        move || {
            crate::shell::poll_rev(
                timer,
                last_rev,
                POLL_INTERVAL,
                move || bridge.core().interaction_snapshot(),
                move |s| set_state.call(s),
            );
        }
    });

    // 投影 key 变化 → 重置本地表单。首次挂载也执行（清空即 no-op）。
    // deps 与闭包都捕获 clone 值，避免 move 闭包拿走 state 所有权。
    let kind_key = state.kind.clone();
    let id_key = state.id.clone();
    cx.use_effect((kind_key.clone(), id_key.clone()), {
        let selected = selected.clone();
        let custom = custom.clone();
        let trust = trust.clone();
        let feedback = feedback.clone();
        let autonomous = autonomous.clone();
        let last_key = last_key.clone();
        let cur_q = cur_q.clone();
        move || {
            let key = format!("{kind_key}:{id_key}");
            if key != *last_key.borrow() {
                *last_key.borrow_mut() = key.clone();
                selected.borrow_mut().clear();
                custom.borrow_mut().clear();
                *cur_q.borrow_mut() = 0;
                *trust.borrow_mut() = false;
                *feedback.borrow_mut() = String::new();
                *autonomous.borrow_mut() = false;
                log_diag(&format!("interaction key -> {key}"));
            }
        }
    });

    // 无交互：空 grid（无背景 → 不参与命中测试，点击穿透，同 splash）。
    if state.kind == "none" || state.kind.is_empty() {
        return grid(()).into();
    }

    // ── 交互回调（普通闭包 + 捕获 id；投影 key 变化后按钮才可再次触发，
    //    重复点击由 Web 侧 beginInteractionSubmit gate 兜底）──────────
    let on_approve = {
        let bridge = bridge.clone();
        let trust = trust.clone();
        let id = state.id.clone();
        move || {
            let seed = bridge.core().active_seed();
            bridge.spawn_interaction_response(
                "interaction.permission",
                serde_json::json!({
                    "seed": seed,
                    "toolCallId": id.clone(),
                    "approved": true,
                    "trustFolder": *trust.borrow(),
                }),
            );
        }
    };
    let on_reject = {
        let bridge = bridge.clone();
        let id = state.id.clone();
        move || {
            let seed = bridge.core().active_seed();
            bridge.spawn_interaction_response(
                "interaction.permission",
                serde_json::json!({
                    "seed": seed,
                    "toolCallId": id.clone(),
                    "approved": false,
                    "trustFolder": false,
                }),
            );
        }
    };
    let on_ask_dismiss = {
        let bridge = bridge.clone();
        let id = state.id.clone();
        move || {
            let seed = bridge.core().active_seed();
            bridge.spawn_interaction_response(
                "interaction.ask_dismiss",
                serde_json::json!({ "seed": seed, "askId": id.clone() }),
            );
        }
    };
    // 提交答案：custom 非空优先，否则选中选项（对齐 Web handleSubmit）。
    let on_ask_submit = {
        let bridge = bridge.clone();
        let id = state.id.clone();
        let questions = state.questions.clone();
        let selected = selected.clone();
        let custom = custom.clone();
        move || {
            let mut answers: Vec<AskAnswer> = Vec::new();
            for q in &questions {
                let c = custom.borrow().get(&q.id).cloned().unwrap_or_default();
                let answer = if c.trim().is_empty() {
                    selected.borrow().get(&q.id).cloned().unwrap_or_default()
                } else {
                    c
                };
                if !answer.trim().is_empty() {
                    answers.push(AskAnswer {
                        question_id: q.id.clone(),
                        answer,
                    });
                }
            }
            let seed = bridge.core().active_seed();
            bridge.spawn_interaction_response(
                "interaction.ask_response",
                serde_json::json!({ "seed": seed, "askId": id.clone(), "answers": answers }),
            );
        }
    };

    // ── plan 审批：拒绝（带理由）/ 批准（可按目标模式）─────────────
    let on_plan_approve = {
        let bridge = bridge.clone();
        let autonomous = autonomous.clone();
        let id = state.id.clone();
        move || {
            let seed = bridge.core().active_seed();
            bridge.spawn_interaction_response(
                "interaction.plan_review",
                serde_json::json!({
                    "seed": seed,
                    "callId": id.clone(),
                    "approved": true,
                    "message": null,
                    "autonomous": *autonomous.borrow(),
                }),
            );
        }
    };
    let on_plan_reject = {
        let bridge = bridge.clone();
        let feedback = feedback.clone();
        let id = state.id.clone();
        move || {
            let msg = feedback.borrow().trim().to_string();
            let seed = bridge.core().active_seed();
            bridge.spawn_interaction_response(
                "interaction.plan_review",
                serde_json::json!({
                    "seed": seed,
                    "callId": id.clone(),
                    "approved": false,
                    "message": (!msg.is_empty()).then_some(msg),
                    "autonomous": false,
                }),
            );
        }
    };

    // ── 卡片内容（permission / ask / plan 三模板，统一交互弹窗体系）──
    let body: Element = if state.kind == "permission" {
        permission_body(&state, trust, on_approve, on_reject)
    } else if state.kind == "plan" {
        plan_body(
            &state,
            feedback,
            autonomous,
            &bump,
            on_plan_approve,
            on_plan_reject,
        )
    } else {
        ask_body(&state, selected, custom, &bump, &cur_q, on_ask_submit, on_ask_dismiss)
    };

    // 卡片实底（SolidBackground 100% 不透明 + CardStroke 描边）：CardBackground
    // 约 90% 不透明会让底层文字透出形成"重叠"观感（demo 实证）；模态弹窗
    // 需要清晰可读 → 全实底。reactor 无元素级 Acrylic，虚化不可行（窗口级
    // backdrop=Mica 已启用），scrim 变暗承担背景降噪。
    let card: Element = border(vstack((body,)).spacing(12.0).padding(20.0))
        .corner_radius(8.0)
        .background(ThemeRef::SolidBackground)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .width(CARD_WIDTH)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into();

    // 模态遮罩：显式 STAR 行/列让 scrim 全窗铺满（Grid 默认 Auto 行高会让
    // Stretch 子元素只覆盖内容大小——"遮罩不全窗"的根因，demo 已实证）。
    // 有背景 → 拦截下方基础层命中（与空 grid 穿透互补）。
    grid((card,))
        .rows([GridLength::STAR])
        .columns([GridLength::STAR])
        .background(Color {
            a: SCRIM_ALPHA,
            r: 0,
            g: 0,
            b: 0,
        })
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .vertical_alignment(VerticalAlignment::Stretch)
        .into()
}

/// permission 模板：需要授权 / 工具名 / category · risk / 原因 / 后果 /
/// 路径列表 / [信任此目录] / [拒绝] [批准…]。
fn permission_body(
    state: &InteractionState,
    trust: HookRef<bool>,
    on_approve: impl Fn() + 'static,
    on_reject: impl Fn() + 'static,
) -> Element {
    let risk = risk_color(&state.risk);
    let meta: Element = hstack((
        text_block(&state.category)
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
        text_block(" · ")
            .font_size(12.0)
            .foreground(ThemeRef::TertiaryText),
        text_block(&state.risk)
            .font_size(12.0)
            .semibold()
            .foreground(risk),
    ))
    .spacing(4.0)
    .into();

    // 路径列表：mono pill（对齐 Web `code` 的等宽语义）。
    let mut rows: Vec<Element> = Vec::new();
    for (i, p) in state.paths.iter().enumerate() {
        let pill: Element = border(
            text_block(format!("▸ {p}"))
                .font_size(12.0)
                .font_family(MONO_FONT)
                .foreground(ThemeRef::SecondaryText),
        )
        .background(ThemeRef::ControlFillSecondary)
        .corner_radius(4.0)
        .padding(4.0)
        .into();
        rows.push(pill.with_key(format!("path-{i}")));
    }
    let paths: Element = if rows.is_empty() {
        grid(()).into()
    } else {
        vstack(rows).spacing(4.0).into()
    };

    // 信任此目录（high risk 且涉及路径时，对齐 Web PermissionPrompt）。
    let trust_row: Element = if state.risk == "high" && !state.paths.is_empty() {
        check_box(false)
            .content("信任此目录")
            .on_checked(move |v: bool| *trust.borrow_mut() = v)
            .into()
    } else {
        grid(()).into()
    };

    vstack((
        eyebrow("需要授权"),
        text_block(&state.tool_name).font_size(16.0).semibold(),
        meta,
        text_block(&state.reason).font_size(13.0).wrap(),
        text_block(&state.consequence)
            .font_size(13.0)
            .foreground(ThemeRef::SecondaryText)
            .wrap(),
        paths,
        trust_row,
        // 动作行右对齐：占位 spacer（Star 列）推按钮到右侧。
        grid((
            grid(()).grid_column(0),
            hstack((
                button("拒绝").subtle().on_click(on_reject),
                button(approval_label(&state.risk, &state.category))
                    .accent()
                    .on_click(on_approve),
            ))
            .spacing(8.0)
            .grid_column(1),
        ))
        .columns([GridLength::STAR, GridLength::Auto]),
    ))
    .spacing(12.0)
    .into()
}

/// ask 模板：单题视图（多题时上一题/下一题横向切换）+ 选项 RadioButton 组
/// + 自定义输入 TextBox + [跳过] [确认]。
///
/// 多题时一次只渲染当前题（cur_q 索引），导航行 [上一题] 第 x/y 题 [下一题]
/// 切换；已答内容保留在 selected/custom map 中，切回时从 map 恢复。
/// 确认按钮使能 = 全部题目已答（对齐 Web allAnswered，跨题计算）。
fn ask_body(
    state: &InteractionState,
    selected: HookRef<HashMap<String, String>>,
    custom: HookRef<HashMap<String, String>>,
    bump: &Updater<u64>,
    cur_q: &HookRef<usize>,
    on_submit: impl Fn() + 'static,
    on_dismiss: impl Fn() + 'static,
) -> Element {
    let total = state.questions.len();
    // 当前题索引（越界保护）。HookRef 修改不触发重渲染：导航回调里 bump。
    let cur = (*cur_q.borrow()).min(total.saturating_sub(1));
    let q = &state.questions[cur];

    // 当前题选项：RadioButton 组（group = qid；选中 → 清自定义输入）。
    // 切换题目后切回，checked 从 selected map 恢复（map 跨题保留）。
    let mut option_els: Vec<Element> = Vec::new();
    for (j, opt) in q.options.iter().enumerate() {
        let opt = opt.clone();
        let qid = q.id.clone();
        let checked = selected.borrow().get(&q.id) == Some(&opt);
        let radio = RadioButton::new(opt.clone())
            .group(q.id.clone())
            .checked(checked)
            .on_checked({
                let selected = selected.clone();
                let custom = custom.clone();
                let bump = bump.clone();
                let qid = qid.clone();
                move || {
                    selected.borrow_mut().insert(qid.clone(), opt.clone());
                    custom.borrow_mut().remove(&qid);
                    // HookRef 修改不触发重渲染：bump 递增强制重渲染
                    // （all_answered 重算 → 确认按钮使能）。
                    bump.call(|n| n.wrapping_add(1));
                }
            })
            .with_key(format!("opt-{}-{j}", q.id));
        option_els.push(radio.into());
    }
    let options: Element = if option_els.is_empty() {
        grid(()).into()
    } else {
        vstack(option_els).spacing(2.0).into()
    };

    // 当前题自定义输入（无选项或 allow_custom 时；输入 → 清选项选中）。
    let input: Element = if q.options.is_empty() || q.allow_custom {
        let qid = q.id.clone();
        text_box(custom.borrow().get(&q.id).cloned().unwrap_or_default())
            .placeholder_text("输入自定义答案...")
            .on_text_changed({
                let selected = selected.clone();
                let custom = custom.clone();
                let bump = bump.clone();
                let qid = qid.clone();
                move |t: String| {
                    if t.trim().is_empty() {
                        custom.borrow_mut().remove(&qid);
                    } else {
                        custom.borrow_mut().insert(qid.clone(), t);
                        selected.borrow_mut().remove(&qid);
                    }
                    bump.call(|n| n.wrapping_add(1));
                }
            })
            .with_key(format!("input-{}", q.id))
            .into()
    } else {
        grid(()).into()
    };

    let title = if total > 1 {
        format!("{}. {}", cur + 1, q.question)
    } else {
        q.question.clone()
    };

    // 确认按钮使能 = 全部问题已答（对齐 Web allAnswered）。
    // 工具语义（normalize_ask_user）：每问必答——options 空 + allow_custom
    // （纯自定义）同样必答；options 空 + 禁自定义会被 UNANSWERABLE_QUESTION
    // 拒绝，不会到达 UI。
    let all_answered = state.questions.iter().all(|q| {
        let c = custom.borrow().get(&q.id).cloned().unwrap_or_default();
        !c.trim().is_empty() || selected.borrow().get(&q.id).is_some()
    });
    let answered_count = state
        .questions
        .iter()
        .filter(|q| {
            let c = custom.borrow().get(&q.id).cloned().unwrap_or_default();
            !c.trim().is_empty() || selected.borrow().get(&q.id).is_some()
        })
        .count();

    // 导航行（仅多题时）：对称布局——[上一题] 贴左、第 x/y 题居中、[下一题]
    // 贴右（Grid 三列 Auto/STAR/Auto，STAR 列把两端按钮顶到边缘）。
    // 首题"上一题"灰、末题"下一题"灰；切换只改 cur_q + bump，
    // 已答内容保留在 map 中，切回时从 map 恢复。
    let nav: Element = if total > 1 {
        grid((
            button("上一题")
                .subtle()
                .enabled(cur > 0)
                .on_click({
                    let cur_q = cur_q.clone();
                    let bump = bump.clone();
                    move || {
                        let next = cur_q.borrow().saturating_sub(1);
                        *cur_q.borrow_mut() = next;
                        bump.call(|n| n.wrapping_add(1));
                    }
                })
                .grid_column(0),
            text_block(format!("第 {}/{} 题", cur + 1, total))
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Center)
                .grid_column(1),
            button("下一题")
                .subtle()
                .enabled(cur + 1 < total)
                .on_click({
                    let cur_q = cur_q.clone();
                    let bump = bump.clone();
                    move || {
                        let next = (*cur_q.borrow() + 1).min(total - 1);
                        *cur_q.borrow_mut() = next;
                        bump.call(|n| n.wrapping_add(1));
                    }
                })
                .grid_column(2),
        ))
        .columns([GridLength::Auto, GridLength::STAR, GridLength::Auto])
        .into()
    } else {
        grid(()).into()
    };

    vstack((
        eyebrow("问题"),
        text_block(title).font_size(13.0).semibold().wrap(),
        options,
        input,
        nav,
        // 已答进度提示：实时反映 bump 重渲染；未答完时说明确认按钮为何灰。
        text_block(format!(
            "已答 {answered_count}/{total} 题，全部回答后可确认"
        ))
        .font_size(11.0)
        .foreground(ThemeRef::SecondaryText),
        grid((
            grid(()).grid_column(0),
            hstack((
                button("跳过").subtle().on_click(on_dismiss),
                button("确认")
                    .accent()
                    .enabled(all_answered)
                    .on_click(on_submit),
            ))
            .spacing(8.0)
            .grid_column(1),
        ))
        .columns([GridLength::STAR, GridLength::Auto]),
    ))
    .spacing(12.0)
    .into()
}

/// complexity 徽标语义色（对齐 Web badge-small/medium/large 的近似；偏差记录）。
fn complexity_color(c: &str) -> ThemeRef {
    match c {
        "small" => ThemeRef::SystemSuccess,
        "large" => ThemeRef::SystemCritical,
        _ => ThemeRef::SystemCaution,
    }
}

/// plan 模板：内容（todo 列表 / mono pre）+ 拒绝理由输入 + 目标模式 +
/// [拒绝] [批准…]（对齐 Web `PlanReviewPanel`）。
fn plan_body(
    state: &InteractionState,
    feedback: HookRef<String>,
    autonomous: HookRef<bool>,
    bump: &Updater<u64>,
    on_approve: impl Fn() + 'static,
    on_reject: impl Fn() + 'static,
) -> Element {
    let is_todo = state.review_type == "todo_activation";
    let (eyebrow_text, title, desc) = if is_todo {
        (
            "Goal 激活审核",
            "确认激活目标模式",
            "模型请求激活目标模式，将按复杂度顺序（小→中→大）自动执行以下任务。",
        )
    } else {
        (
            "计划审核",
            "确认执行计划",
            "审阅计划内容后批准执行，或留下拒绝原因。",
        )
    };

    // todo 列表（todo_activation 且有项；id + title + complexity 徽标 + desc）。
    let mut todo_rows: Vec<Element> = Vec::new();
    for (i, item) in state.todo_items.iter().enumerate() {
        let head: Element = hstack((
            text_block(&item.id)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            text_block(&item.title).font_size(13.0).semibold().wrap(),
            text_block(&item.complexity)
                .font_size(11.0)
                .foreground(complexity_color(&item.complexity)),
        ))
        .spacing(8.0)
        .into();
        let full: Element = vstack((
            head,
            if item.description.is_empty() {
                let empty: Element = grid(()).into();
                empty
            } else {
                text_block(&item.description)
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText)
                    .wrap()
                    .into()
            },
        ))
        .spacing(2.0)
        .into();
        todo_rows.push(full.with_key(format!("todo-{i}-{}", item.id)));
    }
    let todo_list: Element = if todo_rows.is_empty() {
        if is_todo {
            text_block("没有可执行的任务。")
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .into()
        } else {
            grid(()).into()
        }
    } else {
        vstack(todo_rows).spacing(6.0).into()
    };

    // 计划内容（非 todo_activation 时 mono pre，限高滚动对齐 Web <pre>）。
    let plan_content: Element = if !is_todo && !state.plan_content.is_empty() {
        border(
            scroll_viewer(
                text_block(&state.plan_content)
                    .font_family(MONO_FONT)
                    .font_size(12.0)
                    .wrap(),
            )
            .height(120.0),
        )
        .background(ThemeRef::ControlFillSecondary)
        .corner_radius(4.0)
        .padding(8.0)
        .into()
    } else {
        grid(()).into()
    };

    // 拒绝理由输入（多行 TextBox；输入即存草稿，拒绝时随 action 回传）。
    let feedback_box: Element = text_box(feedback.borrow().clone())
        .accepts_return(true)
        .placeholder_text(if is_todo {
            "拒绝原因（可选）"
        } else {
            "拒绝原因或修改意见（拒绝时可选）"
        })
        .height(56.0)
        .on_text_changed({
            let feedback = feedback.clone();
            let bump = bump.clone();
            move |t: String| {
                *feedback.borrow_mut() = t;
                bump.call(|n| n.wrapping_add(1));
            }
        })
        .into();

    // 目标模式（非 todo_activation 时；对齐 Web autonomous checkbox）。
    let auto_row: Element = if !is_todo {
        let auto_for_cb = autonomous.clone();
        let bump_for_cb = bump.clone();
        check_box(*autonomous.borrow())
            .content("以目标模式执行")
            .on_checked(move |v: bool| {
                *auto_for_cb.borrow_mut() = v;
                // 批准按钮文案（"批准并启动目标模式"）依赖 autonomous：
                // 勾选后必须重渲染才能更新。
                bump_for_cb.call(|n| n.wrapping_add(1));
            })
            .into()
    } else {
        let empty: Element = grid(()).into();
        empty
    };

    // 按钮行（文案对齐 Web：todo → "批准并启动 Goal 模式"；否则按 autonomous）。
    let approve_label = if is_todo {
        "批准并启动 Goal 模式"
    } else if *autonomous.borrow() {
        "批准并启动目标模式"
    } else {
        "批准并继续"
    };
    let reject_label = if is_todo {
        "拒绝激活"
    } else {
        "拒绝计划"
    };

    vstack((
        eyebrow(eyebrow_text),
        text_block(title).font_size(16.0).semibold(),
        text_block(desc)
            .font_size(13.0)
            .foreground(ThemeRef::SecondaryText)
            .wrap(),
        todo_list,
        plan_content,
        feedback_box,
        auto_row,
        grid((
            grid(()).grid_column(0),
            hstack((
                button(reject_label).subtle().on_click(on_reject),
                button(approve_label).accent().on_click(on_approve),
            ))
            .spacing(8.0)
            .grid_column(1),
        ))
        .columns([GridLength::STAR, GridLength::Auto]),
    ))
    .spacing(12.0)
    .into()
}
