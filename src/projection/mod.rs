use crate::conversation::{Conversation, Turn};
use crate::error::{ERR_BUSY, ERR_NO_PREVIOUS};
use crate::ids;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

const PENDING_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct ProjectionTree {
    inner: Arc<Mutex<ProjectionTreeState>>,
    max_sessions: usize,
    pending_ttl: Duration,
}

#[derive(Debug)]
struct ProjectionTreeState {
    sessions: HashMap<String, ProjectedShellSession>,
    next_lease_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Pending,
    Active,
    Closing,
}

#[derive(Debug)]
struct ProjectedShellSession {
    parent: Option<String>,
    children: HashSet<String>,
    status: SessionStatus,
    pending_since: Instant,
    conversation: Option<Conversation>,
    in_flight: Option<u64>,
}

impl ProjectedShellSession {
    fn pending(parent: Option<String>) -> Self {
        Self {
            parent,
            children: HashSet::new(),
            status: SessionStatus::Pending,
            pending_since: Instant::now(),
            conversation: None,
            in_flight: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskMode {
    New,
    Continue,
}

#[derive(Debug)]
pub enum BeginAsk {
    Replay(String),
    Lease(AskLease),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    InvalidSessionId,
    DuplicateSessionId,
    SessionLimit,
    NotFound,
    NotActive,
    Closing,
    Busy,
    NoPreviousConversation,
    RequestIdConflict,
    OnlyPendingCanCancel,
    Conversation(String),
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId => f.write_str("invalid session id"),
            Self::DuplicateSessionId => f.write_str("duplicate session id"),
            Self::SessionLimit => f.write_str("Projected Shell Session limit reached."),
            Self::NotFound => f.write_str("Projected Shell Session not found."),
            Self::NotActive => f.write_str("Projected Shell Session is not active."),
            Self::Closing => f.write_str("Projected Shell Session is closing."),
            Self::Busy => f.write_str(ERR_BUSY),
            Self::NoPreviousConversation => f.write_str(ERR_NO_PREVIOUS),
            Self::RequestIdConflict => {
                f.write_str("request_id was already used with different input.")
            }
            Self::OnlyPendingCanCancel => f.write_str("Only Pending Sessions can be cancelled."),
            Self::Conversation(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ProjectionError {}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionSnapshot {
    sessions: Vec<SessionSnapshot>,
}

#[cfg(test)]
impl ProjectionSnapshot {
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.session(session_id).is_some()
    }

    pub fn session(&self, session_id: &str) -> Option<&SessionSnapshot> {
        self.sessions
            .iter()
            .find(|session| session.id == session_id)
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub id: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub status: SessionStatus,
    pub has_lease: bool,
    pub conversation_turns: usize,
}

impl ProjectionTree {
    pub fn new(max_sessions: usize) -> Self {
        Self::with_pending_ttl(max_sessions, PENDING_TTL)
    }

    fn with_pending_ttl(max_sessions: usize, pending_ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ProjectionTreeState {
                sessions: HashMap::new(),
                next_lease_id: 1,
            })),
            max_sessions,
            pending_ttl,
        }
    }

    pub async fn create_root(&self, session_id: String) -> Result<(), ProjectionError> {
        validate_session_id(&session_id)?;
        let mut state = self.inner.lock().await;
        expire_pending(&mut state, self.pending_ttl);
        if state.sessions.len() >= self.max_sessions {
            return Err(ProjectionError::SessionLimit);
        }
        if state.sessions.contains_key(&session_id) {
            return Err(ProjectionError::DuplicateSessionId);
        }
        state
            .sessions
            .insert(session_id.clone(), ProjectedShellSession::pending(None));
        drop(state);
        self.schedule_pending_expiry(session_id);
        Ok(())
    }

    pub async fn create_child(
        &self,
        parent_id: &str,
        child_id: String,
    ) -> Result<(), ProjectionError> {
        validate_session_id(&child_id)?;
        let mut state = self.inner.lock().await;
        expire_pending(&mut state, self.pending_ttl);
        if state.sessions.len() >= self.max_sessions {
            return Err(ProjectionError::SessionLimit);
        }
        if state.sessions.contains_key(&child_id) {
            return Err(ProjectionError::DuplicateSessionId);
        }
        let parent = state
            .sessions
            .get_mut(parent_id)
            .ok_or(ProjectionError::NotFound)?;
        if parent.status != SessionStatus::Active {
            return Err(ProjectionError::NotActive);
        }
        parent.children.insert(child_id.clone());
        state.sessions.insert(
            child_id.clone(),
            ProjectedShellSession::pending(Some(parent_id.to_string())),
        );
        drop(state);
        self.schedule_pending_expiry(child_id);
        Ok(())
    }

    pub async fn activate(&self, session_id: &str) -> Result<(), ProjectionError> {
        let mut state = self.inner.lock().await;
        expire_pending(&mut state, self.pending_ttl);
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or(ProjectionError::NotFound)?;
        match session.status {
            SessionStatus::Pending | SessionStatus::Active => {
                session.status = SessionStatus::Active;
                Ok(())
            }
            SessionStatus::Closing => Err(ProjectionError::Closing),
        }
    }

    pub async fn unregister(&self, session_id: &str) {
        let mut state = self.inner.lock().await;
        expire_pending(&mut state, self.pending_ttl);
        let Some(session) = state.sessions.get_mut(session_id) else {
            return;
        };
        if session.in_flight.is_some() {
            session.status = SessionStatus::Closing;
        } else {
            remove_session(&mut state, session_id);
        }
    }

    pub async fn cancel_pending(&self, session_id: &str) -> Result<(), ProjectionError> {
        let mut state = self.inner.lock().await;
        expire_pending(&mut state, self.pending_ttl);
        let Some(session) = state.sessions.get(session_id) else {
            return Ok(());
        };
        if session.status != SessionStatus::Pending {
            return Err(ProjectionError::OnlyPendingCanCancel);
        }
        remove_session(&mut state, session_id);
        Ok(())
    }

    pub async fn begin_ask(
        &self,
        session_id: &str,
        mode: AskMode,
        request_id: &str,
        request_digest: &str,
    ) -> Result<BeginAsk, ProjectionError> {
        let mut state = self.inner.lock().await;
        expire_pending(&mut state, self.pending_ttl);
        let lease_id = state.next_lease_id;
        state.next_lease_id = state.next_lease_id.wrapping_add(1).max(1);
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or(ProjectionError::NotFound)?;
        if session.status != SessionStatus::Active {
            return Err(ProjectionError::NotActive);
        }
        if let Some(conversation) = &session.conversation {
            match conversation.answer_for(request_id, request_digest) {
                Ok(Some(answer)) => return Ok(BeginAsk::Replay(answer)),
                Err(_) => return Err(ProjectionError::RequestIdConflict),
                Ok(None) => {}
            }
        }
        let conversation = match mode {
            AskMode::New => Conversation::default(),
            AskMode::Continue => session
                .conversation
                .clone()
                .ok_or(ProjectionError::NoPreviousConversation)?,
        };
        if session.in_flight.is_some() {
            return Err(ProjectionError::Busy);
        }
        session.in_flight = Some(lease_id);
        Ok(BeginAsk::Lease(AskLease {
            tree: self.clone(),
            session_id: session_id.to_string(),
            lease_id,
            conversation: Some(conversation),
            finished: false,
        }))
    }

    #[cfg(test)]
    pub async fn snapshot(&self) -> ProjectionSnapshot {
        let mut state = self.inner.lock().await;
        expire_pending(&mut state, self.pending_ttl);
        let mut sessions = state
            .sessions
            .iter()
            .map(|(id, session)| {
                let mut children = session.children.iter().cloned().collect::<Vec<_>>();
                children.sort();
                SessionSnapshot {
                    id: id.clone(),
                    parent: session.parent.clone(),
                    children,
                    status: session.status,
                    has_lease: session.in_flight.is_some(),
                    conversation_turns: session
                        .conversation
                        .as_ref()
                        .map_or(0, |conversation| conversation.turns().len()),
                }
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        ProjectionSnapshot { sessions }
    }

    fn schedule_pending_expiry(&self, session_id: String) {
        let tree = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tree.pending_ttl).await;
            let mut state = tree.inner.lock().await;
            let expired = state.sessions.get(&session_id).is_some_and(|session| {
                session.status == SessionStatus::Pending
                    && session.pending_since.elapsed() >= tree.pending_ttl
            });
            if expired {
                remove_session(&mut state, &session_id);
            }
        });
    }

    async fn complete_lease(
        &self,
        session_id: &str,
        lease_id: u64,
        conversation: Option<Conversation>,
    ) {
        let mut state = self.inner.lock().await;
        let remove = if let Some(session) = state.sessions.get_mut(session_id) {
            if session.in_flight != Some(lease_id) {
                return;
            }
            if let Some(conversation) = conversation {
                session.conversation = Some(conversation);
            }
            session.in_flight = None;
            session.status == SessionStatus::Closing
        } else {
            false
        };
        if remove {
            remove_session(&mut state, session_id);
        }
    }
}

#[derive(Debug)]
pub struct AskLease {
    tree: ProjectionTree,
    session_id: String,
    lease_id: u64,
    conversation: Option<Conversation>,
    finished: bool,
}

impl AskLease {
    pub fn conversation(&self) -> &Conversation {
        self.conversation
            .as_ref()
            .expect("unfinished lease always owns a Conversation")
    }

    pub async fn commit(mut self, turn: Turn) -> Result<(), ProjectionError> {
        let mut conversation = self
            .conversation
            .take()
            .expect("unfinished lease always owns a Conversation");
        if let Err(error) = conversation.commit(turn) {
            self.tree
                .complete_lease(&self.session_id, self.lease_id, None)
                .await;
            self.finished = true;
            return Err(ProjectionError::Conversation(error.to_string()));
        }
        self.tree
            .complete_lease(&self.session_id, self.lease_id, Some(conversation))
            .await;
        self.finished = true;
        Ok(())
    }

    pub async fn cancel(mut self) {
        self.tree
            .complete_lease(&self.session_id, self.lease_id, None)
            .await;
        self.finished = true;
    }
}

impl Drop for AskLease {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let tree = self.tree.clone();
        let session_id = self.session_id.clone();
        let lease_id = self.lease_id;
        tokio::spawn(async move {
            tree.complete_lease(&session_id, lease_id, None).await;
        });
    }
}

fn expire_pending(state: &mut ProjectionTreeState, pending_ttl: Duration) {
    let expired = state
        .sessions
        .iter()
        .filter(|(_, session)| {
            session.status == SessionStatus::Pending
                && session.pending_since.elapsed() >= pending_ttl
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in expired {
        remove_session(state, &id);
    }
}

fn remove_session(state: &mut ProjectionTreeState, session_id: &str) {
    let Some(session) = state.sessions.remove(session_id) else {
        return;
    };
    if let Some(parent_id) = session.parent
        && let Some(parent) = state.sessions.get_mut(&parent_id)
    {
        parent.children.remove(session_id);
    }
    for child_id in session.children {
        remove_session(state, &child_id);
    }
}

fn validate_session_id(session_id: &str) -> Result<(), ProjectionError> {
    if ids::is_hex_id(session_id) {
        Ok(())
    } else {
        Err(ProjectionError::InvalidSessionId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{AssistantResponse, UserInput};

    const ROOT: &str = "0123456789abcdef";
    const CHILD: &str = "1111111111111111";
    const GRANDCHILD: &str = "2222222222222222";

    fn turn(request_id: &str, digest: &str, answer: &str) -> Turn {
        Turn {
            request_id: request_id.into(),
            request_digest: digest.into(),
            user: UserInput::new("hello", ""),
            assistant: AssistantResponse::new(answer),
        }
    }

    async fn active_tree(max_sessions: usize) -> ProjectionTree {
        let tree = ProjectionTree::new(max_sessions);
        tree.create_root(ROOT.into()).await.unwrap();
        tree.activate(ROOT).await.unwrap();
        tree
    }

    async fn lease(tree: &ProjectionTree, session_id: &str, request_id: &str) -> AskLease {
        match tree
            .begin_ask(session_id, AskMode::New, request_id, "digest")
            .await
            .unwrap()
        {
            BeginAsk::Lease(lease) => lease,
            BeginAsk::Replay(_) => panic!("expected lease"),
        }
    }

    #[tokio::test]
    async fn pending_activates_and_unregisters() {
        let tree = ProjectionTree::new(1);
        tree.create_root(ROOT.into()).await.unwrap();
        assert_eq!(
            tree.snapshot().await.session(ROOT).unwrap().status,
            SessionStatus::Pending
        );
        tree.activate(ROOT).await.unwrap();
        assert_eq!(
            tree.snapshot().await.session(ROOT).unwrap().status,
            SessionStatus::Active
        );
        tree.unregister(ROOT).await;
        assert!(!tree.snapshot().await.contains(ROOT));
    }

    #[tokio::test]
    async fn active_with_lease_closes_then_is_removed_on_completion() {
        let tree = active_tree(1).await;
        let lease = lease(&tree, ROOT, "aaaaaaaaaaaaaaaa").await;
        tree.unregister(ROOT).await;
        assert_eq!(
            tree.snapshot().await.session(ROOT).unwrap().status,
            SessionStatus::Closing
        );
        lease.cancel().await;
        assert!(!tree.snapshot().await.contains(ROOT));
    }

    #[tokio::test]
    async fn dropped_future_lease_releases_only_its_session() {
        let tree = ProjectionTree::new(2);
        for id in [ROOT, CHILD] {
            tree.create_root(id.into()).await.unwrap();
            tree.activate(id).await.unwrap();
        }
        let first = lease(&tree, ROOT, "aaaaaaaaaaaaaaaa").await;
        let second = lease(&tree, CHILD, "bbbbbbbbbbbbbbbb").await;
        drop(first);
        for _ in 0..20 {
            if !tree.snapshot().await.session(ROOT).unwrap().has_lease {
                break;
            }
            tokio::task::yield_now().await;
        }
        let snapshot = tree.snapshot().await;
        assert!(!snapshot.session(ROOT).unwrap().has_lease);
        assert!(snapshot.session(CHILD).unwrap().has_lease);
        second.cancel().await;
    }

    #[tokio::test]
    async fn parent_removal_recursively_removes_descendants() {
        let tree = active_tree(3).await;
        tree.create_child(ROOT, CHILD.into()).await.unwrap();
        tree.activate(CHILD).await.unwrap();
        tree.create_child(CHILD, GRANDCHILD.into()).await.unwrap();
        tree.unregister(ROOT).await;
        assert_eq!(tree.snapshot().await.len(), 0);
    }

    #[tokio::test]
    async fn stale_lease_cannot_release_a_new_lease_for_the_same_id() {
        let tree = active_tree(3).await;
        tree.create_child(ROOT, CHILD.into()).await.unwrap();
        tree.activate(CHILD).await.unwrap();
        let stale = lease(&tree, CHILD, "aaaaaaaaaaaaaaaa").await;

        tree.unregister(ROOT).await;
        tree.create_root(CHILD.into()).await.unwrap();
        tree.activate(CHILD).await.unwrap();
        let current = lease(&tree, CHILD, "bbbbbbbbbbbbbbbb").await;

        drop(stale);
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(tree.snapshot().await.session(CHILD).unwrap().has_lease);
        current.cancel().await;
    }

    #[tokio::test]
    async fn child_membership_is_removed_from_parent_when_child_is_cancelled() {
        let tree = active_tree(2).await;
        tree.create_child(ROOT, CHILD.into()).await.unwrap();
        assert_eq!(
            tree.snapshot().await.session(ROOT).unwrap().children,
            vec![CHILD.to_string()]
        );
        tree.cancel_pending(CHILD).await.unwrap();
        assert!(
            tree.snapshot()
                .await
                .session(ROOT)
                .unwrap()
                .children
                .is_empty()
        );
    }

    #[tokio::test]
    async fn one_busy_session_does_not_block_another() {
        let tree = ProjectionTree::new(2);
        for id in [ROOT, CHILD] {
            tree.create_root(id.into()).await.unwrap();
            tree.activate(id).await.unwrap();
        }
        let first = lease(&tree, ROOT, "aaaaaaaaaaaaaaaa").await;
        assert!(matches!(
            tree.begin_ask(ROOT, AskMode::New, "bbbbbbbbbbbbbbbb", "digest")
                .await,
            Err(ProjectionError::Busy)
        ));
        let second = lease(&tree, CHILD, "cccccccccccccccc").await;
        first.cancel().await;
        second.cancel().await;
    }

    #[tokio::test]
    async fn replay_and_conflict_do_not_allocate_a_lease() {
        let tree = active_tree(1).await;
        lease(&tree, ROOT, "aaaaaaaaaaaaaaaa")
            .await
            .commit(turn("aaaaaaaaaaaaaaaa", "digest", "answer"))
            .await
            .unwrap();
        assert!(matches!(
            tree.begin_ask(ROOT, AskMode::New, "aaaaaaaaaaaaaaaa", "digest")
                .await
                .unwrap(),
            BeginAsk::Replay(answer) if answer == "answer"
        ));
        assert!(matches!(
            tree.begin_ask(ROOT, AskMode::New, "aaaaaaaaaaaaaaaa", "different")
                .await,
            Err(ProjectionError::RequestIdConflict)
        ));
        assert!(!tree.snapshot().await.session(ROOT).unwrap().has_lease);
    }

    #[tokio::test]
    async fn conversation_is_owned_per_projected_shell_session() {
        let tree = ProjectionTree::new(2);
        for id in [ROOT, CHILD] {
            tree.create_root(id.into()).await.unwrap();
            tree.activate(id).await.unwrap();
        }
        lease(&tree, ROOT, "aaaaaaaaaaaaaaaa")
            .await
            .commit(turn("aaaaaaaaaaaaaaaa", "digest", "root answer"))
            .await
            .unwrap();
        assert!(matches!(
            tree.begin_ask(ROOT, AskMode::Continue, "bbbbbbbbbbbbbbbb", "next")
                .await,
            Ok(BeginAsk::Lease(_))
        ));
        assert!(matches!(
            tree.begin_ask(CHILD, AskMode::Continue, "cccccccccccccccc", "next")
                .await,
            Err(ProjectionError::NoPreviousConversation)
        ));
    }

    #[tokio::test]
    async fn pending_expiry_removes_session_and_membership_without_sleep() {
        let tree = ProjectionTree::with_pending_ttl(2, Duration::ZERO);
        tree.create_root(ROOT.into()).await.unwrap();
        tree.create_root(CHILD.into()).await.unwrap();
        assert_eq!(tree.snapshot().await.len(), 0);
    }

    #[tokio::test]
    async fn session_limit_counts_the_whole_projection_tree() {
        let tree = active_tree(2).await;
        tree.create_child(ROOT, CHILD.into()).await.unwrap();
        assert_eq!(
            tree.create_child(ROOT, GRANDCHILD.into()).await,
            Err(ProjectionError::SessionLimit)
        );
    }
}
