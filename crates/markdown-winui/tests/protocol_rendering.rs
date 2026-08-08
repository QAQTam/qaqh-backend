//! Contract tests for the single declarative rendering path.
//!
//! These tests assert Transcript state and compact invalidation summaries.
//! They deliberately do not duplicate the reactor reconciler with a second
//! command protocol.

use markdown_core::ast::Inline;
use markdown_winui::{
    AnswerView, ConversationEvent, LiveSegment, RoundDeltaKind, Transcript, TranscriptInvalidation,
};

fn start(ts: &mut Transcript, id: &str) {
    ts.apply(&ConversationEvent::TurnStarted {
        turn_id: id.into(),
        user_text: format!("question {id}"),
    });
}

fn delta(id: &str, kind: RoundDeltaKind, text: &str) -> ConversationEvent {
    ConversationEvent::RoundDelta {
        turn_id: id.into(),
        round_num: 0,
        kind,
        delta: text.into(),
    }
}

fn live(ts: &Transcript, turn: usize) -> (&str, &[Inline], &[LiveSegment]) {
    match &ts.turns()[turn].rounds[0].answer {
        AnswerView::Streaming {
            raw,
            inlines,
            segments,
        } => (raw, inlines, segments),
        AnswerView::Final { .. } => panic!("expected streaming answer"),
    }
}

#[test]
fn live_markdown_is_model_state_not_a_render_command() {
    let mut ts = Transcript::new();
    start(&mut ts, "t1");
    let change = ts.apply(&delta("t1", RoundDeltaKind::Answering, "hello **wor"));
    assert_eq!(change.invalidation, TranscriptInvalidation::Live);
    assert!(change.extent_changed);
    let (raw, inlines, _) = live(&ts, 0);
    assert_eq!(raw, "hello **wor");
    assert_eq!(inlines, &[Inline::Text("hello **wor".into())]);

    ts.apply(&delta("t1", RoundDeltaKind::Answering, "ld**"));
    let (_, inlines, _) = live(&ts, 0);
    assert!(matches!(inlines, [Inline::Text(_), Inline::Bold(_)]));
}

#[test]
fn protocol_table_is_kept_in_order_with_text() {
    let mut ts = Transcript::new();
    start(&mut ts, "t1");
    ts.apply(&delta(
        "t1",
        RoundDeltaKind::Answering,
        "before\n```table\nA | B\n1 | 2\n```\nafter",
    ));
    let (_, _, segments) = live(&ts, 0);
    assert!(matches!(
        segments,
        [
            LiveSegment::Text(_),
            LiveSegment::Table(_),
            LiveSegment::Text(_)
        ]
    ));
}

#[test]
fn checkpoint_replaces_live_value_and_duplicate_is_noop() {
    let mut ts = Transcript::new();
    start(&mut ts, "t1");
    ts.apply(&delta("t1", RoundDeltaKind::Answering, "wrong"));
    let checkpoint = ConversationEvent::BlockCheckpoint {
        turn_id: "t1".into(),
        round_num: 0,
        kind: RoundDeltaKind::Answering,
        text: "authoritative".into(),
    };
    assert!(ts.apply(&checkpoint).changed());
    assert_eq!(live(&ts, 0).0, "authoritative");
    assert!(!ts.apply(&checkpoint).changed());
}

#[test]
fn completion_freezes_answer_and_late_delta_is_ignored() {
    let mut ts = Transcript::new();
    start(&mut ts, "t1");
    ts.apply(&delta("t1", RoundDeltaKind::Answering, "draft"));
    let change = ts.apply(&ConversationEvent::RoundCompleted {
        turn_id: "t1".into(),
        round_num: 0,
        thinking: Some("done thinking".into()),
        answer: Some("**final**".into()),
        output_ref: None,
        is_final: true,
    });
    assert_eq!(change.invalidation, TranscriptInvalidation::Structural);
    assert!(matches!(
        ts.turns()[0].rounds[0].answer,
        AnswerView::Final { .. }
    ));
    assert!(
        !ts.apply(&delta("t1", RoundDeltaKind::Answering, " late"))
            .changed()
    );
}

#[test]
fn updates_are_local_to_the_addressed_turn() {
    let mut ts = Transcript::new();
    start(&mut ts, "a");
    start(&mut ts, "b");
    ts.apply(&delta("b", RoundDeltaKind::Answering, "only b"));
    assert!(ts.turns()[0].rounds.is_empty());
    assert_eq!(live(&ts, 1).0, "only b");
}

#[test]
fn tool_cards_are_idempotent_upserts() {
    let mut ts = Transcript::new();
    start(&mut ts, "t1");
    let started = ConversationEvent::ToolStarted {
        tool_call_id: "call-1".into(),
        turn_id: "t1".into(),
        round_num: 0,
        name: "read_file".into(),
    };
    assert!(ts.apply(&started).changed());
    assert!(!ts.apply(&started).changed());
    ts.apply(&ConversationEvent::ToolFinished {
        tool_call_id: "call-1".into(),
        turn_id: "t1".into(),
        round_num: 0,
        result: serde_json::json!({"summary": "ok"}),
    });
    let cards = &ts.turns()[0].rounds[0].tool_calls;
    assert_eq!(cards.len(), 1);
    assert!(cards[0].done);
    assert_eq!(cards[0].args_display, "ok");
}

#[test]
fn output_ref_is_explicit_model_owned_work() {
    let mut ts = Transcript::new();
    start(&mut ts, "t1");
    ts.apply(&delta("t1", RoundDeltaKind::Answering, "preview"));
    let reference = serde_json::json!({
        "content_id": "abc",
        "media_type": "text/markdown",
        "sha256": "abc",
        "truncated": false
    });
    let completed = ConversationEvent::RoundCompleted {
        turn_id: "t1".into(),
        round_num: 0,
        thinking: None,
        answer: None,
        output_ref: Some(reference.clone()),
        is_final: true,
    };
    assert!(ts.apply(&completed).changed());
    assert!(ts.turns()[0].rounds[0].output_loading);
    assert_eq!(live(&ts, 0).0, "preview", "keep useful live preview");
    let pending = ts.take_pending_outputs();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].reference, reference);
    assert!(!ts.apply(&completed).changed(), "replay must not refetch");
    assert!(ts.take_pending_outputs().is_empty());

    ts.resolve_output("t1", 0, "full final");
    assert!(!ts.turns()[0].rounds[0].output_loading);
    assert!(matches!(
        ts.turns()[0].rounds[0].answer,
        AnswerView::Final { .. }
    ));
}

#[test]
fn output_failure_is_visible_and_keeps_preview() {
    let mut ts = Transcript::new();
    start(&mut ts, "t1");
    ts.apply(&delta("t1", RoundDeltaKind::Answering, "preview"));
    ts.fail_output("t1", 0, "expired");
    assert_eq!(live(&ts, 0).0, "preview");
    let round = &ts.turns()[0].rounds[0];
    assert!(!round.output_loading);
    assert_eq!(round.output_error.as_deref(), Some("expired"));
}

#[test]
fn frame_coalesces_adjacent_deltas_and_reports_one_live_change() {
    let mut ts = Transcript::new();
    start(&mut ts, "t1");
    let update = ts.apply_frame([
        delta("t1", RoundDeltaKind::Answering, "a"),
        delta("t1", RoundDeltaKind::Answering, "b"),
        delta("t1", RoundDeltaKind::Answering, "c"),
    ]);
    assert_eq!(update.invalidation, TranscriptInvalidation::Live);
    assert_eq!(live(&ts, 0).0, "abc");
}

#[test]
fn unknown_and_replayed_turn_start_are_noops() {
    let mut ts = Transcript::new();
    assert!(!ts.apply(&ConversationEvent::Unknown).changed());
    let started = ConversationEvent::TurnStarted {
        turn_id: "t1".into(),
        user_text: "same".into(),
    };
    assert!(ts.apply(&started).changed());
    assert!(!ts.apply(&started).changed());
    assert_eq!(ts.turn_count(), 1);
}
