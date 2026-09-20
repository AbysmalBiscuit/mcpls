//! Identities shared by hook and MCP diagnostic delivery.

use serde::{Deserialize, Serialize};

use super::SessionId;

/// The independently acknowledged diagnostic record of a caller.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RecordId {
    /// A host session or Codex thread.
    Session(SessionId),
    /// A Claude agent, whose identifier is scoped to its root session.
    ClaudeAgent {
        /// The root session that scopes the identifier.
        root: SessionId,
        /// The identifier carried by the agent's hook payload.
        agent: String,
    },
}

impl From<&SessionId> for RecordId {
    fn from(session: &SessionId) -> Self {
        Self::Session(session.clone())
    }
}

impl From<&Self> for RecordId {
    fn from(record: &Self) -> Self {
        record.clone()
    }
}

/// A delivery record and its root, when the host has identified the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The caller's independent delivery history.
    pub record: RecordId,
    /// The root session, once either entry point has identified it.
    pub root: Option<SessionId>,
}

/// The host whose hook supplies the agent identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookHost {
    #[default]
    /// Claude Code hooks.
    Claude,
    /// Codex hooks.
    Codex,
}

/// Optional agent fields flattened into hook requests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookAgent {
    /// Missing or empty for a root-session hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Determines the scope of the agent identifier.
    #[serde(default)]
    pub host: HookHost,
}

impl HookAgent {
    /// Resolve the hook's root and agent without concatenating identifiers.
    #[must_use]
    pub fn caller(&self, session: &str) -> Caller {
        let root = SessionId::from(session.to_owned());
        let agent = self.agent_id.as_ref().filter(|agent| !agent.is_empty());
        let record = match (self.host, agent) {
            (HookHost::Claude, Some(agent)) => RecordId::ClaudeAgent {
                root: root.clone(),
                agent: agent.clone(),
            },
            (HookHost::Codex, Some(agent)) => RecordId::Session(SessionId::from(agent.clone())),
            (_, None) => RecordId::Session(root.clone()),
        };
        Caller {
            record,
            root: Some(root),
        }
    }
}
