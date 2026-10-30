//! Accounting for inline image payloads when estimating context size.

use crate::Message;

/// Conservative fixed per-image token charge used when accounting a request.
///
/// Provider-side image accounting is pixel based: a 1 MiB-class screenshot
/// costs a few thousand tokens (Anthropic ≈ `w*h/750`, OpenAI high detail
/// ≈ `85 + 170·tiles`), while the very same base64 payload counted as prose
/// costs 250k-300k tokens under the character heuristic
/// (BUG-2026-09-16-05 / D-14). Charging this fixed upper bound per image keeps
/// the local estimate on the same order of magnitude as the endpoint without
/// pretending to know the pixel dimensions.
pub const IMAGE_TOKEN_BUDGET: u64 = 4_096;

/// Placeholder that replaces an inline base64 payload before serialization.
///
/// It keeps the byte length (so the prepared-request key still distinguishes
/// differently sized images) but never the bytes themselves.
fn image_payload_placeholder(byte_len: usize) -> String {
    format!("<inline image payload: {byte_len} bytes>")
}

/// Replace inline image payloads with [`image_payload_placeholder`] and return
/// how many images were replaced.
///
/// Only the two structural positions that carry image bytes are rewritten —
/// [`crate::ContentBlock::Image::data`] and
/// [`crate::ToolResult::images`]`[].data`. Base64-looking text inside
/// ordinary string content is deliberately left untouched.
pub fn redact_image_payloads(messages: &mut [Message]) -> u64 {
    let mut images = 0u64;
    for message in messages.iter_mut() {
        for block in message.content.iter_mut() {
            match block {
                crate::ContentBlock::Image { data, .. } => {
                    let placeholder = image_payload_placeholder(data.len());
                    *data = placeholder;
                    images += 1;
                }
                crate::ContentBlock::ToolResult { result, .. } => {
                    for image in result.images.iter_mut() {
                        let placeholder = image_payload_placeholder(image.data.len());
                        image.data = placeholder;
                        images += 1;
                    }
                }
                _ => {}
            }
        }
    }
    images
}

/// Token charge for `images` redacted inline image payloads.
pub fn image_token_charge(images: u64) -> u64 {
    images.saturating_mul(IMAGE_TOKEN_BUDGET)
}
