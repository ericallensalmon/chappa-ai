//! Who is calling the control surface: the person, or one agent.
//!
//! The person's token (`<app-config>/control_token`) carries the person's
//! full authority and is meant for the person's own tools and scripts. Each
//! agent the app spawns, and each terminal an agent spawns, gets a token of
//! its own instead: random, held only in this process's memory, mapped to
//! the row it was minted for, and revoked when that row exits or closes. The
//! child receives it in `CHAPPA_AI_CONTROL_TOKEN` (which `chappa-ai-mcp`
//! prefers over the token file), so an agent's MCP server speaks with the
//! agent's authority, never the person's.
//!
//! What a token identifies is decided here, from the token alone: for an
//! agent token the `X-Chappa-Actor` header is ignored and the actor is the
//! row the token was minted for.
//!
//! This is a boundary only for agents that cannot read the person's files.
//! An agent running unsandboxed on the host, with a shell, as the same OS
//! user, can still read the person's token file and use it; containerised
//! tools that see only their own workspace are the case this closes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::registry::TermId;

/// The env var an agent's token is delivered in (read by `chappa-ai-mcp`).
pub const TOKEN_ENV: &str = "CHAPPA_AI_CONTROL_TOKEN";

/// What an agent token was minted for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentGrant {
    /// The row the token speaks for.
    pub process_id: TermId,
    /// The project the row works in, when it has one: where it may spawn.
    pub project_id: Option<u32>,
    /// The agent row that spawned this one with its own token, `None` when
    /// the person did.
    pub parent_process_id: Option<TermId>,
    /// The agent tool, `None` for a terminal an agent opened.
    pub tool: Option<u32>,
    /// Unix ms when it was minted.
    pub created_ms: u64,
}

/// The caller of one request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Caller {
    /// The person token, or the app itself (its own windows, a click).
    #[default]
    Person,
    /// An agent token: scoped to its row and what that row spawned.
    Agent(AgentGrant),
}

impl Caller {
    pub fn is_agent(&self) -> bool {
        matches!(self, Caller::Agent(_))
    }

    pub fn grant(&self) -> Option<&AgentGrant> {
        match self {
            Caller::Agent(grant) => Some(grant),
            Caller::Person => None,
        }
    }

    /// `"person"` or `"agent"`, as `/whoami` answers it.
    pub fn kind(&self) -> &'static str {
        match self {
            Caller::Person => "person",
            Caller::Agent(_) => "agent",
        }
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The live agent tokens. Only a digest of each token is kept: the token
/// itself exists in the child's environment and nowhere in the app after
/// it is handed over, and nothing here is ever written to disk.
#[derive(Clone, Default)]
pub struct AgentTokens {
    inner: Arc<Mutex<HashMap<[u8; 32], AgentGrant>>>,
}

impl AgentTokens {
    /// A new token for `grant` (32 bytes from the OS RNG, hex). Any token
    /// the same row held before is revoked first: one row, one token.
    pub fn mint(&self, grant: AgentGrant) -> Result<String, String> {
        let token = crate::control_http::random_hex(32)
            .map_err(|e| format!("cannot generate an agent token: {e}"))?;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.retain(|_, g| g.process_id != grant.process_id);
        inner.insert(digest(&token), grant);
        Ok(token)
    }

    /// The grant behind a presented token, `None` when it is not a live
    /// agent token.
    pub fn lookup(&self, presented: &str) -> Option<AgentGrant> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.get(&digest(presented.trim())).cloned()
    }

    /// Revoke every token minted for `process_id`. Answers how many went.
    pub fn revoke(&self, process_id: TermId) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = inner.len();
        inner.retain(|_, g| g.process_id != process_id);
        before - inner.len()
    }

    /// How many agent tokens are live.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(id: TermId) -> AgentGrant {
        AgentGrant {
            process_id: id,
            project_id: Some(1),
            parent_process_id: None,
            tool: Some(3),
            created_ms: 1,
        }
    }

    #[test]
    fn an_agent_token_maps_to_its_row_until_revoked_and_a_row_holds_one() {
        let tokens = AgentTokens::default();
        let a = tokens.mint(grant(4)).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(tokens.lookup(&a), Some(grant(4)));
        assert_eq!(tokens.lookup("not-a-token"), None);
        let b = tokens.mint(grant(4)).unwrap();
        assert_ne!(a, b);
        assert_eq!(
            tokens.lookup(&a),
            None,
            "a new token replaces the row's old one"
        );
        assert_eq!(tokens.lookup(&b).map(|g| g.process_id), Some(4));
        let other = tokens.mint(grant(5)).unwrap();
        assert_eq!(tokens.revoke(4), 1);
        assert_eq!(tokens.lookup(&b), None);
        assert!(tokens.lookup(&other).is_some());
        assert_eq!(tokens.len(), 1);
    }
}
