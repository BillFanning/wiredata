//! Channel and configuration identifiers (spec §129, §80.1).
//!
//! `ChannelId` is a runtime object minted while Listener runs (§97.1: the
//! runtime mints `ChannelId`s). `StableConfigId` is the persisted identity stored in a profile
//! (§72, `ChannelConfig::id`).

use std::fmt;

use uuid::Uuid;

/// Unique internal identifier for a Channel (§6, §129). Runtime-minted; never
/// persisted in a profile (§69).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelId(Uuid);

impl ChannelId {
    /// Mint a fresh, random channel id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ChannelId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// User-visible Channel name (§6). User configurable; must be **unique** within a
/// workspace and filesystem-safe (§6, §71, ADR-014) — it generates recording filenames
/// (§59). `ChannelName::new` does not enforce this; uniqueness is checked by
/// `Profile::validate` and guarded in the GUI (the runtime API does not yet own the
/// invariant — see ADR-014).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChannelName(String);

impl ChannelName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChannelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Runtime identity of one Match Rule on a Channel (§50.2). Minted when a
/// Channel's rules are compiled at Start, so events (`MatchTriggered`) — and,
/// later, the deferred live rule-toggle — can name a specific rule.
/// Runtime-only — configuration identifies a rule by its `name` (§50.2); this
/// id is never persisted (§69).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MatchRuleId(Uuid);

impl MatchRuleId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for MatchRuleId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for MatchRuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stable identity for a configured Channel, persisted in a profile so a saved
/// workspace round-trips to the same logical channel (§72, §80.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StableConfigId(Uuid);

impl StableConfigId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for StableConfigId {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_ids_are_unique() {
        assert_ne!(ChannelId::new(), ChannelId::new());
    }

    #[test]
    fn channel_name_round_trips() {
        let name = ChannelName::new("GPS Receiver");
        assert_eq!(name.as_str(), "GPS Receiver");
        assert_eq!(name.to_string(), "GPS Receiver");
    }
}
