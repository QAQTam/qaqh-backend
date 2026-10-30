//! Rendering of compaction requests from an already selected context window.

/// Compaction prompt V2: structured handoff protocol.
///
/// The LLM must produce a decision-first summary with mandatory anchor fields
/// (file line counts, build status, complexity-labeled remaining work).
/// The Thinking Appendix is optional and only included under strict rules.
const COMPACT_PROMPT: &str = "\
You are performing a CONTEXT CHECKPOINT COMPACTION. Create a structured \
handoff summary for another LLM that will resume the task.\n\
\n\
## OUTPUT FORMAT (follow strictly)\n\
\n\
### Decision Log\n\
For each key decision made during the work:\n\
- **Decision**: {what was chosen}\n\
- **Alternatives**: {what was rejected and why, if any}\n\
- **Status**: done / started but not finished / cancelled\n\
\n\
### State Snapshot\n\
- **Key files** (path + approximate line count): file.rs(~L123), ...\n\
- **Build status**: cargo check output summary (errors, warnings)\n\
- **Last successful action**: {command or tool} at {timestamp or turn}\n\
\n\
### Remaining Work\n\
Use complexity labels: [small], [medium], [large]. Include rough estimates.\n\
- [small] {task}  — ~1 edit or trivial fix\n\
- [medium] {task} — multiple edits, one file\n\
- [large] {task}  — new file or cross-crate changes\n\
\n\
### Thinking Appendix (OPTIONAL — include ONLY when applicable)\n\
Include ONLY when:\n\
- ≥2 dead-end investigation paths were tried before finding the root cause\n\
  → briefly note each dead-end and why it was wrong\n\
- A cross-crate or cross-module causality chain was needed to diagnose an issue\n\
  → note the chain (e.g. \"qaqh-workspace → qaqh-message → qaqh-session\")\n\
\n\
If neither condition is met, OMIT this section entirely.\n\
\n\
## RULES\n\
- Be concise: minimum tokens with maximum information density.\n\
- Do NOT mention the compaction process itself.\n\
- Decision Log, State Snapshot, and Remaining Work are MANDATORY.\n\
- Thinking Appendix is OPTIONAL — omit if no dead-ends or cross-crate chains.\n\
- If a section has no content, write \"None\" instead of omitting it.";

/// Prefix injected before a previous summary in UPDATE MODE.
/// Tells the LLM to merge new context with the prior structured handoff,
/// preserving existing Decision Log entries unless superseded.
const SUMMARY_PREFIX: &str = "\
Another language model previously worked on this task and produced a \
structured handoff summary (below). Merge the new context with the \
previous summary to create an updated checkpoint:\n\
- Preserve existing Decision Log entries unless the new context shows \
  they were completed or superseded.\n\
- Update State Snapshot and Remaining Work with the latest information.\n\
- If the previous summary has a Thinking Appendix, decide if the new \
  context adds more dead-end paths worth preserving; otherwise drop it.";

/// Inputs are selected by the context owner; rendering performs no I/O.
pub struct CompactPrompt<'a> {
    pub previous_summary: Option<&'a str>,
    pub anchors: &'a str,
    pub history: &'a [String],
    pub timeline: &'a str,
}

pub fn render(input: CompactPrompt<'_>) -> String {
    let CompactPrompt {
        previous_summary,
        anchors,
        history,
        timeline,
    } = input;
    if let Some(prev) = previous_summary {
        format!(
            "[COMPACT — UPDATE MODE]\n\n\
                 {SUMMARY_PREFIX}\n\n\
                 <previous-summary>\n{prev}\n</previous-summary>\n\n\
                 {anchors}\n\
                 --- HISTORY (newer context to merge) ---\n\
                 {}\n\
                 --- END HISTORY ---\n\n\
                 {COMPACT_PROMPT}",
            history.join("\n\n"),
        )
    } else {
        format!(
            "[COMPACT]\n\n\
                 Create a new checklist summary from the conversation history.\n\n\
                 {anchors}\n\
                 --- HISTORY ---\n\
                 {}\n\
                 --- END HISTORY ---\n\n\
                 Timeline:\n{timeline}\n\n\
                 {COMPACT_PROMPT}",
            history.join("\n\n"),
        )
    }
}

/// Construct the model messages for a compact request.
pub fn request_messages(prompt: &str) -> Vec<qaqh_types::Message> {
    vec![
        qaqh_types::Message::system(
            "You are a context-compaction assistant. Summarize the conversation history below.",
        ),
        qaqh_types::Message::user(prompt),
    ]
}
