//! 有界执行输出捕获。采集、offset 分配、模型待读与 transcript 更新由同一
//! `&mut Capture` 串行完成；调用方在线程间共享时必须在同一把锁内调用。

use std::collections::VecDeque;

pub const TRANSCRIPT_BYTES: usize = 1024 * 1024;
pub const MODEL_PENDING_BYTES: usize = 256 * 1024;
pub const OUTPUT_PREVIEW_BYTES: usize = 16 * 1024;
pub const MAX_EVENT_BYTES: usize = 8 * 1024;
pub const MAX_BLOCKS: usize = 4096;

const HEAD_BYTES: usize = TRANSCRIPT_BYTES / 2;
const TAIL_BYTES: usize = TRANSCRIPT_BYTES - HEAD_BYTES;
const TRANSCRIPT_HEAD_BLOCKS: usize = MAX_BLOCKS / 2;
const TRANSCRIPT_TAIL_BLOCKS: usize = MAX_BLOCKS - TRANSCRIPT_HEAD_BLOCKS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSource {
    Stdout,
    Stderr,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    pub source: OutputSource,
    pub start_offset: u64,
    pub end_offset: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSnapshot {
    pub next_offset: u64,
    pub output_preview: String,
    pub preview_start_offset: u64,
    pub capture_omitted_bytes: u64,
    pub invalid_utf8_bytes: u64,
    pub transcript_head: Vec<OutputChunk>,
    pub transcript_tail: Vec<OutputChunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRead {
    pub chunks: Vec<OutputChunk>,
    pub omitted_bytes: u64,
}

#[derive(Debug, Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Capture {
    decoders: [Utf8Decoder; 3],
    next_offset: u64,
    invalid_utf8_bytes: u64,
    head: VecDeque<OutputChunk>,
    head_bytes: usize,
    tail: VecDeque<OutputChunk>,
    tail_bytes: usize,
    model_pending: VecDeque<OutputChunk>,
    model_pending_bytes: usize,
    model_omitted_bytes: u64,
    preview: String,
}

impl Capture {
    pub fn append_bytes(&mut self, source: OutputSource, bytes: &[u8]) -> Vec<OutputChunk> {
        let index = source_index(source);
        let mut decoded = String::new();
        decode_utf8(
            &mut self.decoders[index],
            bytes,
            &mut decoded,
            &mut self.invalid_utf8_bytes,
        );
        self.append_text(source, &decoded)
    }

    /// EOF 收尾时调用；尚未构成字符的尾部字节按一个 replacement character
    /// 发布，并按原始字节数计入 invalid_utf8_bytes。
    pub fn finish_source(&mut self, source: OutputSource) -> Vec<OutputChunk> {
        let decoder = &mut self.decoders[source_index(source)];
        if decoder.pending.is_empty() {
            return Vec::new();
        }
        self.invalid_utf8_bytes += decoder.pending.len() as u64;
        decoder.pending.clear();
        self.append_text(source, "\u{fffd}")
    }

    pub fn take_model_pending(&mut self, max_bytes: usize) -> ModelRead {
        let mut chunks = Vec::new();
        let mut returned = 0usize;
        let mut omitted = std::mem::take(&mut self.model_omitted_bytes);

        while let Some(chunk) = self.model_pending.pop_front() {
            let remaining = max_bytes.saturating_sub(returned);
            if remaining == 0 {
                omitted += chunk.text.len() as u64;
                continue;
            }
            let kept_bytes = utf8_prefix_len(&chunk.text, remaining);
            if kept_bytes > 0 {
                let mut kept = chunk.clone();
                kept.text.truncate(kept_bytes);
                kept.end_offset = kept.start_offset + kept_bytes as u64;
                returned += kept_bytes;
                chunks.push(kept);
            }
            let discarded = chunk.text.len() - kept_bytes;
            omitted += discarded as u64;
        }
        self.model_pending_bytes = 0;
        ModelRead {
            chunks,
            omitted_bytes: omitted,
        }
    }

    pub fn snapshot(&self) -> OutputSnapshot {
        let stored = self.head_bytes + self.tail_bytes;
        OutputSnapshot {
            next_offset: self.next_offset,
            output_preview: self.preview.clone(),
            preview_start_offset: self.next_offset.saturating_sub(self.preview.len() as u64),
            capture_omitted_bytes: self.next_offset.saturating_sub(stored as u64),
            invalid_utf8_bytes: self.invalid_utf8_bytes,
            transcript_head: self.head.iter().cloned().collect(),
            transcript_tail: self.tail.iter().cloned().collect(),
        }
    }

    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    fn append_text(&mut self, source: OutputSource, text: &str) -> Vec<OutputChunk> {
        let mut emitted = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let bytes = utf8_prefix_len(rest, MAX_EVENT_BYTES);
            let piece = rest
                .get(..bytes)
                .expect("utf8_prefix_len 一定落在字符边界且不超过长度");
            let start_offset = self.next_offset;
            self.next_offset += bytes as u64;
            let chunk = OutputChunk {
                source,
                start_offset,
                end_offset: self.next_offset,
                text: piece.to_owned(),
            };
            self.store_transcript(chunk.clone());
            self.store_model_pending(chunk.clone());
            self.append_preview(piece);
            emitted.push(chunk);
            rest = rest
                .get(bytes..)
                .expect("utf8_prefix_len 一定落在字符边界且不超过长度");
        }
        emitted
    }

    fn store_transcript(&mut self, chunk: OutputChunk) {
        let head_room = HEAD_BYTES.saturating_sub(self.head_bytes);
        let head_len = utf8_prefix_len(&chunk.text, head_room);
        let mut tail_start = 0;
        if head_len > 0 {
            let mut head_chunk = chunk.clone();
            head_chunk.text.truncate(head_len);
            head_chunk.end_offset = head_chunk.start_offset + head_len as u64;
            if push_bounded(&mut self.head, head_chunk, TRANSCRIPT_HEAD_BLOCKS) {
                self.head_bytes += head_len;
                tail_start = head_len;
            }
        }

        if tail_start < chunk.text.len() {
            let suffix = chunk
                .text
                .get(tail_start..)
                .expect("tail_start 来自 utf8_prefix_len，一定落在字符边界");
            let mut tail_chunk = chunk.clone();
            tail_chunk.start_offset += tail_start as u64;
            tail_chunk.text = suffix.to_owned();
            if !push_bounded(&mut self.tail, tail_chunk.clone(), TRANSCRIPT_TAIL_BLOCKS)
                && let Some(removed) = self.tail.pop_front()
            {
                self.tail_bytes -= removed.text.len();
                push_bounded(&mut self.tail, tail_chunk, TRANSCRIPT_TAIL_BLOCKS);
            }
            self.tail_bytes = self.tail.iter().map(|item| item.text.len()).sum();
            trim_front(&mut self.tail, &mut self.tail_bytes, TAIL_BYTES);
        }
    }

    fn store_model_pending(&mut self, chunk: OutputChunk) {
        if !push_bounded(&mut self.model_pending, chunk.clone(), MAX_BLOCKS) {
            if let Some(removed) = self.model_pending.pop_front() {
                self.model_pending_bytes -= removed.text.len();
                self.model_omitted_bytes += removed.text.len() as u64;
            }
            push_bounded(&mut self.model_pending, chunk, MAX_BLOCKS);
        }
        self.model_pending_bytes = self.model_pending.iter().map(|item| item.text.len()).sum();
        trim_model_pending(
            &mut self.model_pending,
            &mut self.model_pending_bytes,
            &mut self.model_omitted_bytes,
        );
    }

    fn append_preview(&mut self, text: &str) {
        self.preview.push_str(text);
        let trim = self.preview.len().saturating_sub(OUTPUT_PREVIEW_BYTES);
        if trim > 0 {
            let boundary = utf8_suffix_start(&self.preview, trim);
            self.preview.drain(..boundary);
        }
    }
}

fn source_index(source: OutputSource) -> usize {
    match source {
        OutputSource::Stdout => 0,
        OutputSource::Stderr => 1,
        OutputSource::Terminal => 2,
    }
}

fn decode_utf8(
    decoder: &mut Utf8Decoder,
    bytes: &[u8],
    output: &mut String,
    invalid_bytes: &mut u64,
) {
    let mut input = std::mem::take(&mut decoder.pending);
    input.extend_from_slice(bytes);
    loop {
        match std::str::from_utf8(&input) {
            Ok(valid) => {
                output.push_str(valid);
                return;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                output.push_str(std::str::from_utf8(&input[..valid_len]).expect("valid prefix"));
                if let Some(invalid_len) = error.error_len() {
                    *invalid_bytes += invalid_len as u64;
                    output.push('\u{fffd}');
                    input.drain(..valid_len + invalid_len);
                } else {
                    decoder.pending.extend_from_slice(&input[valid_len..]);
                    return;
                }
            }
        }
    }
}

fn push_bounded(chunks: &mut VecDeque<OutputChunk>, chunk: OutputChunk, max_blocks: usize) -> bool {
    if let Some(last) = chunks.back_mut()
        && last.source == chunk.source
        && last.end_offset == chunk.start_offset
    {
        last.text.push_str(&chunk.text);
        last.end_offset = chunk.end_offset;
        return true;
    }
    if chunks.len() == max_blocks {
        return false;
    }
    chunks.push_back(chunk);
    true
}

fn trim_front(chunks: &mut VecDeque<OutputChunk>, bytes: &mut usize, limit: usize) {
    while *bytes > limit {
        let excess = *bytes - limit;
        let Some(front) = chunks.front_mut() else {
            *bytes = 0;
            return;
        };
        if front.text.len() <= excess {
            *bytes -= front.text.len();
            chunks.pop_front();
        } else {
            let removed = utf8_suffix_start(&front.text, excess);
            front.text.drain(..removed);
            front.start_offset += removed as u64;
            *bytes -= removed;
        }
    }
}

fn trim_model_pending(chunks: &mut VecDeque<OutputChunk>, bytes: &mut usize, omitted: &mut u64) {
    while *bytes > MODEL_PENDING_BYTES {
        let excess = *bytes - MODEL_PENDING_BYTES;
        let Some(front) = chunks.front_mut() else {
            *bytes = 0;
            return;
        };
        if front.text.len() <= excess {
            let removed = front.text.len();
            *bytes -= removed;
            *omitted += removed as u64;
            chunks.pop_front();
        } else {
            let removed = utf8_suffix_start(&front.text, excess);
            front.text.drain(..removed);
            front.start_offset += removed as u64;
            *bytes -= removed;
            *omitted += removed as u64;
        }
    }
}

fn utf8_prefix_len(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn utf8_suffix_start(text: &str, min_bytes: usize) -> usize {
    let mut start = min_bytes.min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_utf8_is_reassembled_before_offsets_are_assigned() {
        let mut capture = Capture::default();
        assert!(
            capture
                .append_bytes(OutputSource::Stdout, &[0xe4, 0xb8])
                .is_empty()
        );
        let chunks = capture.append_bytes(OutputSource::Stdout, &[0xad]);
        assert_eq!(chunks[0].text, "中");
        assert_eq!((chunks[0].start_offset, chunks[0].end_offset), (0, 3));
        assert_eq!(capture.snapshot().invalid_utf8_bytes, 0);
    }

    #[test]
    fn model_poll_consumes_budget_overflow_once_and_preview_is_utf8_bounded() {
        let mut capture = Capture::default();
        capture.append_bytes(OutputSource::Stdout, "中abc".as_bytes());
        let read = capture.take_model_pending(4);
        assert_eq!(
            read.chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<String>(),
            "中a"
        );
        assert_eq!(read.omitted_bytes, 2);
        assert!(capture.take_model_pending(10).chunks.is_empty());
        let snapshot = capture.snapshot();
        assert_eq!(snapshot.next_offset, 6);
        assert!(
            snapshot
                .output_preview
                .is_char_boundary(snapshot.output_preview.len())
        );
    }

    #[test]
    fn transcript_and_preview_keep_bounded_head_tail() {
        let mut capture = Capture::default();
        let bytes = vec![b'x'; TRANSCRIPT_BYTES + 32];
        capture.append_bytes(OutputSource::Stderr, &bytes);
        let snapshot = capture.snapshot();
        assert_eq!(
            snapshot
                .transcript_head
                .iter()
                .map(|c| c.text.len())
                .sum::<usize>(),
            HEAD_BYTES
        );
        assert_eq!(
            snapshot
                .transcript_tail
                .iter()
                .map(|c| c.text.len())
                .sum::<usize>(),
            TAIL_BYTES
        );
        assert_eq!(snapshot.capture_omitted_bytes, 32);
        assert_eq!(snapshot.output_preview.len(), OUTPUT_PREVIEW_BYTES);
    }
}
