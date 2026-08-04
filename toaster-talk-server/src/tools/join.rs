use crate::{
    error::Error,
    rooms::{RoomStore, RoomToken},
    state::{AuthMethod, MembershipCheck, SharedAppState},
    tools::{session_id, validate_member_name},
};
use std::{sync::Arc, time::Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

#[derive(Debug)]
pub struct JoinResult {
    pub room_name: String,
}

/// Attach a room membership to an existing session, using a plain room token
/// (shared-access credential). The *only* path by which any room membership is
/// ever created (TDD 1b) — including an agent's very first room.
///
/// - Missing/unrecognized `session_token`: hard `InvalidSessionToken` — `join`
///   never silently mints a session (TDD 1b.2, 1b.6).
/// - The desired member name already held by a *different* session in this room:
///   `MemberNameTaken` (TDD 1b.3). There is no `old_session_token` escape hatch
///   here — that concept lives solely on `connect` now, and no longer carries
///   room state (see PLAN.multi-receive.md).
/// - The caller's own session already belongs to this room: rejected as
///   redundant with `AlreadyJoinedRoom`, not treated as a no-op (TDD 1b.7).
pub async fn handle_join_room_token(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    session_token: String,
    room_token: String,
    member_name: String,
) -> Result<JoinResult, Error> {
    validate_member_name(&member_name)?;

    let room_name = {
        let store_read = store.read().await;
        let room = store_read
            .verify_token(&room_token)
            .ok_or(Error::InvalidRoomToken)?;
        if room.is_expired() {
            warn!(room = %room.name, "join rejected — room has expired");
            return Err(Error::RoomExpired(room.name.clone()));
        }
        debug!(room = %room.name, member = %member_name, "room token verified for join");
        room.name.clone()
    };

    let mut app_write = app.write().await;

    match app_write.check_room_membership(&session_token, &room_token) {
        MembershipCheck::Superseded => return Err(Error::Superseded),
        MembershipCheck::InvalidSession => return Err(Error::InvalidSessionToken),
        MembershipCheck::Member(_) => return Err(Error::AlreadyJoinedRoom(room_name)),
        MembershipCheck::NotMember => {}
    }

    // Room-token collision: reject if a DIFFERENT session already holds this
    // member name in the room. join never supersedes a room-token holder.
    if app_write
        .rooms
        .get(room_token.as_str())
        .and_then(|rs| rs.members.get(&member_name))
        .is_some()
    {
        warn!(room = %room_name, member = %member_name, "join rejected — member name already in use");
        return Err(Error::MemberNameTaken(member_name));
    }

    let now = Instant::now();
    app_write.register_room_membership(
        RoomToken::from(room_token),
        member_name.clone(),
        &session_token,
        now,
        AuthMethod::RoomToken,
    );
    drop(app_write);

    info!(room = %room_name, member = %member_name, session = %session_id(&session_token), "member joined room via room token");

    Ok(JoinResult { room_name })
}

/// Attach a room membership to an existing session, using a seat token (member
/// credential). Preserves today's existing (and deliberate) asymmetry with the
/// room-token path: a seat-token collision unconditionally evicts the existing
/// holder's membership *in this room only* (`Superseded`) and registers the new
/// one, exactly like the historical seat-connect behavior — just invoked through
/// `join` (TDD 1b, 12.2).
pub async fn handle_join_seat(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    session_token: String,
    seat_token: String,
    member_name: String,
) -> Result<JoinResult, Error> {
    validate_member_name(&member_name)?;

    let (room_token, room_name) = {
        let store_read = store.read().await;
        let (room, seat) = store_read
            .seat_for_token(&seat_token)
            .ok_or(Error::InvalidSeatToken)?;

        if seat.member_name != member_name {
            warn!(
                expected = %seat.member_name,
                got = %member_name,
                "seat token presented for wrong member name"
            );
            return Err(Error::InvalidSeatToken);
        }
        if seat.is_expired() {
            warn!(member = %member_name, "seat token is expired");
            return Err(Error::InvalidSeatToken);
        }
        if room.is_expired() {
            warn!(room = %room.name, "room is expired during seat join");
            return Err(Error::RoomExpired(room.name.clone()));
        }
        debug!(room = %room.name, member = %member_name, "seat token verified for join");
        (room.token.clone(), room.name.clone())
    };

    let mut app_write = app.write().await;

    match app_write.check_room_membership(&session_token, room_token.as_str()) {
        MembershipCheck::Superseded => return Err(Error::Superseded),
        MembershipCheck::InvalidSession => return Err(Error::InvalidSessionToken),
        MembershipCheck::Member(_) => return Err(Error::AlreadyJoinedRoom(room_name)),
        MembershipCheck::NotMember => {}
    }

    // Seat-token collision: unconditionally evict the existing holder's
    // membership in this room only, carrying over its still-valid, room-tagged
    // queued messages (ANTI-PATTERNS #11) into the caller's own shared inbox.
    let is_active = app_write
        .rooms
        .get(&room_token)
        .and_then(|rs| rs.members.get(&member_name))
        .is_some();
    let carried = if is_active {
        let carried = app_write
            .evict_member_superseded(&room_token, &room_name, &member_name)
            .await;
        info!(
            room = %room_name,
            member = %member_name,
            session = %session_id(&session_token),
            carried = carried.len(),
            "prior room membership superseded by seat-holder join"
        );
        carried
    } else {
        Vec::new()
    };

    let now = Instant::now();
    app_write.register_room_membership(
        room_token,
        member_name.clone(),
        &session_token,
        now,
        AuthMethod::Seat,
    );
    let inbox = app_write.session_inbox(&session_token);
    drop(app_write);

    if !carried.is_empty() {
        if let Some(inbox) = inbox {
            inbox.seed(carried).await;
        }
    }

    info!(room = %room_name, member = %member_name, session = %session_id(&session_token), "member joined room via seat token");

    Ok(JoinResult { room_name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rooms::RoomStore, state::AppState, test_support::TEST_MASTER_SECRET,
        tools::connect::handle_connect,
    };
    use tempfile::TempDir;

    async fn make_store_with_room(dir: &TempDir) -> (Arc<RwLock<RoomStore>>, String) {
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        let room = store
            .create("testroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        (Arc::new(RwLock::new(store)), room.token.to_string())
    }

    #[tokio::test]
    async fn test_join_room_token_happy_path() {
        // Rubric 1b.1/1b.5: a valid session can join a room with a free name.
        let dir = TempDir::new().unwrap();
        let (store, room_token) = make_store_with_room(&dir).await;
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;

        let result =
            handle_join_room_token(&app, &store, session.clone(), room_token, "alice".into())
                .await
                .unwrap();
        assert_eq!(result.room_name, "testroom");
        assert!(app
            .read()
            .await
            .session_memberships(&session)
            .unwrap()
            .iter()
            .any(|(_, mn)| mn == "alice"));
    }

    #[tokio::test]
    async fn test_join_requires_valid_session_token() {
        // Rubric 1b.2/1b.6: join never silently mints a session — a missing or
        // invalid session_token is a hard InvalidSessionToken.
        let dir = TempDir::new().unwrap();
        let (store, room_token) = make_store_with_room(&dir).await;
        let app = AppState::new();

        let result = handle_join_room_token(
            &app,
            &store,
            "bogus-session".into(),
            room_token,
            "alice".into(),
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::InvalidSessionToken));
    }

    #[tokio::test]
    async fn test_join_room_token_name_taken_by_different_session() {
        // Rubric 1b.3: a second session cannot join under a name already active.
        let dir = TempDir::new().unwrap();
        let (store, room_token) = make_store_with_room(&dir).await;
        let app = AppState::new();

        let s1 = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(&app, &store, s1, room_token.clone(), "alice".into())
            .await
            .unwrap();

        let s2 = handle_connect(&app, None).await.unwrap().session_token;
        let result = handle_join_room_token(&app, &store, s2, room_token, "alice".into()).await;
        assert!(matches!(result.unwrap_err(), Error::MemberNameTaken(_)));
    }

    #[tokio::test]
    async fn test_join_invalid_room_token_rejected() {
        // Rubric 1b.4: an invalid/expired room token is rejected.
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        let store = Arc::new(RwLock::new(RoomStore::load(store_path).unwrap()));
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;

        let result =
            handle_join_room_token(&app, &store, session, "bad-token".into(), "alice".into()).await;
        assert!(matches!(result.unwrap_err(), Error::InvalidRoomToken));
    }

    #[tokio::test]
    async fn test_join_rejects_rejoining_own_room() {
        // Rubric 1b.7: rejoining a room the caller's own session already belongs
        // to is a descriptive error, not a silent no-op.
        let dir = TempDir::new().unwrap();
        let (store, room_token) = make_store_with_room(&dir).await;
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;

        handle_join_room_token(
            &app,
            &store,
            session.clone(),
            room_token.clone(),
            "alice".into(),
        )
        .await
        .unwrap();

        let result =
            handle_join_room_token(&app, &store, session, room_token, "alice-again".into()).await;
        assert!(matches!(result.unwrap_err(), Error::AlreadyJoinedRoom(_)));
    }

    #[tokio::test]
    async fn test_join_different_session_can_join_second_room() {
        // Rubric 1b.6: a different (invalid) session token is rejected even when
        // another session has already joined a room.
        let dir = TempDir::new().unwrap();
        let (store, room_token) = make_store_with_room(&dir).await;
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(&app, &store, session, room_token.clone(), "alice".into())
            .await
            .unwrap();

        let result = handle_join_room_token(
            &app,
            &store,
            "not-a-real-session".into(),
            room_token,
            "bob".into(),
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::InvalidSessionToken));
    }

    // ── Seat-token join ────────────────────────────────────────────────────────

    async fn make_store_with_seat(dir: &TempDir) -> (Arc<RwLock<RoomStore>>, String) {
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        store
            .create("testroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let (seat_token, _) = store
            .create_seat("testroom", "alice", 0, TEST_MASTER_SECRET)
            .await
            .unwrap();
        (Arc::new(RwLock::new(store)), seat_token.to_string())
    }

    #[tokio::test]
    async fn test_join_seat_happy_path() {
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;

        let result = handle_join_seat(&app, &store, session, seat_token, "alice".into())
            .await
            .unwrap();
        assert_eq!(result.room_name, "testroom");
    }

    #[tokio::test]
    async fn test_join_seat_evicts_prior_holder_only_in_that_room() {
        // Rubric 12.2 + 10.21: a seat-token join collision evicts the existing
        // holder's membership in this room only — a second, unrelated room
        // membership under the same victim session is left untouched.
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        // A second, independent room the victim also belongs to.
        let other_room_token = {
            let mut s = store.write().await;
            s.create("other-room".into(), 30, 50, TEST_MASTER_SECRET)
                .await
                .unwrap()
                .token
                .to_string()
        };

        let app = AppState::new();
        let victim_session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_seat(
            &app,
            &store,
            victim_session.clone(),
            seat_token.clone(),
            "alice".into(),
        )
        .await
        .unwrap();
        handle_join_room_token(
            &app,
            &store,
            victim_session.clone(),
            other_room_token.clone(),
            "alice-elsewhere".into(),
        )
        .await
        .unwrap();

        let new_session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_seat(
            &app,
            &store,
            new_session.clone(),
            seat_token,
            "alice".into(),
        )
        .await
        .unwrap();

        let state = app.read().await;
        // Victim's session survives (still has the other room) — not torn down,
        // and not marked Superseded (that flag is reserved for whole-session
        // teardown; see TDD 10.21).
        assert!(
            state.session_exists(&victim_session),
            "victim session must survive via its other room membership"
        );
        assert!(!state.superseded_sessions.contains(&victim_session));
        assert!(state
            .session_memberships(&victim_session)
            .unwrap()
            .iter()
            .any(|(_, mn)| mn == "alice-elsewhere"));
    }

    #[tokio::test]
    async fn test_join_seat_wrong_member_name_rejected() {
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;

        let result = handle_join_seat(&app, &store, session, seat_token, "bob".into()).await;
        assert!(matches!(result.unwrap_err(), Error::InvalidSeatToken));
    }

    #[tokio::test]
    async fn test_join_seat_rejects_rejoining_own_room() {
        // Rubric 1b.7 applies identically on the seat-token path.
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_seat(
            &app,
            &store,
            session.clone(),
            seat_token.clone(),
            "alice".into(),
        )
        .await
        .unwrap();

        let result = handle_join_seat(&app, &store, session, seat_token, "alice".into()).await;
        assert!(matches!(result.unwrap_err(), Error::AlreadyJoinedRoom(_)));
    }
}
