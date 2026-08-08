//! XAML 原生设置页（P2）— SettingsView 的壳侧承载（路线图 Phase 3 首块）。
//!
//! 数据源：`bridge.core().settings_snapshot()`——`config.load` + `skills.list_tools`
//! + `workspace.status` 合并投影（shell_store::parse_config_load 等）；
//! 500ms rev 比对轮询（同 skills_view 模式）；首次进入 `spawn_config_load(false)`
//! 兜底拉取。
//!
//! 状态模型（D-2 执行权原则 + Web 单一数据源）：
//!   - 表单字段为本地草稿（use_state），"保存"按钮一次性 `config.save` 全字段
//!     （camelCase，对齐 Web `save()`）；rev 变化且无未保存修改时刷新草稿；
//!   - lang / theme / permissionLevel 的**状态归属 Web**（App.tsx）：壳侧变更
//!     立即 `emit_settings_action` 回传，Web 执行既有 handler（switchLang /
//!     switchTheme / changePermissionLevel）——避免双写；
//!   - workspace 运行模式：`workspace.set_mode` 壳直连；backend.restart 未实现
//!     → 保存后提示"下次启动生效"（ELECTRON-MIGRATION P1#3 前置）。
//!
//! 布局：左侧分类导航（models/api/context/subagent/workspace/appearance/
//! multimodal/advanced，对齐 Web categories）+ 右侧表单区（scroll_viewer）。
//!
//! 交互偏差（reactor 能力边界）：Web SecretInput 的"已配置徽章 + 替换/取消"
//! 折叠交互简化为 password_box 常显 + "已配置"徽章（输入为空 = 保留原值，
//! 非空 = 替换；对齐 Web apiKeyReplacement 语义）。

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use windows_reactor::*;

use deepx_fluent::motion;

use crate::bridge::{Bridge, SettingsProjection};
use crate::fonts;
use crate::shell_store::{SettingsSnapshot, normalize_effort};

/// 快照轮询间隔（同 sidebar / skills_view）。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 分类定义（id + 中文标签，对齐 Web `categories()`）。
const CATEGORIES: [(&str, &str); 8] = [
    ("models", "模型"),
    ("api", "API 密钥"),
    ("context", "上下文"),
    ("subagent", "子代理"),
    ("workspace", "工具套件"),
    ("appearance", "外观"),
    ("multimodal", "多模态"),
    ("advanced", "高级"),
];

/// effort 档位（对齐 Web EFFORT_LADDER）。
const EFFORT_LADDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// 工作区运行模式（对齐 Web workspace.mode 取值）。
const WORKSPACE_MODES: [&str; 3] = ["local", "wsl", "remote"];

/// 表单行：标签（140px）+ 控件（STAR）。
fn field_row(label: &str, control: Element) -> Element {
    let label_el: Element = text_block(label)
        .font_size(13.0)
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    grid((label_el.grid_column(0), control.grid_column(1)))
        .columns([GridLength::Pixel(140.0), GridLength::STAR])
        .column_spacing(8.0)
        .into()
}

/// 分类标题（h2 语义）。
fn section_title(text: &str) -> Element {
    text_block(text).font_size(16.0).semibold().into()
}

/// 设置页主体（放入内容区 Grid；由 main.rs 按 `current_view == "settings"` 切换）。
pub fn settings_view(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (_snapshot, set_snapshot) = cx.use_state::<Option<SettingsSnapshot>>(None);
    let (_proj, set_proj) = cx.use_state::<SettingsProjection>(SettingsProjection::default());
    let (category, set_category) = cx.use_state::<String>("models".to_string());
    let (saved_at, set_saved_at) = cx.use_state::<Option<std::time::Instant>>(None);
    let (save_error, set_save_error) = cx.use_state::<Option<String>>(None);
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    let last_proj_rev = cx.use_ref::<u64>(0);
    // 未保存修改标记（rev 刷新草稿的闸门）。
    let dirty = cx.use_ref::<bool>(false);

    // ── 草稿字段（渲染期读；rev 变化且 !dirty 时整体刷新）────────────
    let draft = cx.use_ref::<SettingsSnapshot>(SettingsSnapshot::default());
    let proj_draft = cx.use_ref::<SettingsProjection>(SettingsProjection::default());
    // theme/lang/permission 变更即发（不入 config.save 全量提交）。

    // ── 轮询：rev 比对刷新快照 + 草稿 ──────────────────────────────
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_snapshot = set_snapshot.clone();
        let set_proj = set_proj.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        let last_proj_rev = last_proj_rev.clone();
        let draft = draft.clone();
        let proj_draft = proj_draft.clone();
        let dirty = dirty.clone();
        move || {
            let core = bridge.core();
            // 首次进入：兜底拉权威快照（Web shell.setSettings 投影并行到达）。
            bridge.spawn_config_load(false);
            let (_, rev) = core.settings_snapshot();
            *last_rev.borrow_mut() = rev;
            let (_, prev) = core.settings_projection();
            *last_proj_rev.borrow_mut() = prev;
            if let Ok(t) = DispatcherTimer::new(POLL_INTERVAL, {
                let core = core.clone();
                let set_snapshot = set_snapshot.clone();
                let set_proj = set_proj.clone();
                let last_rev = last_rev.clone();
                let last_proj_rev = last_proj_rev.clone();
                let draft = draft.clone();
                let proj_draft = proj_draft.clone();
                let dirty = dirty.clone();
                move || {
                    // settings 投影（config.load 结果）rev 变化且无未保存修改 → 刷新草稿。
                    let (snap, srev) = core.settings_snapshot();
                    if srev != *last_rev.borrow() {
                        *last_rev.borrow_mut() = srev;
                        if let Some(snap) = &snap {
                            if !*dirty.borrow() {
                                *draft.borrow_mut() = snap.clone();
                            }
                        }
                        set_snapshot.call(snap);
                    }
                    // Web 初始投影（theme/lang/permission）rev 变化 → 刷新。
                    let (p, prev) = core.settings_projection();
                    if prev != *last_proj_rev.borrow() {
                        *last_proj_rev.borrow_mut() = prev;
                        if !*dirty.borrow() {
                            *proj_draft.borrow_mut() = p.clone();
                        }
                        set_proj.call(p);
                    }
                }
            }) {
                *timer.borrow_mut() = Some(t);
            }
        }
    });

    let d = draft.borrow().clone();
    let pd = proj_draft.borrow().clone();

    // ── 保存：config.save 全字段（camelCase，对齐 Web save()）────────
    let on_save = {
        let bridge = bridge.clone();
        let draft = draft.clone();
        let proj_draft = proj_draft.clone();
        let dirty = dirty.clone();
        let set_saved_at = set_saved_at.clone();
        let set_save_error = set_save_error.clone();
        move || {
            let d = draft.borrow().clone();
            let pd = proj_draft.borrow().clone();
            let fields = json!({
                "apiKey": d.api_key,
                "model": d.model,
                "baseUrl": d.base_url,
                "providerId": d.provider_id,
                "endpoint": d.endpoint,
                "maxTokens": d.max_tokens,
                "contextLimit": d.context_limit,
                "reasoningEffort": normalize_effort(&d.reasoning_effort).to_string(),
                "autoCompactThreshold": if d.auto_compact_threshold > 0.0 { d.auto_compact_threshold } else { 0.0 },
                "complianceEnabled": d.compliance_enabled,
                "lang": pd.lang,
                "fontFamily": d.font_family,
                "subagentModel": d.sub_model,
                "subagentBaseUrl": d.sub_base_url,
                "subagentApiKey": d.sub_api_key,
                "subagentMaxTokens": d.sub_max_tokens,
                "subagentTimeoutSecs": d.sub_timeout_secs,
                "subagentDefaultTools": d.sub_tools,
                "tokenizerPath": d.tokenizer_path,
                "multimodalProviderType": d.mm_provider_type,
                "multimodalEnabled": d.mm_enabled,
                "multimodalApiKey": d.mm_api_key,
                "multimodalBaseUrl": d.mm_base_url,
                "multimodalModel": d.mm_model,
                "multimodalMaxTokens": d.mm_max_tokens,
            });
            *dirty.borrow_mut() = false;
            bridge.spawn_config_save(fields);
            set_saved_at.call(Some(std::time::Instant::now()));
            set_save_error.call(None);
        }
    };

    // ── 字段 setter（写草稿 + 置 dirty）─────────────────────────────
    // 每个闭包捕获 bridge/draft/dirty；通用 helper 不便（借用冲突），逐字段生成。

    // ── 左侧分类导航（固定同构结构：Border(grid(竖条, 文字))）────────
    // 结构稳定性契约（卡死/错位根因修复）：所有 item 恒为 Border，active
    // 只改 background / 竖条颜色（modifiers 字段 diff），**绝不切换元素
    // 类型**（Border↔裸 TextBlock 的 kind 跳变会触发反复 unmount/mount，
    // 多次切换后控件树错位 → 渲染退化假死）。选中语义 = Win11
    // NavigationView 左侧竖条（3px Accent 圆角条），文字恒 PrimaryText。
    let nav_items: Vec<Element> = CATEGORIES
        .iter()
        .map(|(id, label)| {
            let active = *id == category;
            let indicator = border(text_block(""))
                .width(3.0)
                .height(16.0)
                .corner_radius(1.5)
                .vertical_alignment(VerticalAlignment::Center);
            let indicator = if active {
                indicator.background(ThemeRef::Accent)
            } else {
                indicator
            };
            let label_el: Element = text_block(*label)
                .font_size(13.0)
                .foreground(ThemeRef::PrimaryText)
                .vertical_alignment(VerticalAlignment::Center)
                .into();
            let row = grid((indicator.grid_column(0), label_el.grid_column(1)))
                .columns([GridLength::Auto, GridLength::STAR])
                .column_spacing(8.0)
                .padding(Thickness::xy(8.0, 8.0))
                .on_pointer_pressed({
                    let set_category = set_category.clone();
                    let id = id.to_string();
                    move |_| set_category.call(id.clone())
                });
            // 结构恒为 Border；仅 background 随 active 变化（diff_modifiers 原地更新）。
            let item = border(row).corner_radius(6.0);
            let item = if active {
                item.background(ThemeRef::SubtleFill)
            } else {
                item
            };
            item.with_key(id.to_string()).into()
        })
        .collect();
    let nav: Element = scroll_viewer(vstack(nav_items).spacing(2.0)).into();

    // ── 右侧表单区（按分类）────────────────────────────────────────
    let mut rows: Vec<Element> = Vec::new();

    // models：provider / endpoint / baseUrl / model
    if category == "models" {
        rows.push(section_title("模型提供方"));

        // ── 加载态：daemon 未响应时显示「加载中」而非错误的「无 provider 目录」 ──
        if !d.loaded {
            rows.push(
                text_block("正在加载配置…")
                    .font_size(13.0)
                    .foreground(ThemeRef::SecondaryText)
                    .into(),
            );
            // 跳过其余字段渲染（避免显示 0 默认值）；保存按钮仍在底部可用。
        } else {
            let providers = d.providers.clone();
            let provider_names: Vec<String> = providers.iter().map(|p| p.display.clone()).collect();
            let pidx = providers
                .iter()
                .position(|p| p.id == d.provider_id)
                .unwrap_or(0) as i32;
            let provider_combo = if provider_names.is_empty() {
                // 已加载但 providers 为空 = daemon 异常（不应发生，registry 硬编码 10 个）。
                text_block("（未配置任何 provider，请检查 daemon）")
                    .foreground(ThemeRef::SecondaryText)
                    .into()
            } else {
                ComboBox::new(provider_names)
                    .selected_index(pidx)
                    .header("Provider")
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
                            // 仅当新 provider 不支持当前 endpoint 时才取首条 endpoint
                            // （保留用户已选的 Responses API 等偏好）。
                            let has_current = p.endpoints.iter().any(|e| e.id == d.endpoint);
                            if !has_current {
                                if let Some(ep) = p.endpoints.first() {
                                    d.endpoint = ep.id.clone();
                                    d.base_url = ep.base_url.clone();
                                    if !ep.default_model.is_empty() {
                                        d.model = ep.default_model.clone();
                                    }
                                }
                            } else {
                                // 同步 base_url 到当前 endpoint 在新 provider 下的预设。
                                if let Some(ep) = p.endpoints.iter().find(|e| e.id == d.endpoint) {
                                    d.base_url = ep.base_url.clone();
                                    if !ep.default_model.is_empty() && d.model.is_empty() {
                                        d.model = ep.default_model.clone();
                                    }
                                }
                            }
                            *dirty.borrow_mut() = true;
                        }
                    })
                    .into()
            };
            rows.push(field_row("提供方", provider_combo));
            let endpoints = providers
                .iter()
                .find(|p| p.id == d.provider_id)
                .map(|p| p.endpoints.clone())
                .unwrap_or_default();
            // 使用 ui_label 显示协议 + Beta 标记，让用户能直观区分
            // Chat Completions API 与 Responses API (Beta)。
            let endpoint_labels: Vec<String> = endpoints.iter().map(|e| e.ui_label()).collect();
            let eidx = endpoints
                .iter()
                .position(|e| e.id == d.endpoint)
                .unwrap_or(0) as i32;
            let endpoint_combo = if endpoint_labels.is_empty() {
                text_block("—").foreground(ThemeRef::SecondaryText).into()
            } else {
                ComboBox::new(endpoint_labels)
                    .selected_index(eidx)
                    .header("Endpoint")
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
            rows.push(field_row("接口", endpoint_combo));
            rows.push(field_row(
                "Base URL",
                text_box(d.base_url.clone())
                    .on_text_changed({
                        let draft = draft.clone();
                        let dirty = dirty.clone();
                        move |v| {
                            draft.borrow_mut().base_url = v;
                            *dirty.borrow_mut() = true;
                        }
                    })
                    .into(),
            ));
            let model_names = endpoints
                .iter()
                .find(|e| e.id == d.endpoint)
                .map(|e| e.models.clone())
                .unwrap_or_default();
            // 模型：可编辑文本框（models 列表仅作提示，不强制选择）。
            rows.push(field_row(
                "模型",
                text_box(d.model.clone())
                    .placeholder_text(if model_names.is_empty() {
                        "e.g. deepseek-chat"
                    } else {
                        ""
                    })
                    .on_text_changed({
                        let draft = draft.clone();
                        let dirty = dirty.clone();
                        move |v| {
                            draft.borrow_mut().model = v;
                            *dirty.borrow_mut() = true;
                        }
                    })
                    .into(),
            ));
            rows.push(field_row(
                "最大 Tokens",
                NumberBox::new(d.max_tokens as f64)
                    // 下界 16（小模型可用），上界 1_000_000（主流长输出模型已支持 128K+）。
                    .range(16.0, 1_000_000.0)
                    .header("")
                    .on_value_changed({
                        let draft = draft.clone();
                        let dirty = dirty.clone();
                        move |v| {
                            draft.borrow_mut().max_tokens = v as u64;
                            *dirty.borrow_mut() = true;
                        }
                    })
                    .into(),
            ));
        }

        // ── Profile 切换器（在 models 区块底部展示并允许快速切换/管理） ──
        if d.loaded && !d.profiles.is_empty() {
            rows.push(section_title("预设"));
            let profile_names = d.profiles.clone();
            let pidx = profile_names
                .iter()
                .position(|n| n == &d.active_profile)
                .unwrap_or(0) as i32;
            rows.push(field_row(
                "当前预设",
                ComboBox::new(profile_names.clone())
                    .selected_index(pidx)
                    .header("Profile")
                    .on_selection_changed({
                        let bridge = bridge.clone();
                        let profile_names = profile_names.clone();
                        move |i: i32| {
                            if let Some(name) = profile_names.get(i as usize) {
                                // apply_profile 经 daemon 触发 config reload；
                                // 前端下次轮询会拿到新预设的字段。
                                bridge.spawn_apply_profile(name);
                            }
                        }
                    })
                    .into(),
            ));
            rows.push(
                text_block("切换预设会请求 daemon 应用并刷新配置（保存按钮不会触发切换）")
                    .font_size(11.0)
                    .foreground(ThemeRef::SecondaryText)
                    .into(),
            );
            // ── 另存为 / 删除（active 非 default 才允许删除） ──
            let active = d.active_profile.clone();
            let can_delete = active != "default";
            rows.push(field_row(
                "",
                hstack((
                    button("另存为").subtle().on_click({
                        let bridge = bridge.clone();
                        let profiles = profile_names.clone();
                        move || {
                            // 自动命名：profile_<N>，N = 现有 profile 数量（不与已存在冲突）。
                            let mut n = profiles.len();
                            let mut name = format!("profile_{n}");
                            while profiles.contains(&name) {
                                n += 1;
                                name = format!("profile_{n}");
                            }
                            bridge.spawn_save_profile(&name);
                        }
                    }),
                    button("删除当前预设")
                        .subtle()
                        .enabled(can_delete)
                        .on_click({
                            let bridge = bridge.clone();
                            let active = active.clone();
                            move || {
                                if active != "default" {
                                    bridge.spawn_delete_profile(&active);
                                }
                            }
                        }),
                ))
                .spacing(8.0)
                .into(),
            ));
        }
    }

    // api：apiKey / subagentApiKey
    if category == "api" {
        rows.push(section_title("API 密钥"));
        let key_row: Element = {
            let mut badge = Vec::new();
            if d.api_key_configured {
                badge.push(
                    text_block("已配置")
                        .font_size(11.0)
                        .foreground(ThemeRef::AccentText)
                        .into(),
                );
            }
            let input = PasswordBox::new()
                .value(d.api_key.clone())
                .placeholder_text(if d.api_key_configured {
                    "输入新值以替换"
                } else {
                    "sk-…"
                })
                .on_password_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().api_key = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into();
            let mut els: Vec<Element> = vec![input];
            els.extend(badge);
            hstack(els).spacing(8.0).into()
        };
        rows.push(field_row("主 API Key", key_row));
        rows.push(
            text_block("留空 = 保留已配置值（对齐 Web apiKeyReplacement 语义）")
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText)
                .into(),
        );
        rows.push(field_row(
            "子代理 API Key",
            PasswordBox::new()
                .value(d.sub_api_key.clone())
                .placeholder_text(if d.sub_api_key_configured {
                    "输入新值以替换"
                } else {
                    "sk-…"
                })
                .on_password_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().sub_api_key = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
    }

    // context：contextLimit / reasoningEffort / autoCompact / compliance
    if category == "context" {
        rows.push(section_title("上下文窗口"));
        rows.push(field_row(
            "上下文限制",
            NumberBox::new(d.context_limit as f64)
                .range(10000.0, 10_000_000.0)
                .header("")
                .on_value_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().context_limit = v as u64;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "推理强度",
            ComboBox::new(
                EFFORT_LADDER
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )
            .selected_index(
                EFFORT_LADDER
                    .iter()
                    .position(|e| *e == normalize_effort(&d.reasoning_effort))
                    .unwrap_or(2) as i32,
            )
            .header("")
            .on_selection_changed({
                let draft = draft.clone();
                let dirty = dirty.clone();
                move |i: i32| {
                    if let Some(e) = EFFORT_LADDER.get(i as usize) {
                        draft.borrow_mut().reasoning_effort = e.to_string();
                        *dirty.borrow_mut() = true;
                    }
                }
            })
            .into(),
        ));
        rows.push(field_row(
            "自动压缩",
            ToggleSwitch::new(d.auto_compact_threshold > 0.0)
                .header("")
                .on_toggled({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |on: bool| {
                        let mut d = draft.borrow_mut();
                        if on && d.auto_compact_threshold <= 0.0 {
                            d.auto_compact_threshold = 0.75;
                        } else if !on {
                            d.auto_compact_threshold = 0.0;
                        }
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        let threshold = if d.auto_compact_threshold > 0.0 {
            d.auto_compact_threshold
        } else {
            0.75
        };
        rows.push(field_row(
            "压缩阈值",
            Slider::new(threshold)
                .range(0.3, 0.95)
                .step(0.05)
                .header("")
                .on_value_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().auto_compact_threshold = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "合规模式",
            ToggleSwitch::new(d.compliance_enabled)
                .header("")
                .on_toggled({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |on: bool| {
                        draft.borrow_mut().compliance_enabled = on;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
    }

    // subagent：model / baseUrl / maxTokens / timeout / tools
    if category == "subagent" {
        rows.push(section_title("子代理"));
        rows.push(field_row(
            "子代理模型",
            text_box(d.sub_model.clone())
                .placeholder_text("留空 = 继承主模型")
                .on_text_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().sub_model = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "子代理 Base URL",
            text_box(d.sub_base_url.clone())
                .placeholder_text("留空 = 继承主配置")
                .on_text_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().sub_base_url = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "最大 Tokens",
            NumberBox::new(d.sub_max_tokens as f64)
                .range(16.0, 1_000_000.0)
                .header("")
                .on_value_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().sub_max_tokens = v as u64;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "超时（秒）",
            NumberBox::new(d.sub_timeout_secs as f64)
                .range(10.0, 3600.0)
                .header("")
                .on_value_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().sub_timeout_secs = v as u64;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(section_title("默认工具"));
        if d.tools.is_empty() {
            rows.push(
                text_block("（暂无可用工具）")
                    .foreground(ThemeRef::SecondaryText)
                    .into(),
            );
        } else {
            let tools = d.tools.clone();
            let selected = d.sub_tools.clone();
            for t in &tools {
                let checked = selected.contains(t);
                rows.push(
                    check_box(checked)
                        .content(t.clone())
                        .on_checked({
                            let draft = draft.clone();
                            let dirty = dirty.clone();
                            let t = t.clone();
                            move |on: bool| {
                                let mut d = draft.borrow_mut();
                                if on {
                                    if !d.sub_tools.contains(&t) {
                                        d.sub_tools.push(t.clone());
                                    }
                                } else {
                                    d.sub_tools.retain(|x| x != &t);
                                }
                                *dirty.borrow_mut() = true;
                            }
                        })
                        .into(),
                );
            }
        }
    }

    // workspace：mode / status / WSL
    if category == "workspace" {
        rows.push(section_title("工具套件运行环境"));
        rows.push(field_row(
            "运行模式",
            ComboBox::new(
                WORKSPACE_MODES
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )
            .selected_index(
                WORKSPACE_MODES
                    .iter()
                    .position(|m| *m == d.workspace_mode)
                    .unwrap_or(0) as i32,
            )
            .header("")
            .on_selection_changed({
                let draft = draft.clone();
                let dirty = dirty.clone();
                move |i: i32| {
                    if let Some(m) = WORKSPACE_MODES.get(i as usize) {
                        draft.borrow_mut().workspace_mode = m.to_string();
                        *dirty.borrow_mut() = true;
                    }
                }
            })
            .into(),
        ));
        let status_text = if d.workspace_active_mode.is_empty() {
            "（未查询到运行状态）".to_string()
        } else {
            format!(
                "已配置 {} · 当前 {} · {}",
                d.workspace_configured_mode, d.workspace_active_mode, d.workspace_endpoint
            )
        };
        rows.push(
            text_block(status_text)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .into(),
        );
        rows.push(
            text_block("⚠ 切换模式需重启后端（backend.restart 尚未迁移，保存后下次启动生效）")
                .font_size(12.0)
                .foreground(ThemeRef::SystemAttention)
                .into(),
        );
        rows.push(field_row(
            "",
            hstack((
                button("应用模式").subtle().on_click({
                    let bridge = bridge.clone();
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move || {
                        let mode = draft.borrow().workspace_mode.clone();
                        bridge.spawn_workspace_set_mode(&mode);
                        bridge.spawn_workspace_status();
                        *dirty.borrow_mut() = false;
                    }
                }),
                button("刷新状态").subtle().on_click({
                    let bridge = bridge.clone();
                    move || bridge.spawn_workspace_status()
                }),
                button("WSL 诊断").subtle().on_click({
                    let bridge = bridge.clone();
                    move || bridge.spawn_workspace_diagnose()
                }),
                button("安装 WSL").subtle().on_click({
                    let bridge = bridge.clone();
                    move || bridge.spawn_workspace_install_wsl()
                }),
            ))
            .spacing(8.0)
            .into(),
        ));
    }

    // appearance：theme / lang / font（theme/lang 变更即发；font 随保存提交）
    if category == "appearance" {
        rows.push(section_title("界面"));
        rows.push(field_row(
            "主题",
            ComboBox::new(vec![
                "system".to_string(),
                "light".to_string(),
                "dark".to_string(),
                "dark-gray".to_string(),
            ])
            .selected_index(match pd.theme.as_str() {
                "system" => 0,
                "light" => 1,
                "dark" => 2,
                "dark-gray" => 3,
                _ => 0,
            })
            .header("")
            .on_selection_changed({
                let proj_draft = proj_draft.clone();
                let dirty = dirty.clone();
                move |i: i32| {
                    let mode = match i {
                        1 => "light",
                        2 => "dark",
                        3 => "dark-gray",
                        _ => "system",
                    };
                    proj_draft.borrow_mut().theme = mode.to_string();
                    *dirty.borrow_mut() = true;
                    // WebView 移除：主题壳本地立即应用（三态映射同
                    // handle_message shell.setTheme 逻辑）。
                    let theme = match mode {
                        "light" => windows_reactor::RequestedTheme::Light,
                        "dark" | "dark-gray" => windows_reactor::RequestedTheme::Dark,
                        _ => windows_reactor::RequestedTheme::Default,
                    };
                    windows_reactor::set_requested_theme(theme);
                }
            })
            .into(),
        ));
        rows.push(field_row(
            "语言",
            ComboBox::new(vec!["中文".to_string(), "English".to_string()])
                .selected_index(if pd.lang == "en" { 1 } else { 0 })
                .header("")
                .on_selection_changed({
                    let proj_draft = proj_draft.clone();
                    let dirty = dirty.clone();
                    move |i: i32| {
                        let lang = if i == 1 { "en" } else { "zh" };
                        proj_draft.borrow_mut().lang = lang.to_string();
                        *dirty.borrow_mut() = true;
                        // WebView 移除：语言随保存按钮统一 config.save。
                    }
                })
                .into(),
        ));
        // ── 字体：Windows 系统字体族列表（fonts.rs 注册表枚举，进程级缓存）；
        // 首项「系统默认」= 空值。切换立即全局生效（FontFamily 继承属性，
        // set_font_family 设置内容根），随保存按钮 config.save 落盘。──
        let font_options: Vec<String> = {
            let mut v = vec!["系统默认".to_string()];
            v.extend(fonts::system_fonts_cached().iter().cloned());
            v
        };
        let font_idx = font_options
            .iter()
            .position(|f| *f == d.font_family)
            .unwrap_or(0) as i32;
        rows.push(field_row(
            "字体",
            ComboBox::new(font_options.clone())
                .selected_index(font_idx)
                .header("")
                .on_selection_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    let font_options = font_options.clone();
                    move |i: i32| {
                        let font = font_options.get(i as usize).cloned().unwrap_or_default();
                        draft.borrow_mut().font_family = font.clone();
                        *dirty.borrow_mut() = true;
                        // 立即全局生效（空 = 恢复系统默认）。
                        if font.is_empty() {
                            windows_reactor::set_font_family(None);
                        } else {
                            windows_reactor::set_font_family(Some(&font));
                        }
                    }
                })
                .into(),
        ));
        rows.push(
            text_block("字体应用于整个应用界面（FontFamily 继承属性；中文会回退到系统中文字体）")
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText)
                .into(),
        );
    }

    // multimodal：enabled / providerType / apiKey / baseUrl / model / maxTokens
    if category == "multimodal" {
        rows.push(section_title("多模态"));
        rows.push(field_row(
            "启用",
            ToggleSwitch::new(d.mm_enabled)
                .header("")
                .on_toggled({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |on: bool| {
                        draft.borrow_mut().mm_enabled = on;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "提供方类型",
            text_box(d.mm_provider_type.clone())
                .on_text_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().mm_provider_type = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "API Key",
            PasswordBox::new()
                .value(d.mm_api_key.clone())
                .placeholder_text(if d.mm_api_key_configured {
                    "输入新值以替换"
                } else {
                    "sk-…"
                })
                .on_password_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().mm_api_key = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "Base URL",
            text_box(d.mm_base_url.clone())
                .on_text_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().mm_base_url = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "模型",
            text_box(d.mm_model.clone())
                .on_text_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().mm_model = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
        rows.push(field_row(
            "最大 Tokens",
            NumberBox::new(d.mm_max_tokens as f64)
                .range(16.0, 1_000_000.0)
                .header("")
                .on_value_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().mm_max_tokens = v as u64;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into(),
        ));
    }

    // advanced：permissionLevel（radio）+ tokenizerPath（浏览）
    if category == "advanced" {
        rows.push(section_title("权限控制"));
        rows.push(field_row(
            "权限等级",
            RadioButtons::new(vec![
                "1 · 保守".to_string(),
                "2 · 询问".to_string(),
                "3 · 自动".to_string(),
                "4 · 全自动".to_string(),
            ])
            .selected_index((pd.permission_level.saturating_sub(1).min(3)) as i32)
            .header("")
            .on_selection_changed({
                let bridge = bridge.clone();
                let proj_draft = proj_draft.clone();
                let dirty = dirty.clone();
                move |i: i32| {
                    let level = (i.max(0) + 1) as u64;
                    proj_draft.borrow_mut().permission_level = level;
                    *dirty.borrow_mut() = true;
                    bridge.spawn_set_permission(level);
                }
            })
            .into(),
        ));
        rows.push(section_title("性能"));
        let tokenizer_row: Element = {
            let input: Element = text_box(d.tokenizer_path.clone())
                .placeholder_text("path/to/tokenizer.json")
                .on_text_changed({
                    let draft = draft.clone();
                    let dirty = dirty.clone();
                    move |v| {
                        draft.borrow_mut().tokenizer_path = v;
                        *dirty.borrow_mut() = true;
                    }
                })
                .into();
            let browse = button("浏览…").subtle().on_click({
                let bridge = bridge.clone();
                let draft = draft.clone();
                let dirty = dirty.clone();
                move || {
                    if let Ok(serde_json::Value::String(path)) = bridge.pick_file() {
                        draft.borrow_mut().tokenizer_path = path;
                        *dirty.borrow_mut() = true;
                    }
                }
            });
            hstack((input, browse)).spacing(8.0).into()
        };
        rows.push(field_row("Tokenizer 路径", tokenizer_row));
    }

    // ── 底部：保存按钮 + 状态 ───────────────────────────────────────
    let footer: Element = {
        let saved_text: Element = match saved_at {
            Some(t) if t.elapsed() < Duration::from_secs(3) => text_block("已保存 ✓")
                .font_size(12.0)
                .foreground(ThemeRef::SystemSuccess)
                .into(),
            _ => text_block("").into(),
        };
        let error_text: Element = match save_error.clone() {
            Some(e) => text_block(e)
                .font_size(12.0)
                .foreground(ThemeRef::SystemCritical)
                .into(),
            None => text_block("").into(),
        };
        hstack((
            button("保存设置").accent().on_click({
                let on_save = on_save.clone();
                move || on_save()
            }),
            saved_text,
            error_text,
        ))
        .spacing(12.0)
        .into()
    };

    // ── 表单区（rows 每行带 key：`{category}-{idx}`）────────────────
    // keyed reconcile：跨分类 key 全不同 → 切换分类时整行干净重建（杜绝
    // 同 index 类型跳变（grid↔TextBlock）导致的控件复用错位）；同分类内
    // 重渲染 key 相同 → 原地更新（表单输入状态保持）。
    // 动画：每行挂 enter transition（ImplicitShowAnimation）——切分类时
    // 新行 mount 使用统一内容动效；系统关闭动画时不挂 Composition 动画。
    let rows: Vec<Element> = rows
        .into_iter()
        .enumerate()
        .map(|(i, el)| {
            el.with_key(format!("{category}-{i}"))
                .transition(motion::content_enter(), None)
        })
        .collect();
    let form: Element = vstack(rows).spacing(10.0).into();
    let body: Element = vstack((form, footer)).spacing(16.0).into();
    let content: Element = scroll_viewer(body).into();

    // ── 根：左侧导航 + 右侧表单 ─────────────────────────────────────
    grid((nav.grid_column(0), content.grid_column(1)))
        .columns([GridLength::Pixel(180.0), GridLength::STAR])
        .padding(Thickness::xy(16.0, 16.0))
        .into()
}
