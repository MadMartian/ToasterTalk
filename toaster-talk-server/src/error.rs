use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("room not found: {0}")]
    RoomNotFound(String),

    #[error("invalid room token")]
    InvalidRoomToken,

    #[error("invalid or expired session token")]
    InvalidSessionToken,

    #[error("this session has been displaced by an authoritative seat token; reconnect to obtain a new session")]
    Superseded,

    #[error("member name already in use in this room: {0}")]
    MemberNameTaken(String),

    #[error(
        "invalid room name '{0}': must be non-empty, ≤40 characters, and must not contain spaces, double-quote, or null bytes"
    )]
    InvalidRoomName(String),

    #[error("member not found in room: {0}")]
    MemberNotFound(String),

    #[error(
        "you are not connected to room '{0}'; you can only act on a room your session belongs to"
    )]
    NotConnectedToRoom(String),

    #[error("you have already joined room '{0}' with this session; each session may hold only one membership per room")]
    AlreadyJoinedRoom(String),

    #[error("you have not joined any room yet; call `join` before send/receive/members")]
    NotJoinedAnyRoom,

    #[error("invalid member name '{0}': must be 1–40 characters, [a-zA-Z0-9_-] only")]
    InvalidMemberName(String),

    #[error("message body is {size} bytes, exceeds maximum {max} bytes")]
    MessageTooLarge { size: usize, max: usize },

    #[error("message TTL {given_secs}s exceeds configured maximum {max_secs}s")]
    MessageTtlExceeded { given_secs: u64, max_secs: u64 },

    #[error("room TTL {given_days} days exceeds configured maximum {max_days} days")]
    RoomTtlExceeded { given_days: u64, max_days: u64 },

    #[error("seat TTL {given_days} days exceeds the room's remaining lifetime of {max_days} days")]
    SeatTtlExceeded { given_days: u64, max_days: u64 },

    #[error("invalid or expired seat token")]
    InvalidSeatToken,

    #[error("no seat token registered for member '{0}' in this room")]
    SeatNotFound(String),

    // Reserved for future enforcement when send validates sender/recipient room membership
    #[allow(dead_code)]
    #[error(
        "cross-room messaging is not permitted; sender and recipient must be in the same room"
    )]
    CrossRoomMessage,

    #[error("room limit reached: server is at maximum capacity ({max} rooms)")]
    RoomLimitReached { max: usize },

    #[error("room '{0}' has expired and is no longer accessible")]
    RoomExpired(String),

    #[error("inbox full: member '{0}' has reached the maximum inbox size")]
    InboxFull(String),

    #[error("recipient '{0}' was evicted during the send; message not delivered")]
    RecipientEvicted(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl From<serde_yml::Error> for Error {
    fn from(e: serde_yml::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Self::Config(e.to_string())
    }
}
