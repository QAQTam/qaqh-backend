//! Pure utility functions for string/display operations.

/// Strip ANSI escape sequences from a string (color codes, cursor movements, etc.)
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == 0x1b { // ESC
            // Consume until we hit a letter (end of CSI sequence)
            while let Some(c) = bytes.next() {
                if c.is_ascii_alphabetic() || c == 0x9c { // ST string terminator
                    break;
                }
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

pub(super) fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

pub(super) fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 { format!("{}B", bytes) }
    else if bytes < 1024*1024 { format!("{:.1}K", bytes as f64 / 1024.0) }
    else { format!("{:.1}M", bytes as f64 / (1024.0*1024.0)) }
}

pub(super) fn pad_to_width(s: &str, target: usize) -> String {
    let w = display_width(s);
    if w < target {
        format!("{}{}", s, " ".repeat(target.saturating_sub(w)))
    } else {
        s.to_string()
    }
}

pub(super) fn textwrap(text: &str, max_width: usize) -> Vec<String> {
    let max = max_width.max(20);
    text.lines().flat_map(|line| {
        let w = display_width(line);
        if w <= max { return vec![line.to_string()]; }
        let mut r = Vec::new();
        let mut start = 0;
        while start < line.len() {
            // Walk forward accumulating display width until we hit max
            let mut col = 0;
            let mut end = start;
            for (i, c) in line[start..].char_indices() {
                let cw = if c.is_ascii() { 1 } else { 2 };
                if col + cw > max { break; }
                col += cw;
                end = start + i + c.len_utf8();
            }
            if end == start { end = start + line[start..].chars().next().map(|c| c.len_utf8()).unwrap_or(0); }
            r.push(line[start..end].to_string());
            start = end;
        }
        r
    }).collect()
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max { s.to_string() }
    else {
        let mut col = 0;
        let mut end = 0;
        for (i, c) in s.char_indices() {
            col += if c.is_ascii() { 1 } else { 2 };
            if col > max { break; }
            end = i + c.len_utf8();
        }
        format!("{}…", &s[..end])
    }
}

// ── Char-boundary-safe cursor helpers (UTF-8 multi-byte safety) ──

/// Ensure pos is on a char boundary (clamp left)
pub(crate) fn char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() { return s.len(); }
    let mut p = pos;
    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
    p
}

/// Move cursor one char left
pub(super) fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 { return 0; }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
    p
}

/// Move cursor one char right
pub(super) fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() { return s.len(); }
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) { p += 1; }
    p
}
