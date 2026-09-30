//! Common listener types, stream data model, IDs, states, and shared errors.
//!
//! This is `listener-core` (spec §128): the pure data model shared by every
//! other module. It owns common types but avoids I/O, UI, and async-runtime
//! coupling — transports, the runtime, and the presentation layers depend on
//! these types, not the reverse.

pub mod command;
pub mod error;
pub mod ids;
pub mod state;
pub mod sync;
pub mod timing;

pub use command::{RecordingTap, RuntimeEvent};
pub use error::RecordError;
pub use ids::{ChannelId, ChannelName, DisplayViewId, MatchRuleId, StableConfigId};
pub use state::{ChannelKind, ChannelState, DisplayState, GapReason, RecordingState};
pub use sync::lock_recover;
pub use timing::{
    validate_zda_talker_id, zda_sentence, ArrivalTimestampSource, ArrivalTimestampStatus,
    ChunkTime, TimestampConfig, ZdaTalkerIdError, MAX_ZDA_TALKER_ID_BYTES,
};
