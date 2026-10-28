use crate::{
    error::Error,
    rooms::{RoomStore, RoomToken},
    state::{
        AuthMethod, FlapOutcome, MemberName, MembershipCheck, SeatFlapPolicy, SessionToken,
        SharedAppState,
    },
    tools::session_id,
};
use std::{sync::Arc, time::Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, trace, warn};

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
///   here — that concept lives solely on `connect`, and never carries room state.
/// - The caller's own session already belongs to this room: rejected as
///   redundant with `AlreadyJoinedRoom`, not treated as a no-op (TDD 1b.7).
pub async fn handle_join_room_token(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    session_token: SessionToken,
    room_token: String,
    member_name: String,
) -> Result<JoinResult, Error> {
    let member_name = MemberName::new(member_name)?;

    // Lock order: RoomStore read guard FIRST, then the AppState write lock — the
    // same store → app nesting `LSRM` and the TTL sweep use; no path ever takes the
    // store while holding app state, so this cannot deadlock. The store guard is
    // deliberately held until the membership is registered: a room deletion
    // (`DELRM`, expiry sweep) needs the store WRITE lock, so it can no longer slip
    // in between "credential verified" and "membership registered" and leave a
    // membership attached to a room that no longer exists. The deletion instead
    // runs after this join completes and evicts the fresh membership like any
    // other. Only lock acquisition is awaited while the guard is held.
    let store_read = store.read().await;
    let room_name = {
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
    trace!(room = %room_name, member = %member_name, "join holding store read + app write locks");

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
        return Err(Error::MemberNameTaken(member_name.into_string()));
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
    drop(store_read);

    info!(room = %room_name, member = %member_name, session = %session_id(&session_token), "member joined room via room token");

    Ok(JoinResult { room_name })
}

/// Attach a room membership to an existing session, using a seat token (member
/// credential). Preserves today's existing (and deliberate) asymmetry with the
/// room-token path: a seat-token collision evicts the existing
/// holder's membership *in this room only* (`Superseded`) and registers the new
/// one, exactly like the historical seat-connect behavior — just invoked through
/// `join` (TDD 1b, 12.2). The one exception is a seat frozen by flap detection
/// (see [`handle_join_seat_with_policy`]).
///
/// Uses the default [`SeatFlapPolicy`]; the MCP server calls
/// [`handle_join_seat_with_policy`] with the policy from the live config.
pub async fn handle_join_seat(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    session_token: SessionToken,
    seat_token: String,
    member_name: String,
) -> Result<JoinResult, Error> {
    handle_join_seat_with_policy(
        app,
        store,
        SeatFlapPolicy::default(),
        session_token,
        seat_token,
        member_name,
    )
    .await
}

/// [`handle_join_seat`] with an explicit seat-contention flap policy (TDD 12.6,
/// 12.7). Every supersession is counted per seat; once the seat has been
/// superseded more than `policy.threshold` times within `policy.window`, it is
/// frozen for `policy.hold_down`: a seat-token join that would supersede the
/// current holder is rejected with `SeatContention` and the holder keeps its
/// membership, so two agents sharing one seat credential settle on one winner
/// instead of evicting each other forever. A frozen seat with no current holder
/// can still be claimed — the freeze only blocks supersession.
pub async fn handle_join_seat_with_policy(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    policy: SeatFlapPolicy,
    session_token: SessionToken,
    seat_token: String,
    member_name: String,
) -> Result<JoinResult, Error> {
    let member_name = MemberName::new(member_name)?;

    // Lock order and guard lifetime: see `handle_join_room_token`. Holding the
    // store read guard until registration also excludes a concurrent seat deletion
    // (`DELMB`, the expired-seat sweep), which needs the store write lock.
    let store_read = store.read().await;
    let (room_token, room_name) = {
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
    trace!(room = %room_name, member = %member_name, "seat join holding store read + app write locks");

    match app_write.check_room_membership(&session_token, room_token.as_str()) {
        MembershipCheck::Superseded => return Err(Error::Superseded),
        MembershipCheck::InvalidSession => return Err(Error::InvalidSessionToken),
        MembershipCheck::Member(_) => return Err(Error::AlreadyJoinedRoom(room_name)),
        MembershipCheck::NotMember => {}
    }

    // Seat-token collision: evict the existing holder's
    // membership in this room only, carrying over its still-valid, room-tagged
    // queued messages (ANTI-PATTERNS #11) into the caller's own shared inbox.
    let is_active = app_write
        .rooms
        .get(&room_token)
        .and_then(|rs| rs.members.get(&member_name))
        .is_some();
    let now = Instant::now();
    if is_active {
        if let Some(remaining) =
            app_write.seat_freeze_remaining(&room_token, &room_name, &member_name, now)
        {
            // Round up: a freeze with 0.4 s left must not tell the caller "0s".
            let retry_after_secs = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
            warn!(
                room = %room_name,
                member = %member_name,
                session = %session_id(&session_token),
                retry_after_secs,
                "seat join rejected — seat frozen for credential contention; current holder kept"
            );
            return Err(Error::SeatContention {
                room: room_name,
                member: member_name.into_string(),
                retry_after_secs,
            });
        }
    }
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
        if let FlapOutcome::Frozen { count, hold_down } =
            app_write.record_seat_supersede(&room_token, &room_name, &member_name, now, policy)
        {
            // The joiner that tripped the threshold becomes the frozen holder.
            info!(
                room = %room_name,
                member = %member_name,
                session = %session_id(&session_token),
                supersedes_in_window = count,
                hold_down_secs = hold_down.as_secs(),
                "seat frozen with this join's session as the holder"
            );
        }
        carried
    } else {
        Vec::new()
    };

    app_write.register_room_membership(
        room_token,
        member_name.clone(),
        &session_token,
        now,
        AuthMethod::Seat,
    );
    let inbox = app_write.session_inbox(&session_token);
    drop(app_write);
    drop(store_read);

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
    use crate::test_support::mn;
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

    // ── Seat-contention flap detection (TDD 12.6, 12.7) ───────────────────────

    /// Join the seat from a brand-new session (each thrashing agent reconnects
    /// with a fresh session after being superseded out of its last room).
    async fn seat_join_fresh(
        app: &SharedAppState,
        store: &Arc<RwLock<RoomStore>>,
        policy: SeatFlapPolicy,
        seat_token: &str,
    ) -> (SessionToken, Result<JoinResult, Error>) {
        let session = handle_connect(app, None).await.unwrap().session_token;
        let result = handle_join_seat_with_policy(
            app,
            store,
            policy,
            session.clone(),
            seat_token.to_string(),
            "alice".into(),
        )
        .await;
        (session, result)
    }

    fn holder_of(app: &AppState, room_token: &str) -> Option<SessionToken> {
        app.rooms
            .get(room_token)
            .and_then(|rs| rs.members.get("alice"))
            .map(|m| m.session_token.clone())
    }

    fn policy(threshold: u32, window_ms: u64, hold_down_ms: u64) -> SeatFlapPolicy {
        SeatFlapPolicy {
            threshold,
            window: std::time::Duration::from_millis(window_ms),
            hold_down: std::time::Duration::from_millis(hold_down_ms),
        }
    }

    #[tokio::test]
    async fn test_seat_thrash_freezes_seat_and_keeps_holder() {
        // Rubric 12.6: more than `threshold` supersessions inside the window
        // freezes the seat; the next superseding join is rejected with
        // SeatContention and the current holder keeps its membership.
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let room_token = store
            .read()
            .await
            .get_by_name("testroom")
            .unwrap()
            .token
            .to_string();
        let app = AppState::new();
        let p = policy(3, 60_000, 60_000);

        // Initial claim (no supersede) + 4 supersessions: the 4th trips 4 > 3.
        let mut last_holder = SessionToken::from(String::new());
        for _ in 0..5 {
            let (session, result) = seat_join_fresh(&app, &store, p, &seat_token).await;
            result.expect("joins up to and including the tripping one succeed");
            last_holder = session;
        }
        assert_eq!(
            holder_of(&*app.read().await, &room_token),
            Some(last_holder.clone())
        );

        let (_, result) = seat_join_fresh(&app, &store, p, &seat_token).await;
        match result.unwrap_err() {
            Error::SeatContention {
                room,
                member,
                retry_after_secs,
            } => {
                assert_eq!(room, "testroom");
                assert_eq!(member, "alice");
                assert!((1..=60).contains(&retry_after_secs));
            }
            other => panic!("expected SeatContention, got {other:?}"),
        }
        // The livelock is broken with one winner: the holder was not evicted.
        assert_eq!(
            holder_of(&*app.read().await, &room_token),
            Some(last_holder)
        );
    }

    #[tokio::test]
    async fn test_seat_freeze_lapses_after_hold_down() {
        // Rubric 12.7: once the hold-down lapses, a superseding join is accepted
        // again and counting restarts from zero.
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let app = AppState::new();
        let p = policy(1, 60_000, 50);

        for _ in 0..3 {
            seat_join_fresh(&app, &store, p, &seat_token)
                .await
                .1
                .unwrap();
        }
        let (_, frozen) = seat_join_fresh(&app, &store, p, &seat_token).await;
        assert!(matches!(frozen.unwrap_err(), Error::SeatContention { .. }));

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let (session, result) = seat_join_fresh(&app, &store, p, &seat_token).await;
        result.expect("freeze lapsed — superseding join must be accepted");
        let room_token = store
            .read()
            .await
            .get_by_name("testroom")
            .unwrap()
            .token
            .to_string();
        assert_eq!(holder_of(&*app.read().await, &room_token), Some(session));
    }

    #[tokio::test]
    async fn test_seat_flap_detection_disabled_by_zero_threshold() {
        // Rubric 12.6 (opt-out): threshold 0 never freezes.
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let app = AppState::new();
        let p = policy(0, 60_000, 60_000);
        for _ in 0..10 {
            seat_join_fresh(&app, &store, p, &seat_token)
                .await
                .1
                .unwrap();
        }
        assert!(app
            .read()
            .await
            .seat_flaps
            .values()
            .all(|f| f.frozen_until.is_none()));
    }

    #[tokio::test]
    async fn test_frozen_seat_without_holder_can_be_claimed() {
        // Rubric 12.6: the freeze only blocks supersession — if the holder has
        // left, the seat can be claimed without displacing anyone.
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let room_token = store
            .read()
            .await
            .get_by_name("testroom")
            .unwrap()
            .token
            .to_string();
        let app = AppState::new();
        let p = policy(1, 60_000, 60_000);
        for _ in 0..3 {
            seat_join_fresh(&app, &store, p, &seat_token)
                .await
                .1
                .unwrap();
        }
        assert!(
            app.write()
                .await
                .evict_member(&room_token, "testroom", &mn("alice"), "test: holder left")
                .await
        );
        seat_join_fresh(&app, &store, p, &seat_token)
            .await
            .1
            .expect("an unheld frozen seat must still be claimable");
    }

    #[tokio::test]
    async fn test_operator_clear_unfreezes_seat() {
        // Rubric 12.7: the operator clear (KCKM / DELMB path) drops the freeze.
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let room_token = store
            .read()
            .await
            .get_by_name("testroom")
            .unwrap()
            .token
            .to_string();
        let app = AppState::new();
        let p = policy(1, 60_000, 60_000);
        for _ in 0..3 {
            seat_join_fresh(&app, &store, p, &seat_token)
                .await
                .1
                .unwrap();
        }
        assert!(app
            .write()
            .await
            .clear_seat_flap(&room_token, &mn("alice"), Instant::now()));
        seat_join_fresh(&app, &store, p, &seat_token)
            .await
            .1
            .expect("cleared seat must accept a superseding join");
    }

    #[tokio::test]
    async fn test_prune_drops_stale_flap_history_only() {
        // Every piece of server data has a TTL: the sweep prunes a history that is
        // neither frozen nor inside its window, and keeps a frozen one.
        let app = AppState::new();
        let mut a = app.write().await;
        let now = Instant::now();
        a.record_seat_supersede("rt", "room", &mn("stale"), now, policy(5, 10, 10));
        a.record_seat_supersede("rt", "room", &mn("frozen"), now, policy(0, 10, 10));
        a.record_seat_supersede("rt", "room", &mn("frozen"), now, policy(1, 10, 60_000));
        let later = now + std::time::Duration::from_millis(100);
        assert_eq!(a.prune_seat_flaps(later), 1);
        assert!(a
            .seat_flaps
            .contains_key(&(RoomToken::from("rt".to_string()), mn("frozen"))));
    }

    // ── Join vs. concurrent delete ────────────────────────────────────────────

    /// Spawn `join_fut`, let it run until it is parked on the AppState write lock
    /// (held by the caller), then report whether a deletion's first step — taking
    /// the RoomStore write lock — is excluded while the join is mid-flight.
    async fn store_write_excluded_during_join(store: &Arc<RwLock<RoomStore>>) -> bool {
        // Current-thread runtime: each yield lets the spawned join run until it
        // blocks; it needs no more than one, the bound is just a safety margin.
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if store.try_write().is_err() {
                return true;
            }
        }
        false
    }

    #[tokio::test]
    async fn test_room_token_join_excludes_concurrent_room_delete() {
        // Rubric 1b.8: a room deletion can no longer slip in between join's
        // credential check and its membership registration. With the AppState
        // write lock held by the test, the join is parked exactly in that window;
        // the deletion's store write must be excluded until the join finishes, so
        // the deletion then evicts the new membership instead of leaving it
        // attached to a room that no longer exists.
        let dir = TempDir::new().unwrap();
        let (store, room_token) = make_store_with_room(&dir).await;
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;

        let app_guard = app.write().await;
        let join = tokio::spawn({
            let (app, store, session, room_token) = (
                app.clone(),
                store.clone(),
                session.clone(),
                room_token.clone(),
            );
            async move {
                handle_join_room_token(&app, &store, session, room_token, "alice".into()).await
            }
        });
        assert!(
            store_write_excluded_during_join(&store).await,
            "a room delete must not be able to take the store write lock while a join \
             that has verified the room token is still registering"
        );
        drop(app_guard);
        join.await.unwrap().unwrap();

        // The deletion now runs exactly as DELRM does: store first, then evict.
        store.write().await.delete("testroom").await.unwrap();
        app.write()
            .await
            .evict_room(&room_token, "testroom", "room deleted")
            .await;
        let a = app.read().await;
        assert!(!a.rooms.contains_key(room_token.as_str()));
        assert!(
            !a.session_exists(&session),
            "no membership may survive the delete"
        );
    }

    #[tokio::test]
    async fn test_seat_join_excludes_concurrent_seat_delete() {
        // Rubric 1b.8, seat path: same window, with a seat deletion (DELMB).
        let dir = TempDir::new().unwrap();
        let (store, seat_token) = make_store_with_seat(&dir).await;
        let room_token = store
            .read()
            .await
            .get_by_name("testroom")
            .unwrap()
            .token
            .clone();
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;

        let app_guard = app.write().await;
        let join = tokio::spawn({
            let (app, store, session, seat_token) = (
                app.clone(),
                store.clone(),
                session.clone(),
                seat_token.clone(),
            );
            async move { handle_join_seat(&app, &store, session, seat_token, "alice".into()).await }
        });
        assert!(
            store_write_excluded_during_join(&store).await,
            "a seat delete must not be able to take the store write lock while a seat join \
             that has verified the seat token is still registering"
        );
        drop(app_guard);
        join.await.unwrap().unwrap();

        store
            .write()
            .await
            .delete_seat("testroom", "alice")
            .await
            .unwrap();
        app.write()
            .await
            .evict_member_superseded(&room_token, "testroom", &mn("alice"))
            .await;
        assert!(holder_of(&*app.read().await, room_token.as_str()).is_none());
        assert!(!app.read().await.session_exists(&session));
    }
}
