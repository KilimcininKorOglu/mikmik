// SendMessageTool: send a message to another agent running in this process.
//
// Delivery is in-process only: every agent gets an inbox keyed by its address,
// and the turn loop drains its own inbox at each turn boundary. Two agents in
// two separate processes cannot reach each other.
//
// Addressing exists because a sub-agent shares its parent's session id (see
// the assertion in `agent_tool.rs`), so the session id alone names a pair of
// agents rather than one. An address is therefore `{session_id}:{name}`, with
// the top-level session addressed by its bare session id under the reserved
// name `main`.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// In-process inbox
// ---------------------------------------------------------------------------

/// A single message in the inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub from: String,
    pub to: String,
    pub content: String,
    pub timestamp: u64,
}

/// How much of a message the tool result echoes back to the sender.
const PREVIEW_CHARS: usize = 60;

/// The short name the top-level session answers to.
pub const MAIN_NAME: &str = "main";

/// Resolves to the address of whoever spawned this agent.
const PARENT_ALIAS: &str = "parent";

/// Reaches every other live agent in the same session.
const BROADCAST: &str = "*";

/// How many undelivered messages one inbox holds before it refuses more. A
/// recipient that never takes another turn must not let a sender fill memory.
const MAX_INBOX_MESSAGES: usize = 32;

/// How much of one message survives. The text lands in the recipient's
/// context window, so an unbounded message is an unbounded context cost.
const MAX_MESSAGE_CHARS: usize = 4_000;

/// Longest short name a sub-agent may claim.
const MAX_NAME_CHARS: usize = 24;

/// The name a sub-agent falls back to when nothing usable was supplied.
const FALLBACK_NAME: &str = "agent";

/// Global inbox: address → queued messages.
static INBOX: Lazy<DashMap<String, Vec<AgentMessage>>> = Lazy::new(DashMap::new);

/// Every agent currently able to receive: address → who it is.
static LIVE: Lazy<DashMap<String, LiveAgent>> = Lazy::new(DashMap::new);

/// One registered agent, as the address book sees it.
#[derive(Debug, Clone)]
struct LiveAgent {
    session_id: String,
    name: String,
}

/// Keeps an agent addressable for as long as it is held.
///
/// Dropping it removes both the address book entry and any message that was
/// never collected. `Drop` rather than an explicit call because an agent can
/// leave through a normal return, an error, or a cancellation, and only one of
/// those three runs code the spawn site controls.
pub struct InboxGuard {
    key: String,
}

impl InboxGuard {
    /// The address this registration holds, so a caller does not rebuild it
    /// from parts and risk disagreeing with the address book.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for InboxGuard {
    fn drop(&mut self) {
        LIVE.remove(&self.key);
        INBOX.remove(&self.key);
    }
}

/// This agent's place in the address book, carried on its `ToolContext`.
///
/// A default value is deliberately unaddressable: a context built outside a
/// query loop can neither send nor receive, and says so rather than writing
/// into a mailbox nothing drains.
#[derive(Debug, Clone, Default)]
pub struct AgentAddress {
    /// This agent's own inbox key. `{session_id}` for a top-level session,
    /// `{session_id}:{name}` for a sub-agent.
    pub own: String,
    /// What `parent` resolves to. `None` on a top-level session.
    pub parent: Option<String>,
    /// The short name other agents address this one by.
    pub name: Option<String>,
    /// Whether the parent is waiting inside the call that spawned this agent.
    /// A blocked parent takes no turn until this agent finishes, so it cannot
    /// answer; the message still arrives, only later.
    pub parent_blocked: bool,
}

/// Register the top-level session, which is addressed by its bare session id.
pub fn register_main(session_id: &str) -> InboxGuard {
    claim(session_id.to_string(), session_id, MAIN_NAME)
}

/// Claim a unique short name under `session_id` and register an inbox for it.
///
/// Returns the name that was actually claimed, which differs from `requested`
/// when another live agent already holds it. Claiming and naming are one step
/// so two agents spawned at once cannot settle on the same name.
pub fn register_named(
    session_id: &str,
    requested: Option<&str>,
    description: &str,
) -> (String, InboxGuard) {
    let base = slug(requested.unwrap_or(description));

    for attempt in 1..=u32::MAX {
        let candidate = if attempt == 1 {
            base.clone()
        } else {
            format!("{}-{}", base, attempt)
        };
        let key = format!("{}:{}", session_id, candidate);
        if let dashmap::mapref::entry::Entry::Vacant(slot) = LIVE.entry(key.clone()) {
            slot.insert(LiveAgent {
                session_id: session_id.to_string(),
                name: candidate.clone(),
            });
            return (candidate, InboxGuard { key });
        }
    }

    // Unreachable in practice: it would take four billion live agents under
    // one session id to exhaust the suffixes.
    let key = format!("{}:{}", session_id, base);
    (base, claim(key, session_id, FALLBACK_NAME))
}

/// Insert an address book entry unconditionally.
fn claim(key: String, session_id: &str, name: &str) -> InboxGuard {
    LIVE.insert(
        key.clone(),
        LiveAgent {
            session_id: session_id.to_string(),
            name: name.to_string(),
        },
    );
    InboxGuard { key }
}

/// Reduce free text to a name that can appear in an address.
fn slug(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.chars().count() >= MAX_NAME_CHARS {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() || is_reserved(trimmed) {
        FALLBACK_NAME.to_string()
    } else {
        trimmed.to_string()
    }
}

fn is_reserved(name: &str) -> bool {
    name == MAIN_NAME || name == PARENT_ALIAS || name == BROADCAST
}

/// Turn what the model wrote in `to` into the addresses it names.
///
/// # Errors
/// Returns a message naming the live agents whenever the address cannot be
/// resolved, so a wrong name is corrected rather than silently dropped.
pub fn resolve(addr: &AgentAddress, session_id: &str, to: &str) -> Result<Vec<String>, String> {
    if addr.own.is_empty() {
        return Err(
            "This agent has no address, so it cannot send messages. Messaging works \
             between agents started by AgentTool or TeamCreate."
                .to_string(),
        );
    }

    let to = to.trim();

    if to == BROADCAST {
        let targets: Vec<String> = LIVE
            .iter()
            .filter(|e| e.value().session_id == session_id && e.key() != &addr.own)
            .map(|e| e.key().clone())
            .collect();
        return Ok(targets);
    }

    if to == PARENT_ALIAS {
        let parent = addr
            .parent
            .as_ref()
            .ok_or_else(|| "This is the top-level session; it has no parent.".to_string())?;
        if !LIVE.contains_key(parent) {
            return Err("The parent agent is no longer running.".to_string());
        }
        return Ok(vec![parent.clone()]);
    }

    // A name is matched against the short name, never against the key, so a
    // hand-built `{session}:{name}` string reaches nothing by construction.
    let found = LIVE
        .iter()
        .find(|e| e.value().session_id == session_id && e.value().name == to)
        .map(|e| e.key().clone());

    match found {
        Some(key) if key == addr.own => {
            Err("An agent cannot send a message to itself.".to_string())
        }
        Some(key) => Ok(vec![key]),
        None => Err(format!(
            "No live agent is named '{}'. {}",
            to,
            known_agents(session_id, &addr.own)
        )),
    }
}

/// The names a sender may legitimately use right now.
fn known_agents(session_id: &str, own: &str) -> String {
    let mut names: Vec<String> = LIVE
        .iter()
        .filter(|e| e.value().session_id == session_id && e.key() != own)
        .map(|e| e.value().name.clone())
        .collect();
    names.sort();

    if names.is_empty() {
        "No other agent is running in this session.".to_string()
    } else {
        format!("Live agents: {}.", names.join(", "))
    }
}

/// Remove and return all messages queued for `recipient`.
pub fn drain_inbox(recipient: &str) -> Vec<AgentMessage> {
    INBOX.remove(recipient).map(|(_, v)| v).unwrap_or_default()
}

/// Read (without removing) all messages queued for `recipient`.
pub fn peek_inbox(recipient: &str) -> Vec<AgentMessage> {
    INBOX.get(recipient).map(|v| v.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct SendMessageTool;

#[derive(Debug, Deserialize)]
struct SendMessageInput {
    /// Recipient name, or "*" for broadcast.
    to: String,
    /// Message body.
    message: String,
    /// Short preview text shown in the UI.
    #[serde(default)]
    summary: Option<String>,
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }

    fn description(&self) -> &str {
        "Send a message to another agent running right now. The recipient reads it at the \
         start of its next turn. Address a sub-agent by the name it was given, the session \
         that spawned this one as \"parent\", the top-level session as \"main\", or every \
         other agent in this session as \"*\". Only agents that run at the same time can \
         reach each other: a foreground sub-agent blocks its parent, so the parent reads \
         the message only after that sub-agent has finished."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient: a sub-agent's name, \"parent\", \"main\", or \"*\" \
                                    for every other agent in this session. An unknown name is \
                                    rejected and the live names are listed."
                },
                "message": {
                    "type": "string",
                    "description": "Message content"
                },
                "summary": {
                    "type": "string",
                    "description": "5-10 word preview for the UI (optional)"
                }
            },
            "required": ["to", "message"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: SendMessageInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if params.message.trim().is_empty() {
            return ToolResult::error("Message cannot be empty.".to_string());
        }

        let targets = match resolve(&ctx.inbox, &ctx.session_id, &params.to) {
            Ok(targets) => targets,
            Err(reason) => return ToolResult::error(reason),
        };

        if targets.is_empty() {
            return ToolResult::error(format!(
                "Nothing was sent. {}",
                known_agents(&ctx.session_id, &ctx.inbox.own)
            ));
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // The recipient sees a name it can reply to, not an internal key.
        let from = ctx
            .inbox
            .name
            .clone()
            .unwrap_or_else(|| ctx.inbox.own.clone());

        let (content, clipped) =
            mikmik_core::truncate::truncate_tool_output(&params.message, MAX_MESSAGE_CHARS);

        // Both halves are model-supplied text, so both are bounded, and the
        // cut lands on a character boundary: slicing at a fixed byte offset
        // panics whenever that byte falls inside a multi-byte character.
        let preview = mikmik_core::truncate::truncate_text(
            params.summary.as_deref().unwrap_or(&params.message),
            PREVIEW_CHARS,
        );

        let mut delivered: Vec<String> = Vec::new();
        let mut full: Vec<String> = Vec::new();

        for key in &targets {
            let name = LIVE
                .get(key)
                .map(|e| e.value().name.clone())
                .unwrap_or_else(|| key.clone());

            let mut slot = INBOX.entry(key.clone()).or_default();
            if slot.len() >= MAX_INBOX_MESSAGES {
                full.push(name);
                continue;
            }
            slot.push(AgentMessage {
                from: from.clone(),
                to: key.clone(),
                content: content.clone(),
                timestamp: now,
            });
            delivered.push(name);
        }

        if delivered.is_empty() {
            return ToolResult::error(format!(
                "Nothing was sent: {} has {} undelivered message(s) already.",
                full.join(", "),
                MAX_INBOX_MESSAGES
            ));
        }

        let mut report = format!("Message sent to {}: {}", delivered.join(", "), preview);
        // Without this the agent has no way to know a reply will never come,
        // and it spends its remaining turns waiting for one.
        if ctx.inbox.parent_blocked
            && ctx
                .inbox
                .parent
                .as_ref()
                .is_some_and(|parent| targets.contains(parent))
        {
            report.push_str(
                "\nYour parent is waiting for you to finish and will read this only afterwards. \
                 Do not wait for a reply; put anything you still need to say in your final answer.",
            );
        }
        if clipped {
            report.push_str(&format!(
                "\nThe message was cut to {} characters.",
                MAX_MESSAGE_CHARS
            ));
        }
        if !full.is_empty() {
            report.push_str(&format!(
                "\nNot delivered to {}: inbox full.",
                full.join(", ")
            ));
        }

        ToolResult::success(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;
    use std::path::PathBuf;

    /// A context that knows its own address, which is what every send needs.
    fn addressed(session: &str, inbox: AgentAddress) -> ToolContext {
        let mut ctx = allow_all_context(PathBuf::from("/workspace"));
        ctx.session_id = session.to_string();
        ctx.inbox = inbox;
        ctx
    }

    /// The address a top-level session holds once the query loop registers it.
    fn main_address(session: &str) -> AgentAddress {
        AgentAddress {
            own: session.to_string(),
            parent: None,
            name: Some(MAIN_NAME.to_string()),
            parent_blocked: false,
        }
    }

    /// The address a sub-agent holds, given the guard that claimed it.
    fn child_address(session: &str, guard: &InboxGuard, name: &str, blocked: bool) -> AgentAddress {
        AgentAddress {
            own: guard.key().to_string(),
            parent: Some(session.to_string()),
            name: Some(name.to_string()),
            parent_blocked: blocked,
        }
    }

    async fn send(ctx: &ToolContext, to: &str, message: &str) -> ToolResult {
        SendMessageTool
            .execute(json!({ "to": to, "message": message }), ctx)
            .await
    }

    /// A message whose 60th byte lands inside a character. Slicing there is
    /// what the preview used to do, and it panicked.
    fn multibyte_message() -> String {
        format!("a{}", "€".repeat(25))
    }

    #[tokio::test]
    async fn preview_survives_a_multibyte_message() {
        let session = "sess-preview-boundary";
        let _main = register_main(session);
        let (name, guard) = register_named(session, Some("scout"), "look around");

        let ctx = addressed(session, main_address(session));
        let result = send(&ctx, &name, &multibyte_message()).await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(peek_inbox(guard.key()).len(), 1);
    }

    #[tokio::test]
    async fn preview_bounds_a_long_summary_too() {
        let session = "sess-preview-summary";
        let _main = register_main(session);
        let (name, _guard) = register_named(session, Some("scout"), "look around");

        let ctx = addressed(session, main_address(session));
        let result = SendMessageTool
            .execute(
                json!({ "to": name, "message": "short", "summary": "€".repeat(200) }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("(truncated)"),
            "an oversized summary should be cut: {}",
            result.content
        );
    }

    /// Without this the address book and the mailbox would both grow for the
    /// life of the process, and a name could never be reused.
    #[tokio::test]
    async fn a_guard_releases_the_address_and_the_unread_mail() {
        let session = "sess-guard-release";
        let _main = register_main(session);
        let (name, guard) = register_named(session, Some("scout"), "look around");
        let key = guard.key().to_string();

        let ctx = addressed(session, main_address(session));
        assert!(!send(&ctx, &name, "hello").await.is_error);
        assert_eq!(peek_inbox(&key).len(), 1);

        drop(guard);

        assert!(
            peek_inbox(&key).is_empty(),
            "unread mail outlived the agent"
        );
        let after = send(&ctx, &name, "hello again").await;
        assert!(after.is_error, "a finished agent is still addressable");
    }

    #[tokio::test]
    async fn parent_resolves_one_level_up() {
        let session = "sess-parent-up";
        let _main = register_main(session);
        let (name, guard) = register_named(session, Some("scout"), "look around");

        let child = addressed(session, child_address(session, &guard, &name, false));
        let result = send(&child, "parent", "found it").await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(peek_inbox(session).len(), 1);
        assert_eq!(peek_inbox(session)[0].from, name);
        drain_inbox(session);
    }

    #[tokio::test]
    async fn the_top_level_session_has_no_parent() {
        let session = "sess-no-parent";
        let _main = register_main(session);

        let ctx = addressed(session, main_address(session));
        let result = send(&ctx, "parent", "anyone there").await;

        assert!(result.is_error);
        assert!(result.content.contains("no parent"), "{}", result.content);
    }

    /// A broadcast that crossed sessions would put one user's sub-agent text
    /// into another user's conversation.
    #[tokio::test]
    async fn broadcast_stays_inside_the_session() {
        let mine = "sess-broadcast-mine";
        let theirs = "sess-broadcast-theirs";
        let _my_main = register_main(mine);
        let _their_main = register_main(theirs);
        let (_a, guard_a) = register_named(mine, Some("alpha"), "a");
        let (_b, guard_b) = register_named(mine, Some("beta"), "b");
        let (_c, guard_c) = register_named(theirs, Some("gamma"), "c");

        let ctx = addressed(mine, main_address(mine));
        let result = send(&ctx, "*", "all hands").await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(peek_inbox(guard_a.key()).len(), 1);
        assert_eq!(peek_inbox(guard_b.key()).len(), 1);
        assert!(
            peek_inbox(guard_c.key()).is_empty(),
            "the broadcast reached another session"
        );
        assert!(
            peek_inbox(mine).is_empty(),
            "the sender got its own broadcast"
        );
    }

    /// Neither the short name, nor a hand-built `{session}:{name}` key, nor
    /// the other session's own id reaches an agent that belongs elsewhere.
    #[tokio::test]
    async fn a_name_in_another_session_is_not_reachable() {
        let mine = "sess-reach-mine";
        let theirs = "sess-reach-theirs";
        let _my_main = register_main(mine);
        let _their_main = register_main(theirs);
        let (name, guard) = register_named(theirs, Some("stranger"), "s");

        let ctx = addressed(mine, main_address(mine));

        for attempt in [name.as_str(), guard.key(), theirs] {
            let result = send(&ctx, attempt, "hello").await;
            assert!(
                result.is_error,
                "'{attempt}' got through: {}",
                result.content
            );
        }
        assert!(peek_inbox(guard.key()).is_empty());
        assert!(peek_inbox(theirs).is_empty());
    }

    #[tokio::test]
    async fn an_unknown_name_lists_the_live_ones() {
        let session = "sess-unknown-name";
        let _main = register_main(session);
        let (_name, _guard) = register_named(session, Some("scout"), "look around");

        let ctx = addressed(session, main_address(session));
        let result = send(&ctx, "nobody", "hello").await;

        assert!(result.is_error);
        assert!(result.content.contains("scout"), "{}", result.content);
    }

    /// Dropping the overflow silently would report a delivery that never
    /// happened, which is the fault this whole tool used to have.
    #[tokio::test]
    async fn a_full_inbox_refuses_rather_than_dropping() {
        let session = "sess-full-inbox";
        let _main = register_main(session);
        let (name, guard) = register_named(session, Some("scout"), "look around");

        let ctx = addressed(session, main_address(session));
        for i in 0..MAX_INBOX_MESSAGES {
            assert!(!send(&ctx, &name, &format!("message {i}")).await.is_error);
        }

        let overflow = send(&ctx, &name, "one too many").await;

        assert!(overflow.is_error, "{}", overflow.content);
        assert_eq!(peek_inbox(guard.key()).len(), MAX_INBOX_MESSAGES);
    }

    #[tokio::test]
    async fn a_blocked_parent_is_reported_to_the_sender() {
        let session = "sess-blocked-parent";
        let _main = register_main(session);
        let (name, guard) = register_named(session, Some("scout"), "look around");

        let child = addressed(session, child_address(session, &guard, &name, true));
        let result = send(&child, "parent", "a question").await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("Do not wait for a reply"),
            "a waiting agent was not told the parent is blocked: {}",
            result.content
        );
        drain_inbox(session);
    }

    #[tokio::test]
    async fn a_running_agent_does_not_answer_to_a_second_one() {
        let session = "sess-name-clash";
        let (first, _guard_first) = register_named(session, Some("scout"), "a");
        let (second, _guard_second) = register_named(session, Some("scout"), "b");

        assert_eq!(first, "scout");
        assert_ne!(second, first, "two live agents took the same name");
    }

    #[tokio::test]
    async fn an_agent_cannot_message_itself() {
        let session = "sess-self-message";
        let _main = register_main(session);
        let (name, guard) = register_named(session, Some("scout"), "look around");

        let child = addressed(session, child_address(session, &guard, &name, false));
        let result = send(&child, &name, "talking to myself").await;

        assert!(result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn an_oversized_message_is_cut_and_the_sender_is_told() {
        let session = "sess-oversized";
        let _main = register_main(session);
        let (name, guard) = register_named(session, Some("scout"), "look around");

        let ctx = addressed(session, main_address(session));
        let result = send(&ctx, &name, &"x".repeat(MAX_MESSAGE_CHARS * 2)).await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("was cut"), "{}", result.content);
        let stored = peek_inbox(guard.key());
        assert!(stored[0].content.len() < MAX_MESSAGE_CHARS * 2);
    }

    /// A context built outside a query loop can neither send nor receive, and
    /// has to say so rather than writing into a mailbox nothing drains.
    #[tokio::test]
    async fn an_unaddressed_context_cannot_send() {
        let ctx = allow_all_context(PathBuf::from("/workspace"));
        let result = send(&ctx, "anyone", "hello").await;

        assert!(result.is_error);
        assert!(result.content.contains("no address"), "{}", result.content);
    }

    #[test]
    fn a_reserved_word_cannot_be_claimed_as_a_name() {
        assert_eq!(slug("main"), FALLBACK_NAME);
        assert_eq!(slug("parent"), FALLBACK_NAME);
        assert_eq!(slug("*"), FALLBACK_NAME);
    }

    #[test]
    fn a_description_becomes_a_usable_name() {
        assert_eq!(slug("Review the auth module"), "review-the-auth-module");
        assert_eq!(slug("  ??  "), FALLBACK_NAME);
        assert!(slug(&"x".repeat(200)).chars().count() <= MAX_NAME_CHARS);
    }
}
