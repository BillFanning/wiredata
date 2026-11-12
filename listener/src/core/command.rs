//! Runtime event vocabulary the runtime orchestrator reports to the presentation
//! layers (spec §137).
//!
//! Events flow runtime → UI; the UI observes them and never owns transport,
//! recording, or pipeline state (§3). The *command* direction (UI → runtime) is
//! the [`Listener`](crate::runtime::Listener)'s async method API directly — there is
//! no separate runtime-command enum (ADR-012). The GUI's own `UiCommand`
//! (`gui::bridge`) is the on-the-wire form the driver translates into those calls.

use std::time::Duration;

use super::ids::{ChannelId, MatchRuleId};

/// Which recording lane an event refers to (§53/§54). A Channel runs its Raw
/// (verbatim bytes) and Display (rendered text) recordings independently, and
/// their faults and recoveries are independent too — an observer must not clear
/// a Display fault because the Raw lane started, or vice versa, so the events
/// carry the lane instead of leaving observers to guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingTap {
    Raw,
    Display,
}

impl RecordingTap {
    /// Human label for messages: "Raw" / "Display".
    pub fn label(self) -> &'static str {
        match self {
            RecordingTap::Raw => "Raw",
            RecordingTap::Display => "Display",
        }
    }
}

/// Something the runtime reports happened (§137).
///
/// `#[non_exhaustive]`: the event vocabulary grows across versions, so observers
/// (notably the GUI's event-folding loop) must keep a wildcard arm and stay
/// forward-compatible.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeEvent {
    ChannelStarted(ChannelId),
    ChannelStopped(ChannelId),
    ChannelFaulted(ChannelId),
    /// A recording on the carried lane faulted (§55, §56.1): a begin that couldn't
    /// open the file, a mid-run write/flush failure, or a dirty stop.
    RecordingFaulted(ChannelId, RecordingTap),
    /// A recording on the carried lane successfully began (§50.2, §54): the file is
    /// open and recording. Lets observers clear a prior `RecordingFaulted` on the
    /// **same lane** — a recording fault leaves the Channel Running, so
    /// `ChannelStarted` doesn't re-fire to clear it, and the other lane's begin
    /// must not clear it either.
    RecordingStarted(ChannelId, RecordingTap),
    WarningRaised(ChannelId),
    TcpClientConnected(ChannelId),
    TcpClientDisconnected(ChannelId),
    /// A sustained reader stall on a Channel's Transport→Pipeline edge (§101,
    /// ADR-007): possible transport-specific loss. Carries how long the reader was
    /// stalled. Dedicated variant (v1.2 §137) — replaces the earlier reuse of
    /// `WarningRaised` so observers can distinguish a stall from any other warning.
    ReceptionStalled(ChannelId, Duration),
    /// A reader stall that had raised [`ReceptionStalled`](Self::ReceptionStalled)
    /// ended because the blocked send went through; carries the stall's full length.
    ReceptionResumed(ChannelId, Duration),
    /// A serial Channel's control/status lines changed (§14.3, §161); read the
    /// current state from the snapshot/query. Part of the v1.2 §137 vocabulary.
    ControlLinesChanged(ChannelId),
    /// Auto-reconnect (§9.1, §162) is attempting to re-Start a faulted Channel;
    /// carries the 1-based attempt number.
    ChannelReconnecting(ChannelId, u32),
    /// Auto-reconnect succeeded — the Channel is Running again.
    ChannelReconnected(ChannelId),
    /// Auto-reconnect gave up after `max_attempts`; the Channel stays Faulted.
    ChannelReconnectGaveUp(ChannelId),
    /// Free disk space for a Channel's recording fell below its guard threshold
    /// (§56.2, §168). Recording may continue (Warn) or stop (see below).
    DiskSpaceLow(ChannelId),
    /// A recording was stopped and finalized because of low disk (§168); reception
    /// continues.
    RecordingStoppedLowDisk(ChannelId),
    /// A Match Rule fired on a Channel (§50.2, §165). Carries the runtime id of the
    /// rule that matched; `Notify`/`Mark` actions are observable here.
    MatchTriggered(ChannelId, MatchRuleId),
}
