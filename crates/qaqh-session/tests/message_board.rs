//! Team message board canonical aggregate contract tests.

use std::fs::OpenOptions;
use std::io::Write;

use qaqh_session::canonical::generate_ulid;
use qaqh_session::session_fact_v2::{AgentPath, EventId, LogId, SessionId};
use qaqh_session::team::{
    BoardCreated, BoardFact, BoardId, BoardPayload, BoardStore, BoardSubscriptionTarget,
    ChannelCreated, ChannelId, PostCreated, PostId, SubscriptionChanged, TaskId, TeamActor,
    ThreadCreated, ThreadId, new_board_schema,
};

fn board_id() -> BoardId {
    BoardId::new(SessionId::new("0198f0a0-0000-7000-8000-000000000010"))
}

fn actor(path: &str) -> TeamActor {
    TeamActor::new(AgentPath::parse_absolute(path).expect("agent path"), None)
}

fn fact(board_id: &BoardId, actor: TeamActor, payload: BoardPayload) -> BoardFact {
    BoardFact {
        schema: new_board_schema(),
        board_id: board_id.clone(),
        log_id: LogId::new("board-test-log"),
        fact_seq: 0,
        event_id: EventId::new(generate_ulid()),
        ts_ms: 1_789_830_000_000,
        causation_id: None,
        actor,
        payload,
    }
}

fn created(board_id: &BoardId) -> BoardFact {
    fact(
        board_id,
        actor("/root"),
        BoardPayload::BoardCreated(BoardCreated {
            root_session_id: board_id.0.clone(),
            created_at_ms: 1_789_830_000_000,
        }),
    )
}

fn open(temp: &tempfile::TempDir) -> BoardStore {
    let id = board_id();
    BoardStore::open_or_create(temp.path(), id, 1_789_830_000_000).expect("open board store")
}

fn channel_created(board_id: &BoardId, channel_id: &ChannelId) -> BoardFact {
    fact(
        board_id,
        actor("/root"),
        BoardPayload::ChannelCreated(ChannelCreated {
            channel_id: channel_id.clone(),
            name: "engineering".into(),
            topic: Some("coordination".into()),
            created_by: actor("/root"),
            created_at_ms: 1_789_830_000_001,
        }),
    )
}

fn thread_created(board_id: &BoardId, channel_id: &ChannelId, thread_id: &ThreadId) -> BoardFact {
    fact(
        board_id,
        actor("/root"),
        BoardPayload::ThreadCreated(ThreadCreated {
            thread_id: thread_id.clone(),
            channel_id: channel_id.clone(),
            title: "wire the board".into(),
            task_id: Some(TaskId::generate()),
            created_by: actor("/root"),
            created_at_ms: 1_789_830_000_002,
        }),
    )
}

#[test]
fn create_channel_thread_post_subscription_and_replay() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = board_id();
    let channel_id = ChannelId::generate();
    let thread_id = ThreadId::generate();
    let post_id = PostId::generate();
    let worker = actor("/root/worker");

    let mut store = open(&temp);
    store.append(created(&id)).expect("board created");
    store
        .append(channel_created(&id, &channel_id))
        .expect("channel created");
    store
        .append(thread_created(&id, &channel_id, &thread_id))
        .expect("thread created");
    let thread = store.snapshot().threads[0].clone();
    store
        .append(fact(
            &id,
            worker.clone(),
            BoardPayload::PostCreated(PostCreated {
                post_id: post_id.clone(),
                thread_id: thread_id.clone(),
                task_id: thread.task_id.clone(),
                author: worker.clone(),
                body: "first post".into(),
                created_at_ms: 1_789_830_000_003,
                reply_to: None,
            }),
        ))
        .expect("post created");
    store
        .append(fact(
            &id,
            worker.clone(),
            BoardPayload::SubscriptionChanged(SubscriptionChanged {
                target: BoardSubscriptionTarget::Thread {
                    thread_id: thread_id.clone(),
                },
                subscriber: worker.clone(),
                subscribed: true,
                updated_at_ms: 1_789_830_000_004,
            }),
        ))
        .expect("subscription");

    let snapshot = store.snapshot();
    assert_eq!(snapshot.channels.len(), 1);
    assert_eq!(snapshot.threads.len(), 1);
    assert_eq!(snapshot.threads[0].post_count, 1);
    assert_eq!(snapshot.posts.len(), 1);
    assert_eq!(snapshot.posts[0].post_id, post_id);
    assert_eq!(snapshot.subscriptions.len(), 1);
    assert!(snapshot.subscriptions[0].subscribed);

    let reopened = open(&temp);
    assert_eq!(reopened.snapshot(), snapshot);
}

#[test]
fn duplicate_channel_and_unknown_references_are_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = board_id();
    let channel_id = ChannelId::generate();
    let other_channel = ChannelId::generate();
    let thread_id = ThreadId::generate();

    let mut store = open(&temp);
    store.append(created(&id)).expect("board created");
    store
        .append(channel_created(&id, &channel_id))
        .expect("channel created");

    let duplicate = store.append(channel_created(&id, &ChannelId::generate()));
    assert!(
        duplicate.is_err(),
        "duplicate channel name must be rejected"
    );

    let unknown_channel = store.append(fact(
        &id,
        actor("/root"),
        BoardPayload::ThreadCreated(ThreadCreated {
            thread_id: thread_id.clone(),
            channel_id: other_channel,
            title: "unknown channel".into(),
            task_id: None,
            created_by: actor("/root"),
            created_at_ms: 1_789_830_000_002,
        }),
    ));
    assert!(
        unknown_channel.is_err(),
        "thread must reference an existing channel"
    );

    let unknown_thread = store.append(fact(
        &id,
        actor("/root/worker"),
        BoardPayload::PostCreated(PostCreated {
            post_id: PostId::generate(),
            thread_id: ThreadId::generate(),
            task_id: None,
            author: actor("/root/worker"),
            body: "orphan".into(),
            created_at_ms: 1_789_830_000_003,
            reply_to: None,
        }),
    ));
    assert!(
        unknown_thread.is_err(),
        "post must reference an existing thread"
    );
}

#[test]
fn reply_must_stay_in_thread_and_task_link_must_match() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = board_id();
    let channel_id = ChannelId::generate();
    let first_thread = ThreadId::generate();
    let second_thread = ThreadId::generate();
    let first_post = PostId::generate();

    let mut store = open(&temp);
    store.append(created(&id)).expect("board created");
    store
        .append(channel_created(&id, &channel_id))
        .expect("channel created");
    store
        .append(thread_created(&id, &channel_id, &first_thread))
        .expect("first thread");
    store
        .append(fact(
            &id,
            actor("/root"),
            BoardPayload::ThreadCreated(ThreadCreated {
                thread_id: second_thread.clone(),
                channel_id: channel_id.clone(),
                title: "second thread".into(),
                task_id: None,
                created_by: actor("/root"),
                created_at_ms: 1_789_830_000_003,
            }),
        ))
        .expect("second thread");

    let first_task = store.snapshot().threads[0].task_id.clone();
    store
        .append(fact(
            &id,
            actor("/root/worker"),
            BoardPayload::PostCreated(PostCreated {
                post_id: first_post.clone(),
                thread_id: first_thread.clone(),
                task_id: first_task.clone(),
                author: actor("/root/worker"),
                body: "root post".into(),
                created_at_ms: 1_789_830_000_004,
                reply_to: None,
            }),
        ))
        .expect("first post");

    let wrong_task = store.append(fact(
        &id,
        actor("/root/worker"),
        BoardPayload::PostCreated(PostCreated {
            post_id: PostId::generate(),
            thread_id: first_thread.clone(),
            task_id: Some(TaskId::generate()),
            author: actor("/root/worker"),
            body: "wrong task".into(),
            created_at_ms: 1_789_830_000_005,
            reply_to: None,
        }),
    ));
    assert!(wrong_task.is_err(), "task link must match thread task");

    let cross_thread_reply = store.append(fact(
        &id,
        actor("/root/worker"),
        BoardPayload::PostCreated(PostCreated {
            post_id: PostId::generate(),
            thread_id: second_thread,
            task_id: None,
            author: actor("/root/worker"),
            body: "cross thread reply".into(),
            created_at_ms: 1_789_830_000_006,
            reply_to: Some(first_post),
        }),
    ));
    assert!(
        cross_thread_reply.is_err(),
        "reply target must stay in the same thread"
    );
}

#[test]
fn torn_tail_is_truncated_on_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = board_id();
    let channel_id = ChannelId::generate();

    let mut store = open(&temp);
    store.append(created(&id)).expect("board created");
    store
        .append(channel_created(&id, &channel_id))
        .expect("channel created");
    let expected = store.snapshot();

    {
        let mut events = OpenOptions::new()
            .append(true)
            .open(temp.path().join("events.jsonl"))
            .expect("open events");
        events.write_all(b"{\"torn\":").expect("write torn tail");
        events.sync_all().expect("sync torn tail");
    }

    let reopened = open(&temp);
    assert_eq!(reopened.snapshot(), expected);
    let events = std::fs::read(temp.path().join("events.jsonl")).expect("read events");
    assert!(
        !events.ends_with(b"{\"torn\":"),
        "torn tail must be truncated to committed offset"
    );
}
