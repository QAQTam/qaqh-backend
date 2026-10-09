//! Bounded, release-safe memory observability primitives.
//!
//! This crate deliberately does not replace the global allocator and never
//! captures payload text. It combines process-level measurements with
//! caller-supplied logical/estimated gauges and a bounded phase-sample ring.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

const MAX_PHASE_SAMPLES: usize = 4096;
const MAX_SESSION_GAUGES: usize = 512;
const MAX_COMPONENT_GAUGES: usize = 64;

/// Coarse source group for a component gauge.
///
/// Derived from the name prefix on update so clients can bucket rows without
/// re-implementing the naming convention. New groups are additive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentGroup {
    #[default]
    Other,
    Ringing,
    AgentRegistry,
    Service,
    Transport,
    Workspace,
}

impl ComponentGroup {
    pub fn classify(name: &str) -> Self {
        match name.split_once('.') {
            Some(("ringing", _)) => Self::Ringing,
            Some(("agent_registry", _)) => Self::AgentRegistry,
            Some(("service", _)) => Self::Service,
            Some(("mcp" | "lsp", _)) => Self::Transport,
            Some(("workspace", _)) => Self::Workspace,
            _ => Self::Other,
        }
    }
}

/// Stable lifecycle class for a phase sample, independent of its exact label.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    #[default]
    Other,
    SessionResume,
    ContextBuild,
    TokenPreflight,
    ProviderRequest,
    Compact,
    RequestLog,
}

impl PhaseKind {
    pub fn classify(phase: &str) -> Self {
        let mut parts = phase.split('.');
        let head = parts.next().unwrap_or("");
        let second = parts.next().unwrap_or("");
        match (head, second) {
            ("session", _) => Self::SessionResume,
            ("context", "estimate") => Self::TokenPreflight,
            ("context", _) => Self::ContextBuild,
            ("gate", _) => Self::ProviderRequest,
            ("compact", _) => Self::Compact,
            ("request_log", _) => Self::RequestLog,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProcessMemory {
    /// Resident pages currently mapped into the process working set.
    pub resident_bytes: Option<u64>,
    /// Private committed bytes where the platform exposes them.
    pub private_bytes: Option<u64>,
    /// Virtual address space size where the platform exposes it.
    pub virtual_bytes: Option<u64>,
    /// Peak resident bytes reported by the OS (not necessarily peak private).
    pub peak_resident_bytes: Option<u64>,
    pub source: &'static str,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionMemory {
    pub session_id: String,
    pub resident: bool,
    pub message_count: u64,
    pub turn_count: u64,
    pub content_block_count: u64,
    pub text_bytes: u64,
    pub image_bytes: u64,
    pub store_heap_estimate_bytes: u64,
    /// AgentState fields other than its MessageStore; still an estimate.
    pub agent_aux_heap_estimate_bytes: u64,
    pub pending_persist_ops: u64,
    pub context_message_count: u64,
    pub context_payload_bytes: u64,
    /// JSON byte count used by token preflight; inline image payloads are redacted.
    pub estimate_json_bytes: u64,
    pub last_phase: String,
    #[serde(default)]
    pub last_phase_kind: PhaseKind,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ComponentMemory {
    pub name: String,
    /// Coarse source group, derived from the name prefix on update.
    #[serde(default)]
    pub group: ComponentGroup,
    pub item_count: u64,
    /// `None` means the component does not expose a trustworthy payload count.
    pub payload_bytes: Option<u64>,
    /// Approximate owned heap usage, not allocator live bytes.
    pub heap_estimate_bytes: Option<u64>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseSample {
    pub sequence: u64,
    pub at_ms: u64,
    pub phase: String,
    /// Stable lifecycle class, derived from the phase label at record time.
    pub kind: PhaseKind,
    pub session_id: Option<String>,
    pub process: ProcessMemory,
    pub store_heap_estimate_bytes: Option<u64>,
    pub context_payload_bytes: Option<u64>,
    pub estimate_json_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemorySnapshot {
    pub schema_version: u32,
    pub enabled: bool,
    pub started_at_ms: u64,
    pub sampled_at_ms: u64,
    pub process: ProcessMemory,
    pub peak_sampled_private_bytes: Option<u64>,
    pub peak_sampled_resident_bytes: Option<u64>,
    pub tracked_session_count: usize,
    pub resident_session_count: usize,
    pub dropped_session_gauges: u64,
    pub dropped_phase_samples: u64,
    pub oldest_phase_sequence: Option<u64>,
    pub latest_phase_sequence: u64,
    pub phase_gap: bool,
    pub components: Vec<ComponentMemory>,
    pub sessions: Vec<SessionMemory>,
    pub phases: Vec<PhaseSample>,
}

#[derive(Debug)]
struct State {
    enabled: bool,
    started_at_ms: u64,
    next_sequence: u64,
    peak_sampled_private_bytes: Option<u64>,
    peak_sampled_resident_bytes: Option<u64>,
    dropped_session_gauges: u64,
    dropped_phase_samples: u64,
    sessions: BTreeMap<String, SessionMemory>,
    components: BTreeMap<String, ComponentMemory>,
    phases: VecDeque<PhaseSample>,
}

/// Process-wide monitor. All retained state is bounded and contains metadata
/// only; phase samples never include prompts, tool output, or image payloads.
#[derive(Debug)]
pub struct MemoryMonitor {
    enabled: AtomicBool,
    state: Mutex<State>,
}

impl Default for MemoryMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryMonitor {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            state: Mutex::new(State {
                enabled: false,
                started_at_ms: now_ms(),
                next_sequence: 1,
                peak_sampled_private_bytes: None,
                peak_sampled_resident_bytes: None,
                dropped_session_gauges: 0,
                dropped_phase_samples: 0,
                sessions: BTreeMap::new(),
                components: BTreeMap::new(),
                phases: VecDeque::with_capacity(MAX_PHASE_SAMPLES),
            }),
        }
    }

    /// Start a fresh measurement window, preserving current live gauges.
    pub fn start(&self) {
        let mut state = self.lock();
        state.enabled = true;
        state.started_at_ms = now_ms();
        state.peak_sampled_private_bytes = None;
        state.peak_sampled_resident_bytes = None;
        state.phases.clear();
        state.next_sequence = 1;
        state.dropped_phase_samples = 0;
        self.enabled.store(true, Ordering::Release);
    }

    /// Stop recording phase samples. Snapshot reads remain available.
    pub fn stop(&self) {
        self.enabled.store(false, Ordering::Release);
        self.lock().enabled = false;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn started_at_ms(&self) -> u64 {
        self.lock().started_at_ms
    }

    pub fn update_session(&self, mut gauge: SessionMemory) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.lock();
        if !state.enabled {
            return;
        }
        gauge.updated_at_ms = now_ms();
        gauge.last_phase_kind = PhaseKind::classify(&gauge.last_phase);
        if !state.sessions.contains_key(&gauge.session_id)
            && state.sessions.len() >= MAX_SESSION_GAUGES
        {
            if let Some(oldest) = state
                .sessions
                .iter()
                .min_by_key(|(_, item)| item.updated_at_ms)
                .map(|(key, _)| key.clone())
            {
                state.sessions.remove(&oldest);
                state.dropped_session_gauges = state.dropped_session_gauges.saturating_add(1);
            }
        }
        state.sessions.insert(gauge.session_id.clone(), gauge);
    }

    pub fn mark_session_unloaded(&self, session_id: &str) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.lock();
        if !state.enabled {
            return;
        }
        if let Some(gauge) = state.sessions.get_mut(session_id) {
            gauge.resident = false;
            gauge.message_count = 0;
            gauge.turn_count = 0;
            gauge.content_block_count = 0;
            gauge.text_bytes = 0;
            gauge.image_bytes = 0;
            gauge.store_heap_estimate_bytes = 0;
            gauge.agent_aux_heap_estimate_bytes = 0;
            gauge.pending_persist_ops = 0;
            gauge.context_message_count = 0;
            gauge.context_payload_bytes = 0;
            gauge.estimate_json_bytes = 0;
            gauge.last_phase = "session.unloaded".into();
            gauge.last_phase_kind = PhaseKind::classify("session.unloaded");
            gauge.updated_at_ms = now_ms();
        }
    }

    pub fn update_estimate_json_bytes(&self, session_id: &str, estimate_json_bytes: u64) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.lock();
        if !state.enabled {
            return;
        }
        if let Some(gauge) = state.sessions.get_mut(session_id) {
            gauge.estimate_json_bytes = estimate_json_bytes;
            gauge.updated_at_ms = now_ms();
        }
    }

    pub fn update_component(&self, mut gauge: ComponentMemory) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.lock();
        if !state.enabled {
            return;
        }
        gauge.updated_at_ms = now_ms();
        gauge.group = ComponentGroup::classify(&gauge.name);
        if !state.components.contains_key(&gauge.name)
            && state.components.len() >= MAX_COMPONENT_GAUGES
        {
            if let Some(oldest) = state
                .components
                .iter()
                .min_by_key(|(_, item)| item.updated_at_ms)
                .map(|(key, _)| key.clone())
            {
                state.components.remove(&oldest);
            }
        }
        state.components.insert(gauge.name.clone(), gauge);
    }

    pub fn record_phase(
        &self,
        phase: &str,
        session_id: Option<&str>,
        store_heap_estimate_bytes: Option<u64>,
        context_payload_bytes: Option<u64>,
        estimate_json_bytes: Option<u64>,
    ) {
        if !self.is_enabled() {
            return;
        }
        let process = capture_process_memory();
        let mut state = self.lock();
        if !state.enabled {
            return;
        }
        update_peaks(&mut state, &process);
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        if state.phases.len() == MAX_PHASE_SAMPLES {
            state.phases.pop_front();
            state.dropped_phase_samples = state.dropped_phase_samples.saturating_add(1);
        }
        state.phases.push_back(PhaseSample {
            sequence,
            at_ms: now_ms(),
            phase: phase.to_string(),
            kind: PhaseKind::classify(phase),
            session_id: session_id.map(str::to_string),
            process,
            store_heap_estimate_bytes,
            context_payload_bytes,
            estimate_json_bytes,
        });
        if let Some(session_id) = session_id
            && let Some(gauge) = state.sessions.get_mut(session_id)
        {
            gauge.last_phase = phase.to_string();
            gauge.updated_at_ms = now_ms();
        }
    }

    pub fn snapshot(&self) -> MemorySnapshot {
        self.snapshot_since(None)
    }

    /// Return phase rows newer than `after_sequence`; `None` returns the full
    /// bounded ring. This keeps UI polling responses incremental after the
    /// initial snapshot.
    pub fn snapshot_since(&self, after_sequence: Option<u64>) -> MemorySnapshot {
        let process = capture_process_memory();
        let mut state = self.lock();
        if state.enabled {
            update_peaks(&mut state, &process);
        }
        let resident_session_count = state.sessions.values().filter(|s| s.resident).count();
        let mut sessions = state.sessions.values().cloned().collect::<Vec<_>>();
        sessions.sort_by(|a, b| {
            let a_bytes = a
                .store_heap_estimate_bytes
                .saturating_add(a.agent_aux_heap_estimate_bytes)
                .saturating_add(a.context_payload_bytes);
            let b_bytes = b
                .store_heap_estimate_bytes
                .saturating_add(b.agent_aux_heap_estimate_bytes)
                .saturating_add(b.context_payload_bytes);
            b_bytes.cmp(&a_bytes)
        });
        let oldest_phase_sequence = state.phases.front().map(|phase| phase.sequence);
        let latest_phase_sequence = state
            .phases
            .back()
            .map(|phase| phase.sequence)
            .unwrap_or_else(|| state.next_sequence.saturating_sub(1));
        let phase_gap = after_sequence.is_some_and(|after| {
            oldest_phase_sequence.is_some_and(|oldest| after.saturating_add(1) < oldest)
        });
        let phases = state
            .phases
            .iter()
            .filter(|phase| after_sequence.is_none_or(|after| phase.sequence > after))
            .cloned()
            .collect();
        MemorySnapshot {
            schema_version: 1,
            enabled: state.enabled,
            started_at_ms: state.started_at_ms,
            sampled_at_ms: now_ms(),
            process,
            peak_sampled_private_bytes: state.peak_sampled_private_bytes,
            peak_sampled_resident_bytes: state.peak_sampled_resident_bytes,
            tracked_session_count: state.sessions.len(),
            resident_session_count,
            dropped_session_gauges: state.dropped_session_gauges,
            dropped_phase_samples: state.dropped_phase_samples,
            oldest_phase_sequence,
            latest_phase_sequence,
            phase_gap,
            components: state.components.values().cloned().collect(),
            sessions,
            phases,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub fn global() -> &'static MemoryMonitor {
    static MONITOR: OnceLock<MemoryMonitor> = OnceLock::new();
    MONITOR.get_or_init(MemoryMonitor::new)
}

fn update_peaks(state: &mut State, process: &ProcessMemory) {
    if let Some(value) = process.private_bytes {
        state.peak_sampled_private_bytes =
            Some(state.peak_sampled_private_bytes.unwrap_or(0).max(value));
    }
    if let Some(value) = process.resident_bytes {
        state.peak_sampled_resident_bytes =
            Some(state.peak_sampled_resident_bytes.unwrap_or(0).max(value));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(windows)]
fn capture_process_memory() -> ProcessMemory {
    #[repr(C)]
    struct ProcessMemoryCountersEx {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut ProcessMemoryCountersEx,
            size: u32,
        ) -> i32;
    }

    let mut counters = ProcessMemoryCountersEx {
        cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
        private_usage: 0,
    };
    // SAFETY: GetCurrentProcess returns a pseudo-handle; the output struct has
    // the exact PROCESS_MEMORY_COUNTERS_EX C layout and is writable.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return ProcessMemory {
            source: "windows.psapi",
            error: Some(std::io::Error::last_os_error().to_string()),
            ..ProcessMemory::default()
        };
    }
    ProcessMemory {
        resident_bytes: Some(counters.working_set_size as u64),
        private_bytes: Some(counters.private_usage as u64),
        peak_resident_bytes: Some(counters.peak_working_set_size as u64),
        source: "windows.psapi",
        ..ProcessMemory::default()
    }
}

#[cfg(target_os = "linux")]
fn capture_process_memory() -> ProcessMemory {
    fn status_kib(key: &str) -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status.lines().find_map(|line| {
            let (name, rest) = line.split_once(':')?;
            if name != key {
                return None;
            }
            rest.split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .map(|kib| kib.saturating_mul(1024))
        })
    }
    fn private_kib() -> Option<u64> {
        let content = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
        let mut total = 0_u64;
        let mut found = false;
        for line in content.lines() {
            let Some((name, rest)) = line.split_once(':') else {
                continue;
            };
            if matches!(name, "Private_Clean" | "Private_Dirty" | "Private_Hugetlb")
                && let Some(kib) = rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
            {
                total = total.saturating_add(kib.saturating_mul(1024));
                found = true;
            }
        }
        found.then_some(total)
    }
    ProcessMemory {
        resident_bytes: status_kib("VmRSS"),
        private_bytes: private_kib(),
        virtual_bytes: status_kib("VmSize"),
        peak_resident_bytes: status_kib("VmHWM"),
        source: "linux.procfs",
        error: None,
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn capture_process_memory() -> ProcessMemory {
    ProcessMemory {
        source: "unsupported",
        error: Some("process memory counters are not implemented for this target".into()),
        ..ProcessMemory::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_group_follows_name_prefix() {
        assert_eq!(
            ComponentGroup::classify("ringing.timeline.appender"),
            ComponentGroup::Ringing
        );
        assert_eq!(
            ComponentGroup::classify("agent_registry.workers"),
            ComponentGroup::AgentRegistry
        );
        assert_eq!(
            ComponentGroup::classify("service.task_stores"),
            ComponentGroup::Service
        );
        assert_eq!(
            ComponentGroup::classify("mcp.connections"),
            ComponentGroup::Transport
        );
        assert_eq!(
            ComponentGroup::classify("lsp.connections"),
            ComponentGroup::Transport
        );
        assert_eq!(
            ComponentGroup::classify("workspace.image_registry"),
            ComponentGroup::Workspace
        );
        assert_eq!(ComponentGroup::classify("other"), ComponentGroup::Other);
    }

    #[test]
    fn phase_kind_classifies_the_lifecycle() {
        assert_eq!(
            PhaseKind::classify("session.resume.begin"),
            PhaseKind::SessionResume
        );
        assert_eq!(
            PhaseKind::classify("context.build.end"),
            PhaseKind::ContextBuild
        );
        assert_eq!(
            PhaseKind::classify("context.estimate.serialized"),
            PhaseKind::TokenPreflight
        );
        assert_eq!(
            PhaseKind::classify("gate.request.begin"),
            PhaseKind::ProviderRequest
        );
        assert_eq!(
            PhaseKind::classify("compact.prompt.build.end"),
            PhaseKind::Compact
        );
        assert_eq!(
            PhaseKind::classify("request_log.value_built"),
            PhaseKind::RequestLog
        );
        assert_eq!(PhaseKind::classify("weird"), PhaseKind::Other);
    }

    #[test]
    fn snapshot_since_returns_only_new_phases() {
        let monitor = MemoryMonitor::new();
        monitor.start();
        monitor.record_phase("context.build.begin", Some("s1"), None, None, None);
        monitor.record_phase("context.build.end", Some("s1"), Some(10), Some(20), None);
        let first = monitor.snapshot();
        assert_eq!(first.phases.len(), 2);
        assert_eq!(first.latest_phase_sequence, 2);
        assert!(!first.phase_gap);
        assert_eq!(first.phases[1].kind, PhaseKind::ContextBuild);

        let idle = monitor.snapshot_since(Some(first.latest_phase_sequence));
        assert!(idle.phases.is_empty());
        assert_eq!(idle.latest_phase_sequence, 2);

        monitor.record_phase("gate.request.begin", Some("s1"), None, None, None);
        let delta = monitor.snapshot_since(Some(2));
        assert_eq!(delta.phases.len(), 1);
        assert_eq!(delta.phases[0].kind, PhaseKind::ProviderRequest);
    }

    #[test]
    fn component_group_is_stamped_on_update() {
        let monitor = MemoryMonitor::new();
        monitor.start();
        monitor.update_component(ComponentMemory {
            name: "ringing.timeline.appender".into(),
            ..Default::default()
        });
        let snapshot = monitor.snapshot();
        let component = snapshot
            .components
            .iter()
            .find(|c| c.name == "ringing.timeline.appender")
            .expect("component present");
        assert_eq!(component.group, ComponentGroup::Ringing);
    }

    #[test]
    fn unloading_a_session_zeroes_its_gauges() {
        let monitor = MemoryMonitor::new();
        monitor.start();
        monitor.update_session(SessionMemory {
            session_id: "s1".into(),
            resident: true,
            message_count: 4,
            text_bytes: 999,
            image_bytes: 128,
            ..Default::default()
        });
        monitor.mark_session_unloaded("s1");
        let snapshot = monitor.snapshot();
        let session = snapshot
            .sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("session present");
        assert!(!session.resident);
        assert_eq!(session.message_count, 0);
        assert_eq!(session.text_bytes, 0);
        assert_eq!(session.image_bytes, 0);
        assert_eq!(session.last_phase, "session.unloaded");
        assert_eq!(session.last_phase_kind, PhaseKind::SessionResume);
    }
}
