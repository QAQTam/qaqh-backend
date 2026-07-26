use crate::error::VectorResult;

/// Splits text into embeddable segments (chunks).
///
/// Different chunking strategies are appropriate for different content types:
/// - Code: split at function/struct boundaries
/// - Documentation: split at markdown section headers
/// - Conversation: split by message turns
///
/// The default `TextChunker` uses paragraph-based splitting with
/// configurable max chunk size.
pub trait Chunker: Send + Sync {
    /// Split text into chunks.
    fn chunk(&self, text: &str) -> VectorResult<Vec<String>>;
}

/// A simple paragraph-based text chunker.
///
/// Splits on double newlines (paragraph boundaries), then merges
/// small paragraphs and splits large ones to stay within
/// `max_chunk_size` characters.
pub struct TextChunker {
    /// Maximum characters per chunk (soft limit).
    max_chunk_size: usize,
    /// Minimum characters per chunk (merge smaller paragraphs).
    min_chunk_size: usize,
}

impl TextChunker {
    /// Create a new text chunker.
    ///
    /// # Panics
    ///
    /// Panics if `min_chunk_size > max_chunk_size`.
    pub fn new(max_chunk_size: usize, min_chunk_size: usize) -> Self {
        assert!(
            min_chunk_size <= max_chunk_size,
            "min_chunk_size must be <= max_chunk_size"
        );
        Self {
            max_chunk_size,
            min_chunk_size,
        }
    }

    /// Default settings: 512-2048 characters per chunk.
    pub fn standard() -> Self {
        Self::new(2048, 512)
    }
}

impl Default for TextChunker {
    fn default() -> Self {
        Self::standard()
    }
}

impl Chunker for TextChunker {
    fn chunk(&self, text: &str) -> VectorResult<Vec<String>> {
        if text.is_empty() {
            return Ok(Vec::new());
        }

        // Step 1: Split into paragraphs
        let paragraphs: Vec<&str> = text
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();

        // Step 2: Merge small paragraphs and split large ones
        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();

        for para in paragraphs {
            // If adding this paragraph would exceed max, finalize current chunk
            if !current.is_empty() && current.len() + para.len() > self.max_chunk_size {
                chunks.push(std::mem::take(&mut current));
            }

            if para.len() > self.max_chunk_size {
                // Large paragraph: split by sentence boundaries
                if !current.is_empty() {
                    chunks.push(std::mem::take(&mut current));
                }
                let sub_chunks = split_long_text(para, self.max_chunk_size);
                chunks.extend(sub_chunks);
            } else if !current.is_empty() {
                current.push_str("\n\n");
                current.push_str(para);
            } else {
                current.push_str(para);
            }

            // If current exceeds soft limit, finalize it
            if current.len() >= self.max_chunk_size {
                chunks.push(std::mem::take(&mut current));
            }
        }

        // Don't forget the last chunk
        if !current.is_empty() {
            // Merge with last chunk if it's too small
            if let Some(last) = chunks.last_mut() {
                if last.len() < self.min_chunk_size {
                    last.push_str("\n\n");
                    last.push_str(&current);
                } else {
                    chunks.push(current);
                }
            } else {
                chunks.push(current);
            }
        }

        Ok(chunks)
    }
}

/// Split text that's too long for a single chunk at sentence boundaries.
fn split_long_text(text: &str, max_size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_size {
            chunks.push(remaining.trim().to_string());
            break;
        }

        // Find a sentence boundary within max_size
        let slice = &remaining[..max_size];
        let split_at = slice
            .rfind(['.', '。', '！', '？', '\n'])
            .map(|i| i + 1) // include the punctuation
            .unwrap_or(max_size); // fallback: hard cut

        let chunk = &remaining[..split_at];
        chunks.push(chunk.trim().to_string());
        remaining = remaining[split_at..].trim();
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text() {
        let chunker = TextChunker::default();
        let result = chunker.chunk("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn single_short_text() {
        let chunker = TextChunker::default();
        let result = chunker.chunk("Hello world").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "Hello world");
    }

    #[test]
    fn paragraph_split() {
        let chunker = TextChunker::default();
        let text = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.";
        let result = chunker.chunk(text).unwrap();
        assert_eq!(result.len(), 1); // all fit in one chunk
        assert!(result[0].contains("First"));
        assert!(result[0].contains("Third"));
    }

    #[test]
    fn large_paragraph_splitting() {
        let chunker = TextChunker::new(50, 20);
        let text = "A".repeat(200);
        let result = chunker.chunk(&text).unwrap();
        assert!(result.len() >= 2);
        for chunk in &result {
            assert!(chunk.len() <= 50 + 20); // with some tolerance
        }
    }
}
