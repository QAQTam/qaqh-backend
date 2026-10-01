//! `read_image` tool — load an image into the model's own visual context.
//!
//! Unlike the former `image_query` tool (which forwarded images to an
//! external vision model), `read_image` attaches the image directly to the
//! tool result message. The gate lowers attached images into provider-native
//! media parts (`image_url` / `input_image`) on the next request, so a
//! vision-capable main model sees the pixels itself.
//!
//! Sources:
//! - `image_index`: an image uploaded in the current conversation
//!   (registered by engine_input when the user attaches one).
//! - `path`: an image file inside the workspace (admission authorizes the
//!   path resource before the handler runs).
//!
//! The tool is only usable on endpoints that declare vision input support
//! ([`image_model_supported`]: registry flag + optional per-model allowlist).
//! Rejections are actionable: the error tells the model to stop retrying,
//! inform the user, and fall back to a text-only approach.

pub mod image_utils;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ToolRisk;
use crate::file_mutate::{mutation_error, resolve_mutation_path};
use crate::tool_api::{
    OutputBudget, PathOp, ToolBody, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay,
    ToolExecutionError, ToolExposure, ToolHeader, ToolName, ToolProjection, ToolSource, TypedTool,
};

/// Raw byte cap before decoding (~20 MB).
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

// ── Global image registry ─────────────────────────────────────────────
///
/// Stores uploaded images keyed by session seed so read_image can look them
/// up by index without the LLM needing the raw base64 data. Images are
/// **peeked** (cloned) on lookup and never consumed — repeated reads across
/// turns must keep working while the upload stays in context.
static IMAGE_REGISTRY: std::sync::LazyLock<Mutex<HashMap<String, Vec<ImageEntry>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Registry 条目（A-2 L0 索引化）：只持有磁盘引用，不再常驻 base64。
#[derive(Clone)]
struct ImageEntry {
    mime_type: String,
    /// 内容寻址 id = sha256(base64 文本)（字节已由 qaqh_types::image_store 落盘）。
    sha256: String,
}

/// Register an uploaded image for a session. Called from engine_input.
///
/// A-2 L0：字节外置磁盘（内容寻址，幂等去重），registry 只留 `{mime, sha256}`
/// 索引；`peek_image` 时按需读盘。落盘失败则丢弃条目并告警（调用方拿到
/// peek None，会引导用户重新附加——与"上传离开上下文"同一语义）。
pub fn store_image(session_id: &str, mime_type: &str, data: &str) {
    let sha256 = match qaqh_types::image_store::store_image_b64(data, mime_type) {
        Ok(sha) => sha,
        Err(e) => {
            log::warn!("[read_image] registry store failed (entry dropped): {e}");
            return;
        }
    };
    if let Ok(mut reg) = IMAGE_REGISTRY.lock() {
        reg.entry(session_id.to_string())
            .or_default()
            .push(ImageEntry {
                mime_type: mime_type.to_string(),
                sha256,
            });
    }
}

/// 按 ImageRef 重建 registry 条目（resume 路径专用）：磁盘文件已在场，
/// 无需任何字节，O(1) 登记。
pub fn register_image_ref(session_id: &str, mime_type: &str, sha256: &str) {
    if let Ok(mut reg) = IMAGE_REGISTRY.lock() {
        reg.entry(session_id.to_string())
            .or_default()
            .push(ImageEntry {
                mime_type: mime_type.to_string(),
                sha256: sha256.to_string(),
            });
    }
}

/// Drop all registered images for a session.
///
/// Called before rebuilding the registry from persisted message history
/// (session restore) so repeated restores never shift the indices.
pub fn reset_images(session_id: &str) {
    if let Ok(mut reg) = IMAGE_REGISTRY.lock() {
        reg.remove(session_id);
    }
}

/// Peek at an image by index — returns base64 text without removing.
///
/// 磁盘读取在锁外执行（几 MB 文本，ms 级），不阻塞其它 seed 的 registry 操作。
pub fn peek_image(session_id: &str, index: usize) -> Option<(String, String)> {
    let entry = {
        let reg = IMAGE_REGISTRY.lock().ok()?;
        let entries = reg.get(session_id)?;
        entries.get(index)?.clone()
    };
    let data = qaqh_types::image_store::load_image_b64(&entry.sha256, &entry.mime_type).ok()?;
    Some((entry.mime_type.clone(), data))
}

// ── Capability gate ───────────────────────────────────────────────────

/// Whether the currently configured provider endpoint accepts image input.
use crate::runtime::image_model_supported;

// ── Typed output / handler ────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadImageArgs {
    #[serde(default)]
    pub image_index: Option<u64>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadImageOutput {
    pub status: String,
    pub source: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub base64_bytes: usize,
    #[serde(skip)]
    #[schemars(skip)]
    model_text: String,
    #[serde(skip)]
    #[schemars(skip)]
    image: Option<qaqh_types::ToolImage>,
}

impl ToolProjection for ReadImageOutput {
    fn images(&self) -> Vec<qaqh_types::ToolImage> {
        self.image.clone().into_iter().collect()
    }

    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text.clone(),
        }]
    }

    fn summary(&self) -> Option<String> {
        Some(format!(
            "{}x{} · {}",
            self.width, self.height, self.mime_type
        ))
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        let header = args
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .map(|path| ToolHeader::Path {
                path: path.to_string(),
                op: PathOp::Read,
            })
            .unwrap_or(ToolHeader::Other {
                label: "read_image".to_string(),
            });
        let (text, truncated) = crate::tool_api::display::clamp_display_body(&self.model_text);
        ToolDisplay::new(header, ToolBody::Text { text, truncated })
            .with_summary(self.summary().unwrap_or_else(|| "image".to_string()))
    }
}

pub struct ReadImageTool;

impl TypedTool for ReadImageTool {
    type Args = ReadImageArgs;
    type Output = ReadImageOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("read_image").expect("valid read_image tool name"),
            display_name: None,
            description: "Load image into visual context (by image_index or file path). Auto downscale if oversized."
                .to_string(),
            input_schema: read_image_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(ReadImageOutput))
                .expect("read_image output schema"),
            category: crate::permission::ToolCategory::Read,
            risk: ToolRisk::ReadOnly,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("read_image")
                .unwrap_or_default(),
        }
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        if !image_model_supported() {
            return Err(mutation_error(
                "tool_error",
                "read_image: the active model does not support image input. Do NOT retry read_image. Tell the user this model cannot see images, and continue with a text-only approach (e.g. ask the user to describe the image or paste relevant text).",
                None,
                json!({}),
            ));
        }

        let (raw_bytes, display) = if let Some(index) = args.image_index {
            let index = index as usize;
            let session_id = &ctx.session_id;
            let (_mime, data) = peek_image(session_id, index).ok_or_else(|| {
                mutation_error(
                    "tool_error",
                    format!(
                        "read_image: image_index {index} not found in session '{session_id}'. The upload may have left the context. Ask the user to re-attach it."
                    ),
                    None,
                    json!({}),
                )
            })?;
            if data.len() > image_utils::MAX_BASE64_BYTES * 4 {
                return Err(mutation_error(
                    "tool_error",
                    format!(
                        "read_image: upload #{index} is too large ({}, limit ~{} bytes)",
                        data.len(),
                        image_utils::MAX_BASE64_BYTES * 4
                    ),
                    None,
                    json!({}),
                ));
            }
            let raw = image_utils::decode_base64(&data).map_err(|error| {
                mutation_error(
                    "tool_error",
                    format!("read_image: upload #{index} has invalid base64: {error}"),
                    None,
                    json!({}),
                )
            })?;
            (raw, format!("upload #{index}"))
        } else if let Some(path) = args.path.as_deref().filter(|path| !path.is_empty()) {
            read_image_file(ctx, path)?
        } else {
            return Err(mutation_error(
                "tool_error",
                "read_image: either image_index or path is required. If you see [Image #N: ...] in the conversation, use image_index=N.",
                None,
                json!({}),
            ));
        };

        let normalized = image_utils::normalize_image(&raw_bytes).map_err(|error| {
            mutation_error(
                "tool_error",
                format!("read_image: {display}: {error}"),
                None,
                json!({}),
            )
        })?;
        let mime_type = normalized.mime.to_string();
        let data = image_utils::encode_base64(&normalized.bytes);
        let model_text = format!(
            "Image read successfully: {display} ({mime_type}, {}×{}, {} bytes base64 after normalization). The image is attached to this tool result and visible to you.",
            normalized.width,
            normalized.height,
            data.len()
        );
        Ok(ReadImageOutput {
            status: "ok".to_string(),
            source: display,
            mime_type: mime_type.clone(),
            width: normalized.width,
            height: normalized.height,
            base64_bytes: data.len(),
            model_text,
            image: Some(qaqh_types::ToolImage { mime_type, data }),
        })
    }
}

#[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
fn read_image_file(
    ctx: &ToolCallContext,
    path: &str,
) -> Result<(Vec<u8>, String), ToolExecutionError> {
    let full = PathBuf::from(resolve_mutation_path(ctx, path));
    let metadata = std::fs::metadata(&full).map_err(|error| {
        mutation_error(
            "tool_error",
            format!("read_image: cannot stat '{}': {error}", full.display()),
            None,
            json!({}),
        )
    })?;
    if !metadata.is_file() {
        return Err(mutation_error(
            "tool_error",
            format!("read_image: '{}' is not a regular file", full.display()),
            None,
            json!({}),
        ));
    }
    if metadata.len() as usize > MAX_IMAGE_BYTES {
        return Err(mutation_error(
            "tool_error",
            format!(
                "read_image: '{}' is too large ({} bytes, max ~{MAX_IMAGE_BYTES})",
                full.display(),
                metadata.len()
            ),
            None,
            json!({}),
        ));
    }
    let bytes = std::fs::read(&full).map_err(|error| {
        mutation_error(
            "tool_error",
            format!("read_image: cannot read '{}': {error}", full.display()),
            None,
            json!({}),
        )
    })?;
    Ok((bytes, full.display().to_string()))
}

fn read_image_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "image_index": {
                "type": "integer",
                "description": "Uploaded image index (0-based)"
            },
            "path": {
                "type": "string",
                "description": "Image file path"
            }
        },
        "additionalProperties": false,
        "anyOf": [
            { "required": ["image_index"] },
            { "required": ["path"] }
        ]
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(ReadImageTool);
}

/// Compatibility entry retained for existing in-process tests.
#[cfg(test)]
pub(super) fn handle_read_image(ctx: crate::ToolCallCtx) -> crate::ToolResult {
    use crate::file_mutate::ambient_tool_context;
    use crate::tool_api::{ErasedTool, TypedToolAdapter};

    let call_ctx = ambient_tool_context("read-image-compat", Duration::from_secs(30));
    TypedToolAdapter::new(ReadImageTool)
        .execute(call_ctx, ctx.args)
        .unwrap_or_else(|fatal| panic!("read_image tool fatal: {}", fatal.message))
        .to_tool_result()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid PNG (1×1 pixel, red)
    fn test_png_bytes() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0E, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x27, 0x34, 0x03,
            0x7A, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn registry_peek_is_non_destructive() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("qaqh-read-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("QAQH_DATA_DIR", &tmp) };
        let session_id = "read_image_registry_test";
        store_image(session_id, "image/png", "Zm9v");
        assert_eq!(
            peek_image(session_id, 0),
            Some(("image/png".into(), "Zm9v".into()))
        );
        // Peek again — must still be there (no consume semantics).
        assert_eq!(
            peek_image(session_id, 0),
            Some(("image/png".into(), "Zm9v".into()))
        );
        assert_eq!(peek_image(session_id, 1), None);
        assert_eq!(peek_image("other-seed", 0), None);
        unsafe { std::env::remove_var("QAQH_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reset_then_replay_keeps_indices_stable() {
        // 模拟 session restore：先 reset 再按历史顺序重放注册，
        // 重复 restore 不产生重复条目、不移动索引。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("qaqh-read-img-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("QAQH_DATA_DIR", &tmp) };
        let session_id = "read_image_reset_test";
        store_image(session_id, "image/png", "AAA");
        store_image(session_id, "image/jpeg", "BBB");
        reset_images(session_id);
        reset_images(session_id); // 幂等
        store_image(session_id, "image/png", "AAA");
        store_image(session_id, "image/jpeg", "BBB");
        assert_eq!(
            peek_image(session_id, 0),
            Some(("image/png".into(), "AAA".into()))
        );
        assert_eq!(
            peek_image(session_id, 1),
            Some(("image/jpeg".into(), "BBB".into()))
        );
        assert_eq!(peek_image(session_id, 2), None);
        unsafe { std::env::remove_var("QAQH_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn register_image_ref_rebuilds_peek_without_bytes() {
        // A-2 L0 resume 重建路径：ImageRef 已在场（磁盘有文件），
        // register_image_ref 零字节登记，peek 读盘还原。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("qaqh-read-img-ref-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("QAQH_DATA_DIR", &tmp) };

        let b64 = "aW1hZ2UtcmVmLXJlYnVpbGQ=";
        let sha = qaqh_types::image_store::store_image_b64(b64, "image/jpeg").expect("store");
        let session_id = "read_image_ref_test";
        reset_images(session_id);
        register_image_ref(session_id, "image/jpeg", &sha);
        assert_eq!(
            peek_image(session_id, 0),
            Some(("image/jpeg".into(), b64.into()))
        );
        // 重复重建幂等（reset + 重放，索引稳定）。
        reset_images(session_id);
        register_image_ref(session_id, "image/jpeg", &sha);
        assert_eq!(
            peek_image(session_id, 0),
            Some(("image/jpeg".into(), b64.into()))
        );

        unsafe { std::env::remove_var("QAQH_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn typed_read_image_projects_attachment_without_base64_in_model_text() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        assert!(
            manager.builtins["read_image"].legacy.is_none(),
            "read_image still has legacy executor"
        );

        let output = ReadImageOutput {
            status: "ok".to_string(),
            source: "upload #0".to_string(),
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            base64_bytes: 4,
            model_text: "Image read successfully: upload #0 (image/png, 1×1, 4 bytes base64 after normalization). The image is attached to this tool result and visible to you.".to_string(),
            image: Some(qaqh_types::ToolImage {
                mime_type: "image/png".to_string(),
                data: "AAAA".to_string(),
            }),
        };
        let images = output.images();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data, "AAAA");
        let blocks = output.model_blocks();
        let model_text = match &blocks[0] {
            ToolContentBlock::Text { text } => text,
            other => panic!("unexpected read_image model block: {other:?}"),
        };
        assert!(!model_text.contains("AAAA"));
        assert!(model_text.contains("attached"));
    }

    #[test]
    fn missing_args_fail_without_session() {
        // No args at all → argument error (before any capability check side
        // effects beyond the enabled probe).
        let ctx = crate::ToolCallCtx {
            id: "read-image-test".into(),
            name: "read_image".into(),
            action: String::new(),
            args: serde_json::json!({}),
            tx_progress: None,
            timeout_secs: Some(30),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            skill_effects: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let result = handle_read_image(ctx);
        assert!(!result.is_success());
    }

    #[test]
    fn png_magic_detected_and_encoded() {
        let bytes = test_png_bytes();
        assert_eq!(image_utils::detect_mime_from_bytes(&bytes), "image/png");
        let b64 = image_utils::encode_base64(&bytes);
        assert!(b64.starts_with("iVBORw0KGgo"));
    }
}
