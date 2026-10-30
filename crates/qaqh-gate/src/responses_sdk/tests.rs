    use super::*;

    #[test]
    fn tool_progress_waits_for_call_identity_and_sends_only_new_bytes() {
        let mut state = Resps::default();
        let mut events = Vec::new();
        state.tools.insert(4, (String::new(), String::new(), "你好".into()));
        state.emit_tool_progress(4, &mut |e| events.push(e));
        assert!(events.is_empty(), "no fabricated/item-id tool keys");
        let entry = state.tools.get_mut(&4).expect("slot");
        entry.0 = "call_edit".into();
        entry.1 = "edit".into();
        state.emit_tool_progress(4, &mut |e| events.push(e));
        state.emit_tool_progress(4, &mut |e| events.push(e));
        state.tools.get_mut(&4).expect("slot").2.push_str("世界");
        state.emit_tool_progress(4, &mut |e| events.push(e));
        let chunks: Vec<_> = events.iter().filter_map(|e| match e {
            StreamEvent::ToolCallProgress { id, args_chunk, .. } => {
                assert_eq!(id, "call_edit");
                Some(args_chunk.as_str())
            }
            _ => None,
        }).collect();
        assert_eq!(chunks, ["你好", "世界"]);
    }

    #[cfg(feature = "native-tool-plan")]
    #[test]
    fn native_plan_declaration_admission_and_replay() {
        use qaqh_tool_core::model_tool_plan::{InputAdapter, ModelToolAddress, ModelToolEntry, TextInputAdapter};
        let plan = ModelToolPlan::new(
            "1",
            "test",
            vec![ModelToolEntry {
                address: ModelToolAddress {
                    kind: ModelToolKind::Custom,
                    namespace: Some("functions".into()),
                    name: "apply_patch".into(),
                },
                declaration: ModelToolDeclaration::Custom {
                    description: "patch".into(),
                    format: CustomFormat::Text,
                },
                internal_tool_key: "apply_patch".into(),
                adapter: InputAdapter::TextInput(
                    TextInputAdapter::custom_raw("patch")
                        .with_defaults(serde_json::json!({"dry_run": false})),
                ),
            }],
        )
        .expect("plan");
        assert!(plan_declarations(&plan, &HashMap::new()).is_err());
        let declarations = plan_declarations(
            &plan,
            &HashMap::from([("functions".into(), "workspace".into())]),
        )
        .expect("declarations");
        assert!(
            matches!(&declarations[0], mutil_ai::ToolDeclaration::Namespace { name, tools, .. } if name == "functions" && matches!(&tools[0], mutil_ai::ScopedToolDeclaration::Custom { .. }))
        );
        let item = serde_json::json!({"type":"custom_tool_call", "name":"apply_patch", "namespace":"functions", "id":"ctc_1", "call_id":"call_1", "input":"你好\n*** End Patch", "status":"completed"});
        let call = native_call_from_item(&item).expect("native call");
        assert!(adapt_native(&call, None).is_err());
        let mut other = call.clone();
        other.namespace = Some("untrusted".into());
        assert!(adapt_native(&other, Some(&plan)).is_err());
        let blocks = adapt_native(&call, Some(&plan)).expect("admit");
        assert!(
            matches!(&blocks[0], ContentBlock::ToolUse { id, input, .. } if id == "call_1" && input["patch"] == "你好\n*** End Patch" && input["dry_run"] == false)
        );
        let messages = vec![
            qa_msg("assistant", blocks),
            Message::tool_result("call_1", qaqh_types::ToolResult::ok("done")),
        ];
        let projected = project_responses_messages(&messages, &ResponsesCompat::default());
        assert!(matches!(&projected[0].parts[0], Part::NativeToolCall(c) if c == &call));
        assert_eq!(projected[0].parts.len(), 1);
        assert!(
            matches!(&projected[1].parts[0], Part::ToolResult(result) if result.call_id.as_deref() == Some("call_1"))
        );
    }

    #[cfg(feature = "native-tool-plan")]
    #[test]
    fn default_namespace_resolution_stays_within_declared_addresses() {
        use qaqh_tool_core::model_tool_plan::{InputAdapter, ModelToolAddress, ModelToolEntry};
        let entry = |namespace: Option<&str>, internal: &str| ModelToolEntry {
            address: ModelToolAddress {
                kind: ModelToolKind::Function,
                namespace: namespace.map(str::to_owned),
                name: "read_file".into(),
            },
            declaration: ModelToolDeclaration::Function {
                description: "read".into(),
                parameters: serde_json::json!({"type":"object"}),
                strict: false,
            },
            internal_tool_key: internal.into(),
            adapter: InputAdapter::FunctionJson,
        };
        let plan =
            ModelToolPlan::new("1", "test", vec![entry(Some("functions"), "read")]).expect("plan");
        let call = mutil_ai::NativeToolCall {
            kind: mutil_ai::NativeToolKind::Function,
            namespace: None,
            name: "read_file".into(),
            call_id: "call_read".into(),
            item_id: None,
            payload: mutil_ai::NativeToolPayload::JsonArguments("{}".into()),
            completion: mutil_ai::NativeToolCompletion::Completed,
            provider_state: None,
        };
        let blocks = adapt_native(&call, Some(&plan)).expect("default namespace alias");
        assert!(matches!(&blocks[0], ContentBlock::ToolUse { name, .. } if name == "read"));
        let mut unknown = call.clone();
        unknown.name = "read".into();
        assert_eq!(
            adapt_native(&unknown, Some(&plan)).unwrap_err().to_string(),
            "unknown_tool_address"
        );
        let mut wrong_kind = call.clone();
        wrong_kind.kind = mutil_ai::NativeToolKind::Custom;
        wrong_kind.payload = mutil_ai::NativeToolPayload::RawText("{}".into());
        assert_eq!(
            adapt_native(&wrong_kind, Some(&plan))
                .unwrap_err()
                .to_string(),
            "unknown_tool_address"
        );
        let mut wrong_namespace = call.clone();
        wrong_namespace.namespace = Some("untrusted".into());
        assert_eq!(
            adapt_native(&wrong_namespace, Some(&plan))
                .unwrap_err()
                .to_string(),
            "unknown_tool_address"
        );
        let other_plan = ModelToolPlan::new("1", "test", vec![entry(Some("other"), "read")])
            .expect("other namespace plan");
        assert_eq!(
            adapt_native(&call, Some(&other_plan))
                .unwrap_err()
                .to_string(),
            "unknown_tool_address"
        );

        let mixed_plan = ModelToolPlan::new(
            "1",
            "test",
            vec![
                entry(Some("functions"), "namespaced_read"),
                entry(None, "flat_read"),
            ],
        )
        .expect("mixed plan");
        let blocks = adapt_native(&call, Some(&mixed_plan)).expect("exact flat address wins");
        assert!(matches!(&blocks[0], ContentBlock::ToolUse { name, .. } if name == "flat_read"));
    }

    fn provider() -> ProviderConfig {
        ProviderConfig::responses(
            "https://example.com/v1",
            "sk-test",
            "m",
            Some("/responses".into()),
        )
    }

    /// 临时诊断（timeline 火化后排查 DeepSeek 400）：读真实 session 归档，
    /// 走与生产一致的 project → build_request 路径，验证 tool call/output 配对
    /// 并 dump 请求 JSON 到 /tmp。
    #[test]
    fn diag_replay_real_session_pairing() {
        let path = std::env::var("QAQH_DIAG_SESSION").unwrap_or_else(|_| {
            "/home/qaqtamsy/.config/qaqh/sessions/01a188e3-6d1b-7119-b2bb-a82d485416a0/messages.jsonl".into()
        });
        let Ok(raw) = std::fs::read_to_string(&path) else {
            eprintln!("skip: session file missing: {path}");
            return;
        };
        let messages: Vec<Message> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("parse archived message"))
            .collect();
        let p = provider();
        let projected = project_responses_messages(&messages, &p.responses_compat);
        let mut call_ids: Vec<String> = Vec::new();
        let mut output_ids: Vec<String> = Vec::new();
        for (idx, msg) in projected.iter().enumerate() {
            for part in &msg.parts {
                match part {
                    Part::NativeToolCall(c) => call_ids.push(format!("#{idx} native:{}", c.call_id)),
                    Part::ToolCall(c) => call_ids.push(format!(
                        "#{idx} derived:{}",
                        c.id.clone().unwrap_or_default()
                    )),
                    Part::ToolResult(r) => output_ids.push(format!(
                        "#{idx} {}",
                        r.call_id.clone().unwrap_or_else(|| "<none>".into())
                    )),
                    _ => {}
                }
            }
        }
        eprintln!("calls:   {call_ids:?}");
        eprintln!("outputs: {output_ids:?}");
        let request = build_request(&p, projected, None, 0, None, None, true);
        let json = serde_json::to_string(&request).expect("serialize request");
        std::fs::write("/tmp/qaqh_diag_request.json", json).expect("write dump");
        // 配对断言：每个 call_id 都必须有对应 output
        let call_set: std::collections::HashSet<String> = call_ids
            .iter()
            .map(|s| s.split(':').next_back().unwrap_or("").to_string())
            .collect();
        let out_set: std::collections::HashSet<String> = output_ids.iter().cloned().collect();
        for call in &call_set {
            assert!(
                out_set.iter().any(|o| o.split(' ').next_back() == Some(call)),
                "orphan call without output: {call}"
            );
        }
    }

    fn qa_msg(role: &str, content: Vec<ContentBlock>) -> Message {
        Message {
            msg_id: None,
            role: role.into(),
            name: None,
            content,
        }
    }

    /// DeepSeek OAuth 实测：`function_call` 与其 `function_call_output` 之间
    /// 只要夹进一个 item（哪怕只是同一轮的 assistant 正文），端点就报
    /// `No tool output found for tool call call_xx`——历史里结果与 call_id
    /// 都逐字俱全。回归锚点：assistant 消息投影后**正文在前、调用在后**，
    /// 且正文不会落在未应答的调用之后。
    #[test]
    fn assistant_text_never_sits_between_a_call_and_its_output() {
        let p = provider();
        let call = |id: &str| ContentBlock::ResponseOutputItem {
            item: serde_json::json!({
                "type": "function_call", "call_id": id, "name": "read",
                "arguments": "{\"path\":\"a.md\"}", "status": "completed",
            }),
        };
        let history = vec![
            Message::user("read both"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::text("我先读这两份文档。"),
                    ContentBlock::ToolUse {
                        id: "call_a".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "a.md"}),
                    },
                    ContentBlock::ToolUse {
                        id: "call_b".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "b.md"}),
                    },
                    call("call_a"),
                    call("call_b"),
                ],
            ),
            qa_msg(
                "tool",
                vec![ContentBlock::ToolResult {
                    tool_use_id: "call_a".into(),
                    result: qaqh_types::ToolResult::ok("a"),
                }],
            ),
            qa_msg(
                "tool",
                vec![ContentBlock::ToolResult {
                    tool_use_id: "call_b".into(),
                    result: qaqh_types::ToolResult::ok("b"),
                }],
            ),
        ];
        let projected = project_responses_messages(&history, &p.responses_compat);
        let assistant = &projected[1].parts;
        let text_at = assistant
            .iter()
            .position(|part| matches!(part, Part::Text { text, .. } if text == "我先读这两份文档。"))
            .expect("正文必须保留");
        let first_call = assistant
            .iter()
            .position(|part| {
                matches!(part, Part::ProviderItem(item)
                    if item.provider_state.data.get("type").and_then(|v| v.as_str()) == Some("function_call"))
            })
            .expect("回放的 function_call 必须保留");
        assert!(
            text_at < first_call,
            "正文必须排在调用之前，否则会插在 call 与 output 之间：{:?}",
            assistant
        );
        assert_eq!(
            assistant.len(),
            3,
            "正文 + 两条回放调用，派生调用不得重复：{:?}",
            assistant
        );
    }

    #[test]
    fn responses_url_and_split() {
        assert_eq!(
            build_responses_url("https://api.openai.com/v1", None),
            "https://api.openai.com/v1/responses"
        );
        let (base, path) = split_url(&build_responses_url("https://x.com/api/v1", None));
        assert_eq!(
            (base.as_str(), path.as_str()),
            ("https://x.com", "/api/v1/responses")
        );
    }

    #[test]
    fn first_system_goes_top_later_system_becomes_developer() {
        let msgs = vec![
            Message::system("base"),
            Message::system("catalog"),
            Message::developer("envelope"),
            Message::user("hi"),
        ];
        let out = project_responses_messages(&msgs, &provider().responses_compat);
        assert_eq!(out[0].role, Role::System);
        assert_eq!(out[1].role, Role::Developer);
        assert_eq!(out[2].role, Role::Developer);
        assert_eq!(out[3].role, Role::User);
    }

    #[test]
    fn assistant_replays_raw_items_and_skips_duplicate_derivations() {
        let mut compat = provider().responses_compat.clone();
        compat.echo_reasoning_content = true;
        let raw_call = serde_json::json!({
            "type": "function_call", "call_id": "c1", "name": "exec", "arguments": "{\"a\":1}"
        });
        let msgs = vec![
            Message::user("go"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::ResponseOutputItem { item: raw_call },
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "exec".into(),
                        input: serde_json::json!({"a": 1}),
                    },
                    ContentBlock::ToolUse {
                        id: "c2".into(),
                        name: "read".into(),
                        input: serde_json::json!({"p": "x"}),
                    },
                ],
            ),
        ];
        let out = project_responses_messages(&msgs, &compat);
        let assistant = &out[1];
        let provider_items = assistant
            .parts
            .iter()
            .filter(|p| matches!(p, Part::ProviderItem(_)))
            .count();
        let calls: Vec<&WireToolCall> = assistant
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(provider_items, 1, "回放 item 原样保留");
        assert_eq!(
            calls.len(),
            1,
            "call_id 已被回放覆盖的派生 ToolUse 不再合成（防双份）"
        );
        assert_eq!(calls[0].id.as_deref(), Some("c2"));
    }

    #[test]
    fn native_read_image_tool_result_precedes_its_image_attachment() {
        let compat = provider().responses_compat;
        let messages = vec![
            Message::user("inspect this image"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::ResponseOutputItem {
                        item: serde_json::json!({
                            "type": "custom_tool_call",
                            "id": "ctc_read_image",
                            "call_id": "call_read_image",
                            "name": "read_image",
                            "input": "{\"image_index\":0}",
                            "status": "completed"
                        }),
                    },
                    ContentBlock::ToolUse {
                        id: "call_read_image".into(),
                        name: "read_image".into(),
                        input: serde_json::json!({"image_index": 0}),
                    },
                ],
            ),
            qa_msg(
                "tool",
                vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_read_image".into(),
                        result: qaqh_types::ToolResult::ok("Image read successfully."),
                    },
                    ContentBlock::Image {
                        mime_type: "image/png".into(),
                        data: "aGVsbG8=".into(),
                    },
                ],
            ),
        ];

        let projected = project_responses_messages(&messages, &compat);
        assert_eq!(
            projected.iter().map(|message| message.role.clone()).collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant, Role::Tool, Role::User]
        );
        assert!(matches!(
            &projected[1].parts[0],
            Part::NativeToolCall(call) if call.call_id == "call_read_image"
        ));
        assert!(matches!(
            &projected[2].parts[0],
            Part::ToolResult(result) if result.call_id.as_deref() == Some("call_read_image")
        ));
        assert!(
            projected[3]
                .parts
                .iter()
                .any(|part| matches!(part, Part::Image { .. }))
        );
    }

    #[test]
    fn echo_gating_drops_reasoning_items_and_bodies() {
        let mut compat = provider().responses_compat.clone();
        compat.echo_reasoning_content = false;
        let msgs = vec![
            Message::user("go"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::ResponseOutputItem {
                        item: serde_json::json!({"type": "reasoning", "summary": []}),
                    },
                    ContentBlock::Reasoning {
                        reasoning: "text".into(),
                        kind: Some(qaqh_types::ReasoningKind::Text),
                    },
                ],
            ),
        ];
        let out = project_responses_messages(&msgs, &compat);
        // 关闭回放后该 assistant 无任何可发内容——整条跳过（旧行为同）。
        assert_eq!(out.len(), 1, "reasoning-only assistant 应被丢弃：{out:?}");
    }

    /// canary：回放给 OpenAI Responses 端点的 `reasoning` / `web_search_call`
    /// item **必须带 `summary` 字段**——缺失即 HTTP 400
    /// `missing_required_parameter: 'input[N].summary'`（OAuth ChatGPT 线上实测）。
    /// 覆盖两条路径：有损历史（raw item 缺 summary / 只剩语义块走派生合成）。
    #[test]
    fn replayed_provider_items_always_carry_summary() {
        let mut compat = provider().responses_compat.clone();
        compat.echo_reasoning_content = true;
        compat.echo_web_search_call = true;
        let msgs = vec![
            Message::user("go"),
            qa_msg(
                "assistant",
                vec![
                    // 旧版本持久化 / 有损路径留下的无 summary raw item。
                    ContentBlock::ResponseOutputItem {
                        item: serde_json::json!({"type": "reasoning"}),
                    },
                    ContentBlock::WebSearchCall {
                        id: "ws_1".into(),
                        action: serde_json::json!({"type": "search"}),
                    },
                ],
            ),
        ];
        let out = project_responses_messages(&msgs, &compat);
        let assistant = &out[1];
        let items: Vec<serde_json::Value> = assistant
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ProviderItem(it) => Some(it.provider_state.data.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2, "raw reasoning + 派生 web_search_call");
        for item in &items {
            let it = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            assert!(
                item.get("summary").is_some(),
                "{it} 回放 item 必须带 summary（OpenAI 400 守卫）：{item}"
            );
        }
        // 补的是空数组（语义等价：无摘要），不覆盖原有字段。
        assert_eq!(items[0]["summary"], serde_json::json!([]));
        assert_eq!(items[1]["summary"], serde_json::json!([]));
        assert_eq!(items[1]["id"], "ws_1");
    }

    #[test]
    fn search_alias_roundtrip() {
        let mut p = provider();
        p.responses_compat.search_function_alias = Some("web_search_tool".into());
        let compat = p.responses_compat.clone();
        let td = ToolDef {
            call_type: "function".into(),
            function: qaqh_types::ToolFunction {
                name: "search".into(),
                description: "d".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let req = build_request(
            &p,
            vec![WireMessage::user("hi")],
            Some(vec![td]),
            100,
            None,
            None,
            true,
        );
        assert_eq!(req.tools[0].name, "web_search_tool");
        assert_eq!(
            canonical_function_name("web_search_tool", &compat),
            "search"
        );
    }

    #[test]
    fn effort_clamps_to_declared_bound() {
        // 端点声明 effort_max=high（OpenAI 缺省）：xhigh/max 都被夹到 high
        assert_eq!(
            clamp_effort(Some("max".into()), "high"),
            ReasoningEffort::High
        );
        // 端点声明 xhigh（muse 类网关经 responses_effort_max 配置，无模型名特判）
        assert_eq!(
            clamp_effort(Some("max".into()), "xhigh"),
            ReasoningEffort::XHigh
        );
        // 关闭思考的值被提升为 low（QAQ 永远思考）；未知值取端点上限
        assert_eq!(
            clamp_effort(Some("none".into()), "high"),
            ReasoningEffort::Low
        );
        assert_eq!(
            clamp_effort(Some("turbo".into()), "max"),
            ReasoningEffort::Max
        );
        assert_eq!(clamp_effort(None, "high"), ReasoningEffort::Medium);
    }

    /// canary：plan 直连路由的请求体**一个被禁字段都不能有**。
    ///
    /// 这是发版前的最后一道闸：被禁字段上线即 400
    /// `subscription_sharing_unsupported_capability`，而该错误码按文档意味着
    /// **我方发错了字段**——它不该靠人工 review 守住。
    #[test]
    fn siwc_plan_request_omits_every_forbidden_field() {
        let mut p = provider();
        p.responses_compat.siwc_plan = true;
        // 把「本会引入被禁字段」的开关全打开：`user` 字段与 hosted web_search。
        p.responses_compat.supports_user = true;
        p.responses_compat.web_search = true;
        p.prompt_cache_key = Some("seed-1".into());
        let request = build_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            4096, // 非 plan 端点会据此下发 max_output_tokens
            Some("high".into()),
            Some("u42".into()),
            true,
        );
        assert!(
            request.max_output_tokens.is_none(),
            "max_output_tokens 被禁"
        );
        assert!(
            request.server_tools.is_empty(),
            "plan 路由不提供 hosted 工具"
        );
        assert_eq!(request.extra_body["store"], serde_json::json!(false));
        assert_eq!(
            request.extra_body["parallel_tool_calls"],
            serde_json::json!(true)
        );
        for field in SIWC_PLAN_FORBIDDEN_EXTRA_FIELDS {
            assert!(
                !request.extra_body.contains_key(*field),
                "plan 请求体不得出现 {field}: {:?}",
                request.extra_body
            );
        }
        let opts = build_options(&p, &GateRetryPolicy::default());
        assert!(
            opts.provider_request.prompt_cache_key.as_deref() == Some("seed-1"),
            "prompt_cache_key 不在禁用清单里，plan 路由必须下发（缓存亲和）"
        );
    }

    /// 收口是**不可绕过**的：即便有人事后往 extra_body 里塞被禁字段，
    /// `apply_siwc_plan` 也必须摘干净（这是「不靠自觉」的那一半）。
    #[test]
    fn siwc_plan_strip_cannot_be_bypassed() {
        let mut p = provider();
        p.responses_compat.siwc_plan = true;
        let mut request =
            build_request(&p, vec![WireMessage::user("hi")], None, 0, None, None, true);
        for field in SIWC_PLAN_FORBIDDEN_EXTRA_FIELDS {
            request
                .extra_body
                .insert((*field).to_string(), serde_json::json!("injected"));
        }
        request.server_tools = vec![ServerTool::WebSearch];
        request.max_output_tokens = Some(99);
        apply_siwc_plan(&p, &mut request);
        assert!(request.max_output_tokens.is_none());
        assert!(request.server_tools.is_empty());
        for field in SIWC_PLAN_FORBIDDEN_EXTRA_FIELDS {
            assert!(!request.extra_body.contains_key(*field), "{field} 未被摘除");
        }
        assert_eq!(
            request.extra_body["store"],
            serde_json::json!(false),
            "store:false 必须保留"
        );
    }

    /// canary：plan 直连路由上 `reasoning` 回放 item 的 `content` 数组必须被
    /// 摘除（该路由 schema 上限 0 条）——HTTP 400 `Invalid 'input[N].content':
    /// array too long. Expected an array with maximum length 0`（OAuth ChatGPT
    /// 线上实测，任何工具调用后的下一回合回放历史即触发）。两条来源都要盖住：
    /// 原样回放的 provider item 与派生合成。
    #[test]
    fn siwc_plan_replayed_reasoning_items_carry_no_content() {
        let mut p = provider();
        p.responses_compat.siwc_plan = true;
        p.responses_compat.echo_reasoning_content = true;
        let history = vec![
            Message::user("go"),
            qa_msg(
                "assistant",
                vec![ContentBlock::ResponseOutputItem {
                    item: serde_json::json!({
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": "s"}],
                        "content": [{"type": "reasoning_text", "text": "cot"}],
                    }),
                }],
            ),
            qa_msg(
                "assistant",
                vec![ContentBlock::Reasoning {
                    reasoning: "derived".into(),
                    kind: Some(qaqh_types::ReasoningKind::Text),
                }],
            ),
        ];
        let projected = project_responses_messages(&history, &p.responses_compat);
        let request = build_request(&p, projected, None, 0, None, None, true);
        let mut reasoning = 0;
        for msg in &request.messages {
            for part in &msg.parts {
                if let Part::ProviderItem(item) = part
                    && item
                        .provider_state
                        .data
                        .get("type")
                        .and_then(|v| v.as_str())
                        == Some("reasoning")
                {
                    reasoning += 1;
                    assert!(
                        item.provider_state.data.get("content").is_none(),
                        "reasoning item 的 content 必须被摘除：{:?}",
                        item.provider_state.data
                    );
                    assert!(
                        item.provider_state.data.get("summary").is_some(),
                        "summary 收口不受影响（schema 必填）：{:?}",
                        item.provider_state.data
                    );
                }
            }
        }
        assert_eq!(reasoning, 2, "原样回放 + 派生合成两条来源都要投影出来");
    }

    /// 收口不可绕过：即便回放 item 事后被塞上 content，`apply_siwc_plan`
    /// 也必须摘干净（与 extra_body 收口同一半边）。
    #[test]
    fn siwc_plan_reasoning_content_strip_cannot_be_bypassed() {
        let mut p = provider();
        p.responses_compat.siwc_plan = true;
        let mut request = build_request(
            &p,
            vec![WireMessage::new(
                Role::Assistant,
                vec![provider_state_item(serde_json::json!({
                    "type": "reasoning",
                    "summary": [],
                    "content": [{"type": "reasoning_text", "text": "injected"}],
                }))],
            )],
            None,
            0,
            None,
            None,
            true,
        );
        // 模拟「收口之后」又被塞入的路径：直接重放收口本身也应摘除。
        apply_siwc_plan(&p, &mut request);
        let msg = &request.messages[0];
        let Part::ProviderItem(item) = &msg.parts[0] else {
            panic!("期望 ProviderItem");
        };
        assert!(item.provider_state.data.get("content").is_none());
    }

    /// 非 plan 端点（echo_reasoning_content 开启的兼容网关）回放形状**不变**：
    /// content 原样保留，摘除只属于 plan 直连路由的契约。
    #[test]
    fn non_plan_endpoints_keep_replayed_reasoning_content() {
        let p = provider();
        let mut compat = p.responses_compat.clone();
        compat.echo_reasoning_content = true;
        let history = vec![
            Message::user("go"),
            qa_msg(
                "assistant",
                vec![ContentBlock::Reasoning {
                    reasoning: "cot".into(),
                    kind: Some(qaqh_types::ReasoningKind::Text),
                }],
            ),
        ];
        let request = build_request(
            &p,
            project_responses_messages(&history, &compat),
            None,
            0,
            None,
            None,
            true,
        );
        let mut found = false;
        for msg in &request.messages {
            for part in &msg.parts {
                if let Part::ProviderItem(item) = part {
                    assert_eq!(
                        item.provider_state.data["content"][0]["text"],
                        serde_json::json!("cot"),
                        "非 plan 端点保持原样回放"
                    );
                    found = true;
                }
            }
        }
        assert!(found, "派生 reasoning item 应被投影出来");
    }

    /// OAuth 账号面头（ChatgptPlan 凭据来源注入）经 `build_options` 统一下发；
    /// 缺省（BYOK）端点一个账号头都不加。
    #[test]
    fn account_headers_flow_into_request_options() {
        let mut p = provider();
        p.chatgpt_account_headers = vec![
            ("chatgpt-account-id".to_string(), "acct_123".to_string()),
            ("x-openai-fedramp".to_string(), "true".to_string()),
        ];
        let opts = build_options(&p, &GateRetryPolicy::default());
        assert_eq!(
            opts.headers
                .get("chatgpt-account-id")
                .and_then(|v| v.to_str().ok()),
            Some("acct_123")
        );
        assert_eq!(
            opts.headers
                .get("x-openai-fedramp")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        let opts = build_options(&provider(), &GateRetryPolicy::default());
        assert!(opts.headers.get("chatgpt-account-id").is_none());
    }

    /// 缓存亲和头经 `chatgpt_account_headers` 通道统一下发（与账号头同一出口）。
    #[test]
    fn session_affinity_headers_flow_into_request_options() {
        let mut p = provider();
        p.responses_compat.siwc_plan = true;
        p.chatgpt_account_headers = vec![
            ("session-id".to_string(), "sess-7".to_string()),
            ("thread-id".to_string(), "sess-7".to_string()),
            ("x-client-request-id".to_string(), "sess-7".to_string()),
        ];
        let opts = build_options(&p, &GateRetryPolicy::default());
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            assert_eq!(
                opts.headers.get(name).and_then(|v| v.to_str().ok()),
                Some("sess-7"),
                "{name} 必须出现在出站请求头里"
            );
        }
    }

    /// DeepSeek OAuth 账号头（x-deepseek-harness-session-id / x-deepseek-harness-user-id）
    /// 严格按 session 颗粒度注入，经 `build_options` 统一下发到请求头中，为官方网关提供会话粘性路由。
    #[test]
    fn deepseek_account_headers_flow_into_request_options() {
        let mut p = provider();
        p.deepseek_account_headers = vec![
            ("x-deepseek-harness-session-id".to_string(), "0198f1a0-session-uuid".to_string()),
            ("x-deepseek-harness-user-id".to_string(), "user-dsh-001".to_string()),
        ];
        let opts = build_options(&p, &GateRetryPolicy::default());
        assert_eq!(
            opts.headers
                .get("x-deepseek-harness-session-id")
                .and_then(|v| v.to_str().ok()),
            Some("0198f1a0-session-uuid")
        );
        assert_eq!(
            opts.headers
                .get("x-deepseek-harness-user-id")
                .and_then(|v| v.to_str().ok()),
            Some("user-dsh-001")
        );
        let opts_default = build_options(&provider(), &GateRetryPolicy::default());
        assert!(opts_default.headers.get("x-deepseek-harness-session-id").is_none());
        assert!(opts_default.headers.get("x-deepseek-harness-user-id").is_none());
    }

    /// 回放 item id 卫生（对标 codex `ResponseItemId::is_prefixed`）：
    /// 无前缀 id 摘除让端点重发，合法前缀 id 原样保留。
    #[test]
    fn replayed_items_drop_unprefixed_ids() {
        let compat = provider().responses_compat;
        let msgs = vec![
            Message::user("go"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::ResponseOutputItem {
                        item: serde_json::json!({
                            "type": "reasoning", "id": "badid", "summary": [],
                        }),
                    },
                    ContentBlock::ResponseOutputItem {
                        item: serde_json::json!({
                            "type": "reasoning", "id": "rs_abc", "summary": [],
                        }),
                    },
                ],
            ),
        ];
        let out = project_responses_messages(&msgs, &compat);
        let items: Vec<serde_json::Value> = out[1]
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ProviderItem(it) => Some(it.provider_state.data.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2);
        assert!(
            items[0].get("id").is_none(),
            "无前缀 id 必须摘除: {items:?}"
        );
        assert_eq!(items[1]["id"], "rs_abc", "合法前缀 id 原样保留");
    }

    /// 默认关闭：非 plan 端点的 wire 形状**不能**被这次改动碰到。
    #[test]
    fn non_plan_endpoints_keep_the_current_wire_shape() {
        let mut p = provider();
        p.responses_compat.supports_user = true;
        p.responses_compat.web_search = true;
        let request = build_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            4096,
            None,
            Some("u1".into()),
            true,
        );
        assert_eq!(request.max_output_tokens, Some(4096));
        assert_eq!(request.server_tools, vec![ServerTool::WebSearch]);
        assert_eq!(request.extra_body["user"], serde_json::json!("u1"));
    }

    #[test]
    fn typed_switches_and_reserved_body_members() {
        let mut p = provider();
        p.responses_compat.supports_user = true;
        p.responses_compat.web_search = true;
        p.prompt_cache_key = Some("seed-1".into());
        let req = build_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            0,
            None,
            Some("u42".into()),
            true,
        );
        assert_eq!(req.server_tools, vec![ServerTool::WebSearch]);
        assert_eq!(req.extra_body["store"], serde_json::json!(false));
        assert_eq!(
            req.extra_body["parallel_tool_calls"],
            serde_json::json!(true)
        );
        assert_eq!(req.extra_body["user"], serde_json::json!("u42"));
        assert!(
            req.max_output_tokens.is_none(),
            "0 = 不发 max_output_tokens"
        );
        let opts = build_options(&p, &GateRetryPolicy::default());
        assert_eq!(
            opts.provider_request.prompt_cache_key.as_deref(),
            Some("seed-1")
        );
    }

    #[test]
    fn usage_maps_deepseek_cache_and_reasoning() {
        let u = WireUsage {
            input_tokens: Some(1000),
            output_tokens: Some(50),
            total_tokens: Some(1050),
            cache_read_tokens: Some(700),
            cache_write_tokens: None,
            reasoning_tokens: Some(20),
            raw: None,
        };
        let info = usage_to_info_responses(&u);
        assert_eq!(info.prompt_cache_hit_tokens, 700);
        assert_eq!(info.prompt_cache_miss_tokens, 300);
        assert_eq!(info.reasoning_tokens, 20);
        assert_eq!(info.cache_usage_reported, Some(true));
        let silent = WireUsage {
            input_tokens: Some(10),
            output_tokens: Some(1),
            total_tokens: Some(11),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            raw: None,
        };
        let info = usage_to_info_responses(&silent);
        assert_eq!(
            info.prompt_cache_miss_tokens, 0,
            "端点未上报 cached 时 miss=0（旧语义）"
        );
        assert_eq!(info.cache_usage_reported, Some(false));
    }

    #[test]
    fn incomplete_event_reads_as_terminal_with_reason_and_items() {
        let event = serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": [
                    {"type": "message", "role": "assistant", "content": [
                        {"type": "output_text", "text": "half a story"}]},
                    {"type": "function_call", "call_id": "c1", "name": "exec",
                     "arguments": "{\"command\":\"x\"}"}
                ],
                "usage": {"input_tokens": 9, "output_tokens": 3, "total_tokens": 12}
            }
        });
        let term = interpret_provider_stream(&event.to_string()).expect("incomplete terminal");
        assert!(!term.is_failed);
        assert_eq!(term.stop_reason.as_deref(), Some("max_output_tokens"));
        // message item 只回存；function_call → ToolUse + 回存（旧 preserve 形态）
        assert_eq!(
            term.raw_message.content.len(),
            3,
            "{:?}",
            term.raw_message.content
        );
        assert!(matches!(
            &term.raw_message.content[0],
            ContentBlock::ResponseOutputItem { .. }
        ));
        assert!(matches!(
            &term.raw_message.content[1],
            ContentBlock::ToolUse { id, .. } if id == "c1"
        ));
        assert!(term.usage.is_some());
    }

    #[test]
    fn failed_event_reads_as_fatal() {
        let event = serde_json::json!({
            "type": "response.failed",
            "response": {"error": {"message": "server is overloaded"}}
        });
        let term = interpret_provider_stream(&event.to_string()).expect("failed terminal");
        assert!(term.is_failed);
        assert_eq!(term.failure_message, "server is overloaded");
    }

    #[test]
    fn sanitizer_strips_nonstructural_keys() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": ["string", "null"], "format": "shell", "default": "ls"},
                "n": {"minimum": 1}
            },
            "required": ["cmd", 5]
        });
        let cleaned = sanitize_openai_schema(&schema);
        // 多类型保留为数组；format/default 等非结构键剥离
        assert_eq!(
            cleaned["properties"]["cmd"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert!(cleaned["properties"]["cmd"].get("format").is_none());
        assert_eq!(cleaned["properties"]["n"]["type"], "number");
        assert_eq!(cleaned["required"], serde_json::json!(["cmd"]));
    }

    /// BUG-2026-10-15 根因锁：流内参数损坏 + 终态已带同名调用 → 流条目按
    /// 重复丢弃。旧代码在此伪造 `{}` 参数，幽灵副本与终态真调用同时进执行，
    /// 造成「补丁已落盘 + 模型看到 missing field `patch` 假错」。
    #[test]
    fn reconcile_drops_corrupt_duplicate_when_terminal_carries_same_name() {
        let mut blocks = vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "apply_patch".into(),
            input: serde_json::json!({"patch": "*** Begin Patch\n*** End Patch"}),
        }];
        // 流内缓冲被陈旧快照截断 → 非法 JSON；id 又是 item id 形态（别名）。
        let mut streamed = HashMap::new();
        streamed.insert(
            0usize,
            (
                "fc_1".to_string(),
                "apply_patch".to_string(),
                "{\"patch\": \"*** Begin Pa".to_string(),
            ),
        );
        reconcile_streamed_tool_calls(&mut blocks, &streamed);
        assert_eq!(blocks.len(), 1, "损坏副本不得入执行面：{blocks:?}");
    }

    /// 同上但终态没带调用：以 Null 参数如实收口，交 runtime 执行界面
    /// 报「上游参数流损坏，未执行」。不得伪造 `{}`（missing-field 假错）。
    #[test]
    fn reconcile_keeps_null_truth_when_terminal_lacks_the_call() {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let mut streamed = HashMap::new();
        streamed.insert(
            0usize,
            (
                "call_1".to_string(),
                "apply_patch".to_string(),
                "{\"patch\": \"*** Begin Pa".to_string(),
            ),
        );
        reconcile_streamed_tool_calls(&mut blocks, &streamed);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "apply_patch");
                assert!(input.is_null(), "损坏参数保持 Null 真值: {input:?}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// id 形态别名（终态 call_id vs 流内 item id）+ 参数语义一致 → 判同一
    /// 调用，不得二次入执行面。
    #[test]
    fn reconcile_treats_aliased_ids_with_identical_args_as_duplicate() {
        let mut blocks = vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "a.rs"}),
        }];
        let mut streamed = HashMap::new();
        streamed.insert(
            0usize,
            (
                "fc_1".to_string(),
                "read".to_string(),
                "{\"path\": \"a.rs\"}".to_string(),
            ),
        );
        reconcile_streamed_tool_calls(&mut blocks, &streamed);
        assert_eq!(blocks.len(), 1, "别名 id + 同参不得重复执行: {blocks:?}");
    }
