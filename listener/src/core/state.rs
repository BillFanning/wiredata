//! Channel kinds and the lifecycle/state enums (spec §7, §8, §11, §12, §130),
//! plus the §9 state-transition rules.

/// The four supported Channel kinds (§7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChannelKind {
    Serial,
    Udp,
    TcpListener,
    TcpConnection,
}

/// Channel lifecycle state (§8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Faulted,
}

impl ChannelState {
    /// Whether a direct transition `self -> next` is permitted by §9.
    ///
    /// Permitted edges (§9):
    /// `Stopped → Starting → Running`, `Running → Stopping → Stopped`,
    /// `Starting → Faulted`, `Running → Faulted`, `Faulted → Stopped`.
    ///
    /// Everything else — including the explicitly forbidden
    /// `Stopped → Running`, `Running → Starting`, `Faulted → Running`, and any
    /// self-transition — is rejected.
    pub fn can_transition_to(self, next: ChannelState) -> bool {
        use ChannelState::*;
        matches!(
            (self, next),
            (Stopped, Starting)
                | (Starting, Running)
                | (Running, Stopping)
                | (Stopping, Stopped)
                | (Starting, Faulted)
                | (Running, Faulted)
                | (Faulted, Stopped)
        )
    }
}

/// Per-view display presentation state (§11). Runtime-only; never persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayState {
    Active,
    Paused,
}

/// Recording state, independent of Channel state (§12). A Channel may be
/// `Running` while its recording is in a gap or faulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingState {
    Disabled,
    /// Writing, or opening its first segment with bytes queued (§56.1).
    Enabled,
    /// In a gap: bytes are being omitted until the next segment opens (§56.1).
    Gap(GapReason),
    /// Could not begin at all — a missing destination, a refused overwrite —
    /// and waits for the user; retrying cannot fix it (§55).
    Faulted,
}

impl RecordingState {
    /// Whether this recording is on: writing, opening, or in a gap it will
    /// recover from. `Faulted` and `Disabled` are off.
    pub fn is_on(self) -> bool {
        matches!(self, Self::Enabled | Self::Gap(_))
    }
}

/// Why a recording is in a gap (§56.1). Every gap names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapReason {
    /// The recording queue filled faster than the disk drained it.
    QueueOverflow,
    /// Writing or flushing the segment failed.
    WriteFailed,
    /// The next segment could not be opened.
    OpenFailed,
    /// The recording folder, or its marker, is not there (§59).
    DestinationMissing,
    /// Free space is below the disk guard's threshold (§56.2).
    LowDisk,
}

impl GapReason {
    /// The reason in words, for diagnostics and the event log.
    pub fn describe(self) -> &'static str {
        match self {
            Self::QueueOverflow => "the recording queue overflowed",
            Self::WriteFailed => "writing the file failed",
            Self::OpenFailed => "the next file could not be opened",
            Self::DestinationMissing => "the recording folder is missing",
            Self::LowDisk => "free disk space is low",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChannelState::*;

    #[test]
    fn permitted_transitions_are_allowed() {
        assert!(Stopped.can_transition_to(Starting));
        assert!(Starting.can_transition_to(Running));
        assert!(Running.can_transition_to(Stopping));
        assert!(Stopping.can_transition_to(Stopped));
        assert!(Starting.can_transition_to(Faulted));
        assert!(Running.can_transition_to(Faulted));
        assert!(Faulted.can_transition_to(Stopped));
    }

    #[test]
    fn explicitly_forbidden_transitions_are_rejected() {
        // §9 names these three as forbidden direct transitions.
        assert!(!Stopped.can_transition_to(Running));
        assert!(!Running.can_transition_to(Starting));
        assert!(!Faulted.can_transition_to(Running));
    }

    #[test]
    fn self_transitions_are_rejected() {
        for state in [Stopped, Starting, Running, Stopping, Faulted] {
            assert!(
                !state.can_transition_to(state),
                "{state:?} should not transition to itself"
            );
        }
    }

    #[test]
    fn stopping_and_stopped_cannot_fault_directly() {
        // §9 lists Faulted only from Starting and Running.
        assert!(!Stopping.can_transition_to(Faulted));
        assert!(!Stopped.can_transition_to(Faulted));
    }
}
