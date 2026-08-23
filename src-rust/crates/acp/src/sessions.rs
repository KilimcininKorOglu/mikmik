//! Per-session state for the ACP server.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use dashmap::DashMap;
use mikmik_core::types::Message;
use mikmik_tools::PendingPermissionStore;
use tokio_util::sync::CancellationToken;

/// What the connected client has changed for this session alone.
///
/// Every field is an override on top of the runtime's own configuration, and
/// none of it is written to `settings.json`: a choice made in an editor panel
/// belongs to that panel's session, not to the user's next terminal run.
#[derive(Debug, Clone, Default)]
pub struct SessionSettings {
    pub permission_mode: Option<mikmik_core::PermissionMode>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub effort: Option<mikmik_core::effort::EffortLevel>,
}

/// One ACP session — a logical conversation with its own cwd, transcript,
/// MCP server roster, and cancellation token.
pub struct SessionState {
    pub session_id: acp::SessionId,
    /// Where this session works. A session can be re-homed to another
    /// worktree of the same project without being started again.
    pub cwd: parking_lot::Mutex<PathBuf>,
    pub messages: parking_lot::Mutex<Vec<Message>>,
    /// The token the current turn is driven by. It is replaced at the start of
    /// every turn, because a cancelled token stays cancelled: keeping one for
    /// the session's lifetime would make a single `session/cancel` abort every
    /// later prompt on that session.
    cancel_token: parking_lot::Mutex<CancellationToken>,
    pub pending_permissions: Arc<parking_lot::Mutex<PendingPermissionStore>>,
    pub file_history: Arc<parking_lot::Mutex<mikmik_core::file_history::FileHistory>>,
    /// What this session has read from each file. Per session, like the
    /// history beside it, so one editor session's reads never authorise
    /// another's edits.
    pub file_snapshots: Arc<parking_lot::Mutex<mikmik_core::file_snapshot::FileSnapshotStore>>,
    pub current_turn: Arc<std::sync::atomic::AtomicUsize>,
    pub settings: parking_lot::Mutex<SessionSettings>,
    /// Human-readable name, shown by anything that lists sessions.
    pub title: parking_lot::Mutex<Option<String>>,
    /// When the session first existed. Kept across saves so a reloaded
    /// session does not claim to have been created on the reload.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// The session this one was forked from, and the message it split at.
    pub forked_from: Option<(String, usize)>,
    /// What this session spent. One agent process now serves many editor
    /// panels, so a tracker shared across sessions would make every panel
    /// report the whole process.
    pub cost_tracker: Arc<mikmik_core::CostTracker>,
    /// Whether a turn is running. Two prompts on one session would each clone
    /// the transcript, run against it, and write their own copy back, so the
    /// second one to finish would erase the first.
    turn_in_flight: AtomicBool,
    /// The MCP servers this session was opened with, and the tools they added.
    /// `None` when the client named none, in which case the session runs with
    /// the agent's own roster.
    pub mcp: parking_lot::Mutex<Option<crate::mcp::SessionMcp>>,
}

/// Proof that this turn owns the session, and the token it is driven by.
///
/// Dropping it frees the session for the next prompt, including on the error
/// paths, so a turn that fails cannot leave the session unusable.
pub struct TurnGuard {
    session: Arc<SessionState>,
    token: CancellationToken,
}

impl TurnGuard {
    /// The cancellation token this turn was started with.
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.session.turn_in_flight.store(false, Ordering::SeqCst);
    }
}

impl SessionState {
    pub fn new(session_id: acp::SessionId, cwd: PathBuf) -> Arc<Self> {
        Self::build(session_id, cwd, Vec::new(), None, chrono::Utc::now(), None)
    }

    /// Rebuild a session from what was stored, keeping its identity: the same
    /// id, the same creation time, and the transcript it had.
    pub fn restored(
        session_id: acp::SessionId,
        cwd: PathBuf,
        stored: &mikmik_core::history::ConversationSession,
    ) -> Arc<Self> {
        Self::build(
            session_id,
            cwd,
            stored.messages.clone(),
            stored.title.clone(),
            stored.created_at,
            None,
        )
    }

    /// Start a new session carrying a copy of another one's transcript, and
    /// remember where it split so both can be told apart later.
    pub fn forked(
        session_id: acp::SessionId,
        cwd: PathBuf,
        stored: &mikmik_core::history::ConversationSession,
    ) -> Arc<Self> {
        Self::build(
            session_id,
            cwd,
            stored.messages.clone(),
            stored.title.clone(),
            chrono::Utc::now(),
            Some((stored.id.clone(), stored.messages.len())),
        )
    }

    fn build(
        session_id: acp::SessionId,
        cwd: PathBuf,
        messages: Vec<Message>,
        title: Option<String>,
        created_at: chrono::DateTime<chrono::Utc>,
        forked_from: Option<(String, usize)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            cwd: parking_lot::Mutex::new(cwd),
            messages: parking_lot::Mutex::new(messages),
            cancel_token: parking_lot::Mutex::new(CancellationToken::new()),
            pending_permissions: Arc::new(parking_lot::Mutex::new(
                PendingPermissionStore::default(),
            )),
            file_history: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            settings: parking_lot::Mutex::new(SessionSettings::default()),
            title: parking_lot::Mutex::new(title),
            created_at,
            forked_from,
            cost_tracker: mikmik_core::CostTracker::new(),
            turn_in_flight: AtomicBool::new(false),
            mcp: parking_lot::Mutex::new(None),
        })
    }

    /// Claim the session for a turn, handing out a fresh token for it and
    /// dropping the one the previous turn used. `None` when a turn is already
    /// running.
    pub fn begin_turn(self: &Arc<Self>) -> Option<TurnGuard> {
        self.turn_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        let token = CancellationToken::new();
        *self.cancel_token.lock() = token.clone();
        Some(TurnGuard {
            session: Arc::clone(self),
            token,
        })
    }

    /// Cancel whatever turn is running. A session with no turn in flight is
    /// unaffected once `begin_turn` replaces the token.
    pub fn cancel(&self) {
        self.cancel_token.lock().cancel();
    }

    /// Whether the token the current turn holds has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.lock().is_cancelled()
    }
}

/// Map of active sessions keyed by ACP session id.
#[derive(Default)]
pub struct SessionRegistry {
    inner: DashMap<acp::SessionId, Arc<SessionState>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, state: Arc<SessionState>) {
        self.inner.insert(state.session_id.clone(), state);
    }

    pub fn get(&self, id: &acp::SessionId) -> Option<Arc<SessionState>> {
        self.inner.get(id).map(|r| r.value().clone())
    }

    pub fn remove(&self, id: &acp::SessionId) -> Option<Arc<SessionState>> {
        self.inner.remove(id).map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_session_starts_with_nothing_recorded() {
        let id = acp::SessionId::new("session-1");
        let cwd = PathBuf::from("/tmp/mikmik-test");
        let state = SessionState::new(id.clone(), cwd.clone());

        assert_eq!(state.session_id, id);
        assert_eq!(*state.cwd.lock(), cwd);
        assert!(state.messages.lock().is_empty());
        assert_eq!(
            state.current_turn.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        // A fresh token, so a cancelled predecessor cannot abort this session.
        assert!(!state.is_cancelled());
    }

    #[test]
    fn two_sessions_cancel_independently() {
        let first = SessionState::new(acp::SessionId::new("a"), PathBuf::from("/tmp/a"));
        let second = SessionState::new(acp::SessionId::new("b"), PathBuf::from("/tmp/b"));

        first.cancel();

        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
    }

    #[test]
    fn a_cancelled_session_runs_again_on_the_next_turn() {
        let state = SessionState::new(acp::SessionId::new("a"), PathBuf::from("/tmp/a"));

        let first = state.begin_turn().expect("nothing was running");
        state.cancel();
        assert!(
            first.token().is_cancelled(),
            "the running turn was cancelled"
        );
        drop(first);

        // The next prompt must not inherit that verdict.
        let second = state.begin_turn().expect("the first turn ended");
        assert!(!second.token().is_cancelled());
        assert!(!state.is_cancelled());
    }

    #[test]
    fn cancelling_only_reaches_the_turn_that_is_running() {
        let state = SessionState::new(acp::SessionId::new("a"), PathBuf::from("/tmp/a"));

        let stale = state.begin_turn().expect("nothing was running");
        let stale_token = stale.token().clone();
        drop(stale);

        let current = state.begin_turn().expect("the first turn ended");
        state.cancel();

        assert!(current.token().is_cancelled());
        assert!(
            !stale_token.is_cancelled(),
            "the replaced token was left alone"
        );
    }

    #[test]
    fn a_second_prompt_is_refused_while_one_is_running() {
        let state = SessionState::new(acp::SessionId::new("a"), PathBuf::from("/tmp/a"));

        let running = state.begin_turn().expect("nothing was running");
        assert!(
            state.begin_turn().is_none(),
            "two turns would each write their own copy of the transcript"
        );

        drop(running);
        assert!(
            state.begin_turn().is_some(),
            "the session stayed claimed after its turn ended"
        );
    }

    #[test]
    fn a_turn_that_ends_badly_still_frees_the_session() {
        let state = SessionState::new(acp::SessionId::new("a"), PathBuf::from("/tmp/a"));

        // Whatever the turn returns, the guard is dropped on the way out.
        let failed: Result<(), ()> = {
            let _guard = state.begin_turn().expect("nothing was running");
            Err(())
        };

        assert!(failed.is_err());
        assert!(state.begin_turn().is_some());
    }

    #[test]
    fn each_session_counts_only_what_it_spent() {
        let first = SessionState::new(acp::SessionId::new("a"), PathBuf::from("/tmp/a"));
        let second = SessionState::new(acp::SessionId::new("b"), PathBuf::from("/tmp/b"));

        first.cost_tracker.add_usage(
            "m",
            mikmik_core::cost::ModelPricing::for_model("m"),
            100,
            20,
            0,
            0,
        );

        assert_eq!(first.cost_tracker.input_tokens(), 100);
        assert_eq!(
            second.cost_tracker.input_tokens(),
            0,
            "one panel's spending reached another"
        );
    }

    #[test]
    fn a_session_survives_a_round_trip_through_the_registry() {
        let registry = SessionRegistry::new();
        let id = acp::SessionId::new("session-2");
        let state = SessionState::new(id.clone(), PathBuf::from("/tmp"));

        assert!(registry.get(&id).is_none());
        registry.insert(Arc::clone(&state));

        let fetched = registry.get(&id).expect("present after insert");
        assert!(
            Arc::ptr_eq(&fetched, &state),
            "the registry cloned the state"
        );

        let removed = registry.remove(&id).expect("present before remove");
        assert!(Arc::ptr_eq(&removed, &state));
        assert!(registry.get(&id).is_none());
    }

    #[test]
    fn removing_an_unknown_session_is_not_an_error() {
        let registry = SessionRegistry::new();
        assert!(registry.remove(&acp::SessionId::new("missing")).is_none());
    }
}
