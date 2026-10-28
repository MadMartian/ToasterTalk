use crate::{
    error::Error,
    rooms::RoomStore,
    state::{MemberName, MembershipCheck, SessionToken, SharedAppState},
    tools::session_id,
};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

#[derive(Debug)]
pub struct LeaveResult {
    pub room_name: String,
    pub member_name: MemberName,
}

/// Voluntarily leave a room the caller's session has joined — the inverse of
/// `join` (TDD 1c). Removes only this one room membership via the same
/// `evict_member`/`evict_one` primitive every other eviction path uses
/// (`kckm`, idle TTL, room expiry, seat supersede): if this was the session's
/// last remaining membership, the whole session and its shared inbox are torn
/// down (TDD 1c.3); otherwise the session and its other room memberships are
/// left completely unaffected (TDD 1c.2, mirrors 10.21). Leaving does not touch
/// any persisted seat credential in `RoomStore` — a seat holder can `join` the
/// same room again later using the same seat token.
pub async fn handle_leave(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    session_token: SessionToken,
    room_name: String,
) -> Result<LeaveResult, Error> {
    let room_token = {
        let store_read = store.read().await;
        store_read
            .get_by_name(&room_name)
            .map(|r| r.token.clone())
            .ok_or_else(|| Error::RoomNotFound(room_name.clone()))?
    };

    let member_name = {
        let mut app_write = app.write().await;

        let member_name = match app_write.check_room_membership(&session_token, room_token.as_str())
        {
            MembershipCheck::Superseded => return Err(Error::Superseded),
            MembershipCheck::InvalidSession => return Err(Error::InvalidSessionToken),
            MembershipCheck::NotMember => return Err(Error::NotConnectedToRoom(room_name.clone())),
            MembershipCheck::Member(member_name) => member_name,
        };

        app_write
            .evict_member(
                room_token.as_str(),
                &room_name,
                &member_name,
                "voluntary leave",
            )
            .await;

        member_name
    };

    info!(
        room = %room_name,
        member = %member_name,
        session = %session_id(&session_token),
        "member left room voluntarily"
    );

    Ok(LeaveResult {
        room_name,
        member_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::mn;
    use crate::{
        rooms::RoomStore,
        state::AppState,
        test_support::TEST_MASTER_SECRET,
        tools::{connect::handle_connect, join::handle_join_room_token, join::handle_join_seat},
    };
    use tempfile::TempDir;

    async fn setup_room(
        dir: &TempDir,
    ) -> (
        SharedAppState,
        Arc<RwLock<RoomStore>>,
        SessionToken,
        MemberName,
    ) {
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        let room = store
            .create("testroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let token = room.token.to_string();
        let store = Arc::new(RwLock::new(store));
        let app = AppState::new();

        let session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(&app, &store, session.clone(), token, "alice".into())
            .await
            .unwrap();
        (app, store, session, mn("alice"))
    }

    #[tokio::test]
    async fn test_leave_happy_path_removes_membership() {
        // Rubric 1c.1: leaving a joined room removes the membership and reports
        // which room and member name were removed.
        let dir = TempDir::new().unwrap();
        let (app, store, session, member_name) = setup_room(&dir).await;

        let result = handle_leave(&app, &store, session.clone(), "testroom".into())
            .await
            .unwrap();
        assert_eq!(result.room_name, "testroom");
        assert_eq!(result.member_name, member_name);

        // The session held only this one membership, so it is fully torn down.
        assert!(!app.read().await.session_exists(&session));
    }

    #[tokio::test]
    async fn test_leave_last_membership_tears_down_session() {
        // Rubric 1c.3: leaving the session's only room membership tears down the
        // whole session (and its shared inbox), same as any other last-membership
        // eviction.
        let dir = TempDir::new().unwrap();
        let (app, store, session, _member_name) = setup_room(&dir).await;

        handle_leave(&app, &store, session.clone(), "testroom".into())
            .await
            .unwrap();

        let state = app.read().await;
        assert!(!state.session_exists(&session));
        assert!(state.session_inbox(&session).is_none());
    }

    #[tokio::test]
    async fn test_leave_one_of_two_rooms_leaves_others_untouched() {
        // Rubric 1c.2 (mirrors TDD 10.21): leaving one room membership does not
        // affect the session's other room memberships.
        let dir = TempDir::new().unwrap();
        let (app, store, session, _member_name) = setup_room(&dir).await;
        let other_room_token = {
            let mut s = store.write().await;
            s.create("other-room".into(), 30, 50, TEST_MASTER_SECRET)
                .await
                .unwrap()
                .token
                .to_string()
        };
        handle_join_room_token(
            &app,
            &store,
            session.clone(),
            other_room_token,
            "alice-elsewhere".into(),
        )
        .await
        .unwrap();

        handle_leave(&app, &store, session.clone(), "testroom".into())
            .await
            .unwrap();

        let state = app.read().await;
        assert!(
            state.session_exists(&session),
            "session must survive via its other room"
        );
        assert!(state
            .session_memberships(&session)
            .unwrap()
            .iter()
            .any(|(_, mn)| mn == "alice-elsewhere"));
        assert!(!state
            .session_memberships(&session)
            .unwrap()
            .iter()
            .any(|(_, mn)| mn == "alice"));
    }

    #[tokio::test]
    async fn test_leave_rejects_room_not_joined() {
        // Rubric 1c.4: leaving a room the session has not joined is rejected, not
        // a silent no-op.
        let dir = TempDir::new().unwrap();
        let (app, store, session, _member_name) = setup_room(&dir).await;
        store
            .write()
            .await
            .create("other-room".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();

        let err = handle_leave(&app, &store, session, "other-room".into())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotConnectedToRoom(r) if r == "other-room"));
    }

    #[tokio::test]
    async fn test_leave_rejects_unknown_room_name() {
        // Rubric 1c.5: an unknown room name is a descriptive error.
        let dir = TempDir::new().unwrap();
        let (app, store, session, _member_name) = setup_room(&dir).await;

        let err = handle_leave(&app, &store, session, "no-such-room".into())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::RoomNotFound(_)));
    }

    #[tokio::test]
    async fn test_leave_rejects_invalid_session() {
        // Rubric 1c.6: a missing/invalid session_token is a hard error.
        let dir = TempDir::new().unwrap();
        let (app, store, _session, _member_name) = setup_room(&dir).await;

        let err = handle_leave(&app, &store, "not-a-session".into(), "testroom".into())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidSessionToken));
    }

    #[tokio::test]
    async fn test_leave_then_rejoin_via_seat_token_succeeds() {
        // Leaving does not touch the persisted seat credential — a seat holder
        // can rejoin the same room afterward using the same seat token.
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        store
            .create("seatroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let (seat_token, _) = store
            .create_seat("seatroom", "alice", 0, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let store = Arc::new(RwLock::new(store));
        let app = AppState::new();

        let session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_seat(
            &app,
            &store,
            session.clone(),
            seat_token.to_string(),
            "alice".into(),
        )
        .await
        .unwrap();

        handle_leave(&app, &store, session.clone(), "seatroom".into())
            .await
            .unwrap();

        let session2 = handle_connect(&app, None).await.unwrap().session_token;
        let result = handle_join_seat(
            &app,
            &store,
            session2,
            seat_token.to_string(),
            "alice".into(),
        )
        .await
        .unwrap();
        assert_eq!(result.room_name, "seatroom");
    }
}
