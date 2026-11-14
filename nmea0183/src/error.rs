use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NmeaError {
    #[error("checksum mismatch: expected {expected:#04X}, computed {computed:#04X}")]
    InvalidChecksum { expected: u8, computed: u8 },

    #[error("sentence has no checksum")]
    MissingChecksum,

    #[error("sentence does not start with '$' or '!'")]
    MissingStartDelimiter,

    #[error("talker ID is too short or malformed: {0:?}")]
    InvalidTalkerId(String),

    #[error("field {index} is invalid: {message}")]
    InvalidField { index: usize, message: String },

    #[error("parse error: {0}")]
    Parse(String),
}
