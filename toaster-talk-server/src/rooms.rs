use crate::error::Error;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::HashMap, path::PathBuf};
pub use toaster_talk_common::sanitize_filename;
use toaster_talk_common::{ct_bytes_eq, format_duration_secs, is_valid_room_name};
use tracing::{debug, info, warn};

type HmacSha256 = Hmac<Sha256>;

/// A room access credential. Newtype over `String` (M-R5-002) so the compiler
/// rejects accidental confusion between room tokens, seat tokens, and session
/// tokens — all of which are otherwise indistinguishable `String`s.
///
/// `#[serde(transparent)]` keeps the on-disk YAML representation identical to a
/// bare string. `Borrow<str>` lets `HashMap<RoomToken, _>` be queried with `&str`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoomToken(pub String);

/// A per-member seat credential. See [`RoomToken`] for the newtype rationale.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SeatToken(pub String);

macro_rules! impl_token_newtype {
    ($t:ty) => {
        impl $t {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl std::ops::Deref for $t {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
        impl AsRef<str> for $t {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl std::borrow::Borrow<str> for $t {
            fn borrow(&self) -> &str {
                &self.0
            }
        }
        impl PartialEq<str> for $t {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }
        impl PartialEq<&str> for $t {
            fn eq(&self, other: &&str) -> bool {
                self.0.as_str() == *other
            }
        }
        impl From<String> for $t {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

impl_token_newtype!(RoomToken);
impl_token_newtype!(SeatToken);

// ── Token generation ──────────────────────────────────────────────────────────

/// Generate a room token: HMAC-SHA256(key=master_secret, msg=room_name).
///
/// Output is 32 bytes base64url-no-pad encoded (43 chars). HMAC accepts any
/// key length; the `.expect` is unreachable.
fn generate_room_token(secret: &[u8], room_name: &str) -> RoomToken {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(room_name.as_bytes());
    RoomToken(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

/// Generate a seat token: HMAC-SHA256(key=master_secret, msg=room_name || '\0' || member_name).
///
/// The null-byte separator prevents collisions between (room="foo", member="bar")
/// and (room="f", member="oobar").
pub fn generate_seat_token(secret: &[u8], room_name: &str, member_name: &str) -> SeatToken {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(room_name.as_bytes());
    mac.update(b"\0");
    mac.update(member_name.as_bytes());
    SeatToken(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

// ── MemberSeat ────────────────────────────────────────────────────────────────

/// A persisted seat token entry within a room. Stored in `Room.seats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberSeat {
    pub member_name: String,
    pub created_at_secs: u64,
    pub expires_at_secs: u64,
}

impl MemberSeat {
    pub fn is_expired(&self) -> bool {
        is_expired_at(self.expires_at_secs)
    }

    pub fn expires_in(&self) -> String {
        expires_in_str(self.expires_at_secs)
    }
}

// ── Room ─────────────────────────────────────────────────────────────────────

/// A persistent room definition. Serialized to the room store YAML file.
/// In-flight message queues are NOT stored here — those live in AppState.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub name: String,
    pub token: RoomToken,
    /// Unix timestamp (seconds since epoch) when the room was created.
    pub created_at_secs: u64,
    /// Unix timestamp (seconds since epoch) when the room expires.
    pub expires_at_secs: u64,
    /// Registered seat tokens for this room. `#[serde(default)]` ensures rooms
    /// stored before seat tokens were introduced deserialize correctly.
    #[serde(default)]
    pub seats: HashMap<SeatToken, MemberSeat>,
}

impl Room {
    pub fn is_expired(&self) -> bool {
        is_expired_at(self.expires_at_secs)
    }

    /// Human-readable expiry duration for CLI display.
    pub fn expires_in(&self) -> String {
        expires_in_str(self.expires_at_secs)
    }
}

// ── RoomStore ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct RoomStore {
    pub rooms: HashMap<RoomToken, Room>,
    #[serde(skip)]
    store_path: PathBuf,
}

impl RoomStore {
    /// Load the room store from disk. Creates an empty store if the file does not exist
    /// or is corrupt — an empty store is a safe default at startup, before any agent
    /// has connected.
    pub fn load(store_path: PathBuf) -> Result<Self, Error> {
        let mut store = match Self::parse(&store_path) {
            Ok(Some(s)) => s,
            Ok(None) => {
                debug!(path = %store_path.display(), "room store file not found; starting fresh");
                RoomStore::default()
            }
            Err(e) => {
                warn!(
                    path = %store_path.display(),
                    error = %e,
                    "room store file is corrupt or unreadable; starting with empty store"
                );
                RoomStore::default()
            }
        };

        store.store_path = store_path;
        Ok(store)
    }

    /// Re-read the room store from disk for a live `SIGHUP` reload — this is what lets
    /// an operator hand-edit `rooms.yaml` (for example to extend a room's or seat's
    /// `expires_at_secs`) and have it take effect without a restart.
    ///
    /// Unlike `load`, a missing or corrupt file is an `Err`, never a silent empty-store
    /// fallback: `load`'s forgiving default is safe at startup (no agent has connected
    /// yet) but would be catastrophic on a live reload, wiping every room and seat out
    /// from under connected agents because of a transient read or a typo. The caller
    /// must keep the running store unchanged on error, mirroring `Config::reload`.
    ///
    /// No reconciliation with `AppState` is needed: post-connect operations (send,
    /// receive) authorize purely against `AppState`, never against `RoomStore` — only
    /// `connect` and the TTL sweeper consult it. A room or seat removed by this reload
    /// therefore decays the same way an expired one already does (via the next sweep),
    /// not by an immediate disconnect.
    pub fn reload(store_path: PathBuf) -> Result<Self, Error> {
        let mut store = Self::parse(&store_path)?.ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("room store file not found: {}", store_path.display()),
            ))
        })?;
        store.store_path = store_path;
        Ok(store)
    }

    /// Parse the room store file, if present. `Ok(None)` means the file does not
    /// exist; `Err` means it exists but failed to read or parse.
    fn parse(store_path: &std::path::Path) -> Result<Option<Self>, Error> {
        if !store_path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(store_path)?;
        let store: RoomStore = serde_yml::from_str(&raw)?;
        info!(
            path = %store_path.display(),
            room_count = store.rooms.len(),
            "room store loaded"
        );
        Ok(Some(store))
    }

    async fn save(&self) -> Result<(), Error> {
        let yaml = serde_yml::to_string(self)?;
        let path = self.store_path.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, yaml)?;
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?
        .map_err(Error::Io)?;
        debug!(path = %self.store_path.display(), "room store saved");
        Ok(())
    }

    // ── Token verification ────────────────────────────────────────────────────

    /// Verify a room token using constant-time comparison to resist timing attacks.
    pub fn verify_token(&self, token: &str) -> Option<&Room> {
        // Linear scan with constant-time comparison — O(n) over max_rooms (≤50 by default),
        // which avoids the timing side-channel of HashMap's non-constant-time key comparison.
        let token_bytes = token.as_bytes();
        self.rooms
            .iter()
            .find(|(k, _)| ct_bytes_eq(k.as_bytes(), token_bytes))
            .map(|(_, v)| v)
    }

    /// Find the room and seat that matches a given seat token.
    /// O(rooms × seats per room) scan with constant-time comparison.
    pub fn seat_for_token(&self, seat_token: &str) -> Option<(&Room, &MemberSeat)> {
        let token_bytes = seat_token.as_bytes();
        for room in self.rooms.values() {
            if let Some((_, seat)) = room
                .seats
                .iter()
                .find(|(k, _)| ct_bytes_eq(k.as_bytes(), token_bytes))
            {
                return Some((room, seat));
            }
        }
        None
    }

    pub fn get_by_name(&self, name: &str) -> Option<&Room> {
        self.rooms.values().find(|r| r.name == name)
    }

    pub fn list(&self) -> Vec<&Room> {
        let mut rooms: Vec<&Room> = self.rooms.values().collect();
        rooms.sort_by_key(|r| r.created_at_secs);
        rooms
    }

    // ── Create ────────────────────────────────────────────────────────────────

    /// Create a new room. Token is HMAC-SHA256(master_secret, room_name).
    /// Returns the room definition including the generated token.
    /// The caller (CLI via mgmt socket) is responsible for writing the token file.
    pub async fn create(
        &mut self,
        name: String,
        ttl_days: u64,
        max_rooms: usize,
        master_secret: &[u8],
    ) -> Result<Room, Error> {
        if !is_valid_room_name(&name) {
            return Err(Error::InvalidRoomName(name));
        }

        if self.rooms.len() >= max_rooms {
            return Err(Error::RoomLimitReached { max: max_rooms });
        }

        let now = now_secs()?;
        let expires_at_secs = ttl_days
            .checked_mul(86400)
            .and_then(|d| now.checked_add(d))
            .ok_or_else(|| Error::Config("room TTL causes integer overflow".into()))?;

        let token = generate_room_token(master_secret, &name);

        let room = Room {
            name: name.clone(),
            token: token.clone(),
            created_at_secs: now,
            expires_at_secs,
            seats: HashMap::new(),
        };

        self.rooms.insert(token, room.clone());
        self.save().await?;

        info!(room = %name, expires_in = room.expires_in(), "room created");

        Ok(room)
    }

    // ── Delete ────────────────────────────────────────────────────────────────

    pub async fn delete(&mut self, name: &str) -> Result<Room, Error> {
        let token = self
            .rooms
            .values()
            .find(|r| r.name == name)
            .map(|r| r.token.clone())
            .ok_or_else(|| Error::RoomNotFound(name.to_string()))?;

        let room = self
            .rooms
            .remove(&token)
            .ok_or_else(|| Error::RoomNotFound(name.to_string()))?;
        self.save().await?;

        info!(room = %name, "room deleted");
        Ok(room)
    }

    // ── Seat management ───────────────────────────────────────────────────────

    /// Get a mutable reference to the room with the given name.
    fn room_mut_by_name(&mut self, name: &str) -> Result<&mut Room, Error> {
        let token = self
            .rooms
            .values()
            .find(|r| r.name == name)
            .map(|r| r.token.clone())
            .ok_or_else(|| Error::RoomNotFound(name.to_string()))?;
        self.rooms
            .get_mut(&token)
            .ok_or_else(|| Error::RoomNotFound(name.to_string()))
    }

    /// Create a seat token for `member_name` in `room_name`.
    /// `ttl_days = 0` defaults to the room's remaining lifetime.
    /// Returns `(seat_token, expires_at_secs)`.
    pub async fn create_seat(
        &mut self,
        room_name: &str,
        member_name: &str,
        ttl_days: u64,
        master_secret: &[u8],
    ) -> Result<(SeatToken, u64), Error> {
        // Validate member name — same rules as tools::validate_member_name, shared
        // via the common crate to break the rooms ← tools ← state ← rooms cycle.
        if !toaster_talk_common::is_valid_member_name(member_name) {
            return Err(Error::InvalidMemberName(member_name.to_string()));
        }

        let (seat_token, expires_at_secs) = {
            let room = self.room_mut_by_name(room_name)?;

            if room.is_expired() {
                return Err(Error::RoomExpired(room_name.to_string()));
            }

            let now = now_secs()?;

            let expires_at_secs = if ttl_days == 0 {
                room.expires_at_secs
            } else {
                let max_remaining_days = room.expires_at_secs.saturating_sub(now).div_ceil(86400);
                if ttl_days > max_remaining_days {
                    return Err(Error::SeatTtlExceeded {
                        given_days: ttl_days,
                        max_days: max_remaining_days,
                    });
                }
                now.saturating_add(ttl_days * 86400)
                    .min(room.expires_at_secs)
            };

            let seat_token = generate_seat_token(master_secret, room_name, member_name);

            room.seats.insert(
                seat_token.clone(),
                MemberSeat {
                    member_name: member_name.to_string(),
                    created_at_secs: now,
                    expires_at_secs,
                },
            );
            (seat_token, expires_at_secs)
        };

        self.save().await?;
        info!(room = %room_name, member = %member_name, "seat token created");
        Ok((seat_token, expires_at_secs))
    }

    /// Delete the seat token for `member_name` in `room_name`.
    pub async fn delete_seat(&mut self, room_name: &str, member_name: &str) -> Result<(), Error> {
        // Single lookup (M-R4-C11): find the seat key and remove it under one
        // mutable borrow rather than resolving the room twice.
        let room = self.room_mut_by_name(room_name)?;
        let key = room
            .seats
            .iter()
            .find(|(_, s)| s.member_name == member_name)
            .map(|(k, _)| k.clone())
            .ok_or_else(|| Error::SeatNotFound(member_name.to_string()))?;
        room.seats.remove(&key);

        self.save().await?;
        info!(room = %room_name, member = %member_name, "seat token deleted");
        Ok(())
    }

    /// List seats for a room, sorted by member_name.
    pub fn list_seats(&self, room_name: &str) -> Option<Vec<&MemberSeat>> {
        let room = self.get_by_name(room_name)?;
        let mut seats: Vec<&MemberSeat> = room.seats.values().collect();
        seats.sort_by(|a, b| a.member_name.cmp(&b.member_name));
        Some(seats)
    }

    /// Remove expired seat tokens from all rooms. Returns total seats removed.
    /// Saves to disk only if at least one seat was removed.
    pub async fn remove_expired_seats(&mut self) -> Result<usize, Error> {
        let now = now_secs()?;
        let mut total = 0usize;
        for room in self.rooms.values_mut() {
            let before = room.seats.len();
            room.seats.retain(|_, s| s.expires_at_secs > now);
            total += before - room.seats.len();
        }
        if total > 0 {
            self.save().await?;
            debug!(removed = total, "expired seat tokens swept");
        }
        Ok(total)
    }

    // ── TTL sweep ─────────────────────────────────────────────────────────────

    /// Remove all expired rooms. Returns the list of expired rooms for eviction.
    pub async fn remove_expired(&mut self) -> Result<Vec<Room>, Error> {
        let now = now_secs()?;
        let expired: Vec<RoomToken> = self
            .rooms
            .iter()
            .filter(|(_, r)| r.expires_at_secs <= now)
            .map(|(t, _)| t.clone())
            .collect();

        if expired.is_empty() {
            return Ok(vec![]);
        }

        let mut removed = Vec::with_capacity(expired.len());
        for token in &expired {
            if let Some(room) = self.rooms.remove(token) {
                info!(room = %room.name, "room expired; removing");
                removed.push(room);
            }
        }

        self.save().await?;
        Ok(removed)
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Current time as Unix seconds, or an error if the system clock is before epoch.
pub fn now_secs() -> Result<u64, Error> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| Error::Config("system clock is set before the Unix epoch".into()))
}

fn is_expired_at(expires_at_secs: u64) -> bool {
    now_secs().map(|n| n >= expires_at_secs).unwrap_or(false)
}

fn expires_in_str(expires_at_secs: u64) -> String {
    match now_secs() {
        Err(_) => "unknown".into(),
        Ok(now) => {
            if now >= expires_at_secs {
                "expired".into()
            } else {
                format_duration_secs(expires_at_secs - now)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TEST_MASTER_SECRET;
    use tempfile::TempDir;

    fn make_store(dir: &TempDir) -> RoomStore {
        RoomStore::load(dir.path().join("rooms.yaml")).unwrap()
    }

    #[test]
    fn test_load_missing_file_gives_empty_store() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        assert!(store.rooms.is_empty());
    }

    #[test]
    fn test_load_corrupted_file_gives_empty_store() {
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        std::fs::write(&store_path, b"not: valid: yaml: {{{").unwrap();
        let store = RoomStore::load(store_path).unwrap();
        assert!(store.rooms.is_empty());
    }

    #[tokio::test]
    async fn test_round_trip_create_and_reload() {
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");

        let token = {
            let mut store = RoomStore::load(store_path.clone()).unwrap();
            store
                .create("testroom".into(), 30, 50, TEST_MASTER_SECRET)
                .await
                .unwrap()
                .token
        };

        let store2 = RoomStore::load(store_path).unwrap();
        let room = store2
            .verify_token(&token)
            .expect("room should persist after reload");
        assert_eq!(room.name, "testroom");
    }

    #[tokio::test]
    async fn test_room_limit_reached() {
        let dir = TempDir::new().unwrap();
        let mut store = make_store(&dir);
        store
            .create("room1".into(), 30, 1, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let err = store
            .create("room2".into(), 30, 1, TEST_MASTER_SECRET)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::error::Error::RoomLimitReached { .. }));
    }

    #[tokio::test]
    async fn test_delete_removes_room() {
        // Rubric F-016: deletion must be persisted — verify via disk reload.
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path.clone()).unwrap();
        store
            .create("deleteme".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        store.delete("deleteme").await.unwrap();

        assert!(store.get_by_name("deleteme").is_none());

        let reloaded = RoomStore::load(store_path).unwrap();
        assert!(
            reloaded.get_by_name("deleteme").is_none(),
            "deletion must be persisted to disk"
        );
    }

    #[tokio::test]
    async fn test_seat_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut store = make_store(&dir);
        store
            .create("myroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();

        let (token, _expires) = store
            .create_seat("myroom", "alice", 0, TEST_MASTER_SECRET)
            .await
            .unwrap();

        let (room, seat) = store.seat_for_token(&token).expect("seat must be found");
        assert_eq!(room.name, "myroom");
        assert_eq!(seat.member_name, "alice");
    }

    #[tokio::test]
    async fn test_seat_token_is_deterministic() {
        let dir = TempDir::new().unwrap();
        let mut store = make_store(&dir);
        store
            .create("myroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();

        let (t1, _) = store
            .create_seat("myroom", "alice", 0, TEST_MASTER_SECRET)
            .await
            .unwrap();

        // Delete and recreate — should produce the same token.
        store.delete_seat("myroom", "alice").await.unwrap();
        let (t2, _) = store
            .create_seat("myroom", "alice", 0, TEST_MASTER_SECRET)
            .await
            .unwrap();

        assert_eq!(t1, t2, "seat token must be deterministic for same inputs");
    }

    #[tokio::test]
    async fn test_seat_null_byte_separator() {
        // HMAC("foo\0bar") != HMAC("f\0oobar") — collision safety check.
        let t1 = generate_seat_token(TEST_MASTER_SECRET, "foo", "bar");
        let t2 = generate_seat_token(TEST_MASTER_SECRET, "f", "oobar");
        assert_ne!(
            t1, t2,
            "seat tokens must be collision-safe across room/member split"
        );
    }

    #[tokio::test]
    async fn test_delete_seat_removes_entry() {
        let dir = TempDir::new().unwrap();
        let mut store = make_store(&dir);
        store
            .create("myroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let (token, _) = store
            .create_seat("myroom", "alice", 0, TEST_MASTER_SECRET)
            .await
            .unwrap();

        store.delete_seat("myroom", "alice").await.unwrap();

        assert!(
            store.seat_for_token(&token).is_none(),
            "seat must be gone after delete"
        );
    }

    // ── TDD 11.1–11.3: room name validation ──────────────────────────────────

    #[tokio::test]
    async fn test_room_name_rejects_space() {
        let dir = TempDir::new().unwrap();
        let mut store = make_store(&dir);
        let result = store
            .create("my room".into(), 30, 50, TEST_MASTER_SECRET)
            .await;
        assert!(matches!(result, Err(Error::InvalidRoomName(_))));
    }

    #[tokio::test]
    async fn test_room_name_rejects_quote() {
        let dir = TempDir::new().unwrap();
        let mut store = make_store(&dir);
        let result = store
            .create("my\"room".into(), 30, 50, TEST_MASTER_SECRET)
            .await;
        assert!(matches!(result, Err(Error::InvalidRoomName(_))));
    }

    #[tokio::test]
    async fn test_room_name_rejects_too_long() {
        let dir = TempDir::new().unwrap();
        let mut store = make_store(&dir);
        let result = store
            .create("a".repeat(41), 30, 50, TEST_MASTER_SECRET)
            .await;
        assert!(matches!(result, Err(Error::InvalidRoomName(_))));
    }

    #[tokio::test]
    async fn test_room_name_rejects_empty() {
        let dir = TempDir::new().unwrap();
        let mut store = make_store(&dir);
        let result = store.create("".into(), 30, 50, TEST_MASTER_SECRET).await;
        assert!(matches!(result, Err(Error::InvalidRoomName(_))));
    }

    // ── SIGHUP room store reload (rubrics 8.15, 8.16) ─────────────────────────

    #[test]
    fn test_reload_missing_file_is_err() {
        // Rubric 8.16: a missing file must error, not silently produce an empty store.
        let dir = TempDir::new().unwrap();
        let err = RoomStore::reload(dir.path().join("rooms.yaml")).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn test_reload_corrupt_file_is_err() {
        // Rubric 8.16: malformed YAML must error, not silently produce an empty store.
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        std::fs::write(&store_path, b"not: valid: yaml: {{{").unwrap();
        assert!(RoomStore::reload(store_path).is_err());
    }

    #[tokio::test]
    async fn test_reload_picks_up_hand_edited_expiry() {
        // Rubric 8.15: the operator's use case is extending a room's expires_at_secs on
        // disk and having a reload pick it up, without needing to recreate the room
        // (which would mint a token — the tokens are deterministic, but this path
        // shouldn't require it).
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path.clone()).unwrap();
        store
            .create("myroom".into(), 1, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();

        let raw = std::fs::read_to_string(&store_path).unwrap();
        let edited = raw.replace(
            &format!(
                "expires_at_secs: {}",
                store.get_by_name("myroom").unwrap().expires_at_secs
            ),
            "expires_at_secs: 9999999999",
        );
        assert_ne!(raw, edited, "hand-edit must actually change the file");
        std::fs::write(&store_path, edited).unwrap();

        let reloaded = RoomStore::reload(store_path).unwrap();
        assert_eq!(
            reloaded.get_by_name("myroom").unwrap().expires_at_secs,
            9999999999
        );
    }

    #[tokio::test]
    async fn test_seat_persists_after_reload() {
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        let token = {
            let mut store = RoomStore::load(store_path.clone()).unwrap();
            store
                .create("myroom".into(), 30, 50, TEST_MASTER_SECRET)
                .await
                .unwrap();
            let (token, _) = store
                .create_seat("myroom", "alice", 0, TEST_MASTER_SECRET)
                .await
                .unwrap();
            token
        };
        let store2 = RoomStore::load(store_path).unwrap();
        assert!(
            store2.seat_for_token(&token).is_some(),
            "seat must persist across reload"
        );
    }
}
