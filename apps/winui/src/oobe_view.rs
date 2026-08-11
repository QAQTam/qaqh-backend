//! OOBE（Out-of-Box Experience）——首次启动独立全屏三步引导。
//!
//! 触发：`main.rs` 启动时无本地完成标志（`%LOCALAPPDATA%\DeepX\oobe.done`）
//! 且 daemon 未配置主 API Key → 渲染本页盖住整个壳（P-6 覆盖层模式最上层）；
//! 完成或跳过都写标志（永久不弹），设置页可「重新运行引导」手动重开。
//!
//! 三步状态机：
//!   Step 0 欢迎    —— 品牌 + 定位 + [开始设置] [跳过]
//!   Step 1 连接    —— 提供方/端点联动 + 模型 + 主 API Key（唯一必填）
//!   Step 2 增强    —— 子代理 / 多模态 Key（可选，留空 = 暂不启用）
//!
//! 数据源与保存：与 settings_view 同模式——`spawn_config_load` 兜底 +
//! 500ms rev 轮询刷新草稿（`!dirty` 闸门）；保存走 `spawn_config_save`
//! （camelCase 字段，对齐 Web `save()` / apiKeyReplacement 语义）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;
use windows_reactor::*;

use deepx_fluent::{motion, tokens};

use crate::bridge::Bridge;
use crate::shell_store::SettingsSnapshot;

/// 完成标志文件名（目录：`%LOCALAPPDATA%\DeepX`）。
const OOBE_MARKER: &str = "oobe.done";
/// 快照轮询间隔（同 settings_view）。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 设置页「重新运行引导」请求（app() 轮询消费后复位）。
static RERUN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 设置页入口：请求重开引导（无需知道 app() 状态细节）。
pub fn request_rerun() {
    RERUN_REQUESTED.store(true, Ordering::Relaxed);
}

/// app() 轮询消费：取走重开请求。
pub fn take_rerun_request() -> bool {
    RERUN_REQUESTED.swap(false, Ordering::Relaxed)
}

/// 完成标志路径（`%LOCALAPPDATA%\DeepX\oobe.done`；无 LOCALAPPDATA 时落 cwd）。
fn marker_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("DeepX").join(OOBE_MARKER)
}

/// 引导是否已结束（完成或跳过——两者都永久不弹）。
pub fn oobe_done() -> bool {
    marker_path().exists()
}

/// 写完成标志（幂等；main.rs 自动收尾路径也调用）。
pub(crate) fn mark_oobe_done() {
    if let Some(dir) = marker_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(marker_path(), "done");
}

/// 步骤指示点（当前步高亮）。
fn step_dot(active: bool) -> Element {
    text_block(if active { "●" } else { "○" })
        .font_size(14.0)
        .foreground(if active {
            ThemeRef::AccentText
        } else {
            ThemeRef::SecondaryText
        })
        .into()
}

/// OOBE 主体（覆盖层组件；由 main.rs 按 `oobe_visible` 挂载/卸载）。
///
/// 完成/跳过统一走 `finish`：写标志 → 隐藏覆盖层 → navigate home（同时
/// 修复壳 `current_view` 与 UI 视图态不同步的问题——OOBE 直接落地 Home）。
pub fn oobe_view(cx: &mut RenderCx, set_visible: SetState<bool>) -> Element {
    let bridge = Bridge::shared();
    let (step, set_step) = cx.use_state::<u8>(0);
    // ── 草稿 + 轮询（同 settings_view：rev 变化且 !dirty 时刷新）────────
    let draft = cx.use_ref::<SettingsSnapshot>(SettingsSnapshot::default());
    let dirty = cx.use_ref::<bool>(false);
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    // 保存反馈（"已保存 ✓" / 错误文案）。
    let (saved_at, set_saved_at) = cx.use_state::<Option<std::time::Instant>>(None);
    let (save_error, set_save_error) = cx.use_state::<Option<String>>(None);

    cx.use_effect((), {
        let bridge = bridge.clone();
        let draft = draft.clone();
        let dirty = dirty.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        move || {
            // 首次挂载：兜底拉权威快照。
            bridge.spawn_config_load(false);
            let (_, rev) = bridge.core().settings_snapshot();
            *last_rev.borrow_mut() = rev;
            if let Ok(t) = DispatcherTimer::new(POLL_INTERVAL, {
                let core = bridge.core();
                let draft = draft.clone();
                let dirty = dirty.clone();
                let last_rev = last_rev.clone();
                move || {
                    let (snap, srev) = core.settings_snapshot();
                    if srev != *last_rev.borrow() {
                        *last_rev.borrow_mut() = srev;
                        if let Some(snap) = &snap {
                            if !*dirty.borrow() {
                                *draft.borrow_mut() = snap.clone();
                            }
                        }
                    }
                }
            }) {
                *timer.borrow_mut() = Some(t);
            }
        }
    });

    let d = draft.borrow().clone();

    // ── 结束引导（完成 / 跳过共用）：写标志 → 隐藏 → 落地 Home ────────
    let finish = {
        let set_visible = set_visible.clone();
        let bridge = bridge.clone();
        move || {
            mark_oobe_done();
            bridge.navigate("home", None);
            set_visible.call(false);
        }
    };

    // ── 保存配置（Step 1/2 完成）：camelCase 全字段（对齐 settings_view）──
    let save_and_finish = {
        let bridge = bridge.clone();
        let draft = draft.clone();
        let dirty = dirty.clone();
        let set_saved_at = set_saved_at.clone();
        let set_save_error = set_save_error.clone();
        let finish = finish.clone();
        move || {
            let d = draft.borrow().clone();
            let fields = json!({
                "apiKey": d.api_key,
                "model": d.model,
                "baseUrl": d.base_url,
                "providerId": d.provider_id,
                "endpoint": d.endpoint,
                "subagentApiKey": d.sub_api_key,
                "mmApiKey": d.mm_api_key,
            });
            *dirty.borrow_mut() = false;
            bridge.spawn_config_save(fields);
            // config.save 异步生效：UI 立即收尾（daemon 侧写入后轮询刷回）。
            set_saved_at.call(Some(std::time::Instant::now()));
            set_save_error.call(None);
            finish();
        }
    };

    // ── Step 0 欢迎 ─────────────────────────────────────────────────
    let welcome: Element = {
        let logo: Element = text_block(">_")
            .font_size(36.0)
            .semibold()
            .foreground(ThemeRef::AccentText)
            .into();
        let title: Element = text_block("DeepX")
            .font_size(36.0)
            .semibold()
            .into();
        let tagline: Element = text_block("本地优先的 AI 工作台——先连接模型，即可开始对话")
            .font_size(14.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        let feat1: Element = text_block("• 一次配置，多端同步（主/子代理/多模态可分别使用不同服务）")
            .font_size(13.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        let feat2: Element = text_block("• 会话、工具、上下文管理开箱即用")
            .font_size(13.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        let start_btn: Element = button("开始设置")
            .accent()
            .on_click({
                let set_step = set_step.clone();
                move || set_step.call(1)
            })
            .into();
        let skip_btn: Element = button("跳过")
            .subtle()
            .on_click(finish.clone())
            .into();
        let actions: Element = hstack((start_btn, skip_btn))
            .spacing(tokens::SPACE_3)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into();
        vstack((
            logo,
            title,
            tagline,
            Element::Empty,
            feat1,
            feat2,
            Element::Empty,
            actions,
        ))
        .spacing(tokens::SPACE_2)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
    };

    // ── Step 1 连接（唯一必填：主 API Key）──────────────────────────
    let connect: Element = {
        let providers = d.providers.clone();
        let provider_names: Vec<String> = providers.iter().map(|p| p.display.clone()).collect();
        let pidx = providers
            .iter()
            .position(|p| p.id == d.provider_id)
            .unwrap_or(0) as i32;
        let provider_combo: Element = if provider_names.is_empty() {
            text_block("（未加载到服务商列表，请检查 daemon 连接）")
                .foreground(ThemeRef::SecondaryText)
                .into()
        } else {
            ComboBox::new(provider_names)
                .selected_index(pidx)
                .header("提供方")
                .on_selection_changed({
                    let providers = providers.clone();
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |i: i32| {
                        let Some(p) = providers.get(i as usize) else {
                            return;
                        };
                        let mut d = draft.borrow_mut();
                        d.provider_id = p.id.clone();
                        let has_current = p.endpoints.iter().any(|e| e.id == d.endpoint);
                        if let Some(ep) = if has_current {
                            p.endpoints.iter().find(|e| e.id == d.endpoint)
                        } else {
                            p.endpoints.first()
                        } {
                            d.endpoint = ep.id.clone();
                            d.base_url = ep.base_url.clone();
                            if !ep.default_model.is_empty() {
                                d.model = ep.default_model.clone();
                            }
                        }
                        *dirty.borrow_mut() = true;
                    }
                })
                .into()
        };
        let endpoints = providers
            .iter()
            .find(|p| p.id == d.provider_id)
            .map(|p| p.endpoints.clone())
            .unwrap_or_default();
        let endpoint_labels: Vec<String> = endpoints.iter().map(|e| e.ui_label()).collect();
        let eidx = endpoints
            .iter()
            .position(|e| e.id == d.endpoint)
            .unwrap_or(0) as i32;
        let endpoint_combo: Element = if endpoint_labels.is_empty() {
            text_block("—").foreground(ThemeRef::SecondaryText).into()
        } else {
            ComboBox::new(endpoint_labels)
                .selected_index(eidx)
                .header("端点")
                .on_selection_changed({
                    let endpoints = endpoints.clone();
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |i: i32| {
                        let Some(ep) = endpoints.get(i as usize) else {
                            return;
                        };
                        let mut d = draft.borrow_mut();
                        d.endpoint = ep.id.clone();
                        d.base_url = ep.base_url.clone();
                        if !ep.default_model.is_empty() {
                            d.model = ep.default_model.clone();
                        }
                        *dirty.borrow_mut() = true;
                    }
                })
                .into()
        };
        let model_input: Element = text_box(d.model.clone())
            .header("模型")
            .on_text_changed({
                let draft = draft.clone();
                let dirty = dirty.clone();
                move |v| {
                    draft.borrow_mut().model = v;
                    *dirty.borrow_mut() = true;
                }
            })
            .into();
        let key_input: Element = PasswordBox::new()
            .value(d.api_key.clone())
            .placeholder_text("sk-…")
            .on_password_changed({
                let draft = draft.clone();
                let dirty = dirty.clone();
                move |v| {
                    draft.borrow_mut().api_key = v;
                    *dirty.borrow_mut() = true;
                }
            })
            .into();
        let hint: Element = text_block("API Key 仅保存在本机配置中（留空无法完成——可点「跳过」稍后在设置中配置）")
            .font_size(11.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        let key_ready = !d.api_key.is_empty();
        let back_btn: Element = button("上一步")
            .subtle()
            .on_click({
                let set_step = set_step.clone();
                move || set_step.call(0)
            })
            .into();
        let next_btn: Element = button("完成并进入")
            .accent()
            .enabled(key_ready)
            .on_click(save_and_finish.clone())
            .into();
        let skip_btn: Element = button("跳过")
            .subtle()
            .on_click(finish.clone())
            .into();
        let actions: Element = hstack((back_btn, skip_btn, next_btn))
            .spacing(tokens::SPACE_3)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into();
        let title_el: Element = text_block("连接你的模型服务")
            .font_size(20.0)
            .semibold()
            .into();
        let sub_el: Element = text_block("提供方与端点决定协议（Chat Completions / Responses API）")
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        vstack((
            title_el,
            sub_el,
            Element::Empty,
            provider_combo,
            endpoint_combo,
            model_input,
            key_input,
            hint,
            Element::Empty,
            actions,
        ))
        .spacing(tokens::SPACE_2)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
    };

    // ── Step 2 增强（全部可选）─────────────────────────────────────
    let enhance: Element = {
        let sub_input: Element = PasswordBox::new()
            .value(d.sub_api_key.clone())
            .placeholder_text("留空 = 暂不配置")
            .header("子代理 API Key")
            .on_password_changed({
                let draft = draft.clone();
                let dirty = dirty.clone();
                move |v| {
                    draft.borrow_mut().sub_api_key = v;
                    *dirty.borrow_mut() = true;
                }
            })
            .into();
        let mm_input: Element = PasswordBox::new()
            .value(d.mm_api_key.clone())
            .placeholder_text("留空 = 暂不配置")
            .header("多模态 API Key")
            .on_password_changed({
                let draft = draft.clone();
                let dirty = dirty.clone();
                move |v| {
                    draft.borrow_mut().mm_api_key = v;
                    *dirty.borrow_mut() = true;
                }
            })
            .into();
        let sub_hint: Element = text_block("子代理用于并行/后台任务，可与主模型不同服务")
            .font_size(11.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        let mm_hint: Element = text_block("多模态用于图像理解，可选")
            .font_size(11.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        let back_btn: Element = button("上一步")
            .subtle()
            .on_click({
                let set_step = set_step.clone();
                move || set_step.call(1)
            })
            .into();
        let done_btn: Element = button("完成")
            .accent()
            .on_click(save_and_finish.clone())
            .into();
        let title_el: Element = text_block("可选增强（稍后可在设置中配置）")
            .font_size(20.0)
            .semibold()
            .into();
        let actions: Element = hstack((back_btn, done_btn))
            .spacing(tokens::SPACE_3)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into();
        vstack((
            title_el,
            Element::Empty,
            sub_input,
            sub_hint,
            mm_input,
            mm_hint,
            Element::Empty,
            actions,
        ))
        .spacing(tokens::SPACE_2)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
    };

    let body: Element = match step {
        0 => welcome,
        1 => connect,
        _ => enhance,
    };

    // ── 反馈行（保存中/已保存/错误）──────────────────────────────────
    let feedback: Element = match save_error.clone() {
        Some(e) => text_block(e)
            .font_size(12.0)
            .foreground(ThemeRef::SystemCritical)
            .into(),
        None => match saved_at {
            Some(t) if t.elapsed() < Duration::from_secs(3) => text_block("已保存 ✓")
                .font_size(12.0)
                .foreground(ThemeRef::SystemSuccess)
                .into(),
            _ => Element::Empty,
        },
    };

    // ── 全屏布局：Mica 之上加 LayerFill 背景盖住壳，内容居中 ─────────
    let dots: Element = hstack((step_dot(step == 0), step_dot(step == 1), step_dot(step == 2)))
        .spacing(tokens::SPACE_2)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into();
    let card: Element = vstack((dots, Element::Empty, body, feedback))
    .spacing(tokens::SPACE_3)
    .max_width(460.0)
    .into();
    let card: Element = border(card)
        .padding(Thickness::uniform(tokens::SPACE_6))
        .background(ThemeRef::CardBackground)
        .corner_radius(tokens::RADIUS_CARD)
        .transition(motion::navigation_enter(), motion::content_exit())
        .into();

    grid((card,))
        .rows([GridLength::STAR])
        .columns([GridLength::STAR])
        .background(ThemeRef::LayerFill)
        .with_key("oobe-layer")
        .into()
}
