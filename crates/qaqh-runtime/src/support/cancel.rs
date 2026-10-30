//! Cancellation token — the only survivor of the old loop's `types` module.
//!
//! Tokens form a parent/child tree: cancelling a parent is immediately visible
//! to every descendant; cancelling a child affects neither parent nor siblings.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

// ═══════════════════════════════════════════════════════
// CancelToken — shared abort flag
// ═══════════════════════════════════════════════════════

/// Cancellation token shared between Loop and all Engines.
///
/// Each Engine receives a `&CancelToken` via `RingContext`.
/// Long-running operations (Gate SSE, tool threads) clone the
/// inner `Arc<AtomicBool>` via `.arc()` and poll it periodically.
///
/// Tokens form a parent/child tree: cancelling a parent is immediately
/// visible to every descendant, while cancelling a child does not affect its
/// parent or siblings. Setting the root token is the responsibility of the
/// Loop dispatcher; Engines only read it.
#[derive(Clone)]
pub struct CancelToken {
    node: Arc<CancelNode>,
    /// 可选的取消判定钩子（测试用）：置位后 `is_set()` 完全由钩子裁决。
    ///
    /// 生产路径恒为 `None`，`is_set()` 退化为读 `self.node.effective`——零行为变化。
    /// 测试用它把取消点钉在**批执行中途**（先生成批、后在收割窗口置位）。
    #[allow(clippy::type_complexity)]
    query: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

struct CancelNode {
    /// Cancellation requested directly on this node.
    own: AtomicBool,
    /// Effective cancellation including the live ancestor chain.
    effective: Arc<AtomicBool>,
    parent: Option<Weak<CancelNode>>,
    children: Mutex<Vec<Weak<CancelNode>>>,
}

impl CancelNode {
    fn root() -> Arc<Self> {
        Arc::new(Self {
            own: AtomicBool::new(false),
            effective: Arc::new(AtomicBool::new(false)),
            parent: None,
            children: Mutex::new(Vec::new()),
        })
    }

    fn child(parent: &Arc<Self>) -> Arc<Self> {
        let inherited = parent.effective.load(Ordering::SeqCst);
        let child = Arc::new(Self {
            own: AtomicBool::new(inherited),
            effective: Arc::new(AtomicBool::new(inherited)),
            parent: Some(Arc::downgrade(parent)),
            children: Mutex::new(Vec::new()),
        });
        parent
            .children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Arc::downgrade(&child));
        // Catch a parent cancellation racing with child registration.
        if parent.effective.load(Ordering::SeqCst) {
            child.own.store(true, Ordering::SeqCst);
        }
        child.recompute();
        child
    }

    fn set_own(&self, cancelled: bool) {
        self.own.store(cancelled, Ordering::SeqCst);
        self.recompute();
        if cancelled {
            self.latch_descendants();
        }
    }

    fn recompute(&self) {
        // Re-read after publishing so a concurrent parent update cannot be
        // overwritten by an older child snapshot.
        loop {
            let own = self.own.load(Ordering::SeqCst);
            let parent = self
                .parent
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|parent| parent.effective.load(Ordering::SeqCst));
            self.effective.store(own || parent, Ordering::SeqCst);
            let own_after = self.own.load(Ordering::SeqCst);
            let parent_after = self
                .parent
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|parent| parent.effective.load(Ordering::SeqCst));
            if own == own_after && parent == parent_after {
                break;
            }
        }

        for child in self.live_children() {
            child.recompute();
        }
    }

    fn latch_descendants(&self) {
        for child in self.live_children() {
            child.own.store(true, Ordering::SeqCst);
            child.recompute();
            child.latch_descendants();
        }
    }

    fn live_children(&self) -> Vec<Arc<Self>> {
        let mut guard = self
            .children
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut live = Vec::new();
        let mut weak = Vec::new();
        for child in guard.drain(..) {
            if let Some(child) = child.upgrade() {
                weak.push(Arc::downgrade(&child));
                live.push(child);
            }
        }
        *guard = weak;
        live
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            node: CancelNode::root(),
            query: None,
        }
    }

    /// 测试专用：用自定义判定钩子替换取消判定（生产路径不用）。
    ///
    /// 钩子返回 true 即视为「取消已到达」，用于精确控制取消时点。
    pub fn with_query_hook(hook: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        Self {
            node: CancelNode::root(),
            query: Some(hook),
        }
    }

    /// Derive a child token. Cancelling this token also cancels the child;
    /// cancelling the child never affects this token or its other children.
    pub fn child(&self) -> Self {
        Self {
            node: CancelNode::child(&self.node),
            query: None,
        }
    }

    /// Signal cancellation. Non-blocking.
    pub fn set(&self) {
        self.node.set_own(true);
    }

    /// Clear the cancel flag (called when starting a new turn).
    pub fn clear(&self) {
        self.node.set_own(false);
    }

    /// Check if cancellation has been requested.
    pub fn is_set(&self) -> bool {
        if let Some(hook) = &self.query {
            return hook();
        }
        self.node.effective.load(Ordering::SeqCst)
    }

    /// Clone the inner Arc for passing to threads / Gate layer.
    pub fn arc(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.node.effective)
    }
}

