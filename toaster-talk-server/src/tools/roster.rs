use crate::{error::Error, rooms::RoomStore, state::SharedAppState, tools::session_id};
use std::{sync::Arc, time::Instant};
use tokio::sync::RwLock;
use tracing::info;

#[derive(Debug)]
pub struct RosterResult {
    /// The authoritative room name (taken from the store, not from caller input).
    pub room_name: String,
    /// Member names currently registered in the room, sorted for stable output.
    pub members: Vec<String>,
}

/// List the members of a room the caller's session belongs to (TDD 5.3).
///
/// A session can belong to more than one room, so this is a set-membership check
/// (is `room_name` one of the rooms this session has joined?), not equality against
/// a single resolved room. A room the caller has not joined is rejected with
/// `NotConnectedToRoom` rather than silently listing it — there is no way to
/// inspect a room the caller is not itself a member of.
pub async fn handle_members(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    session_token: String,
    room_name: String,
) -> Result<RosterResult, Error> {
    // Resolve the room name to its token up front (rubric: an unknown room name
    // is a descriptive error, not a silent empty roster).
    let room_token = {
        let store_read = store.read().await;
        store_read
            .get_by_name(&room_name)
            .map(|r| r.token.clone())
            .ok_or_else(|| Error::RoomNotFound(room_name.clone()))?
    };

    let mut members = {
        let mut app_write = app.write().await;

        // Consumes a one-time `Superseded` flag if present, so a session displaced
        // by a seat-holder is told to reconnect — exactly like send/receive. Also
        // enforces set-membership: NotConnectedToRoom if this session has not
        // joined `room_name`.
        let (member_name, room_state) =
            app_write.room_membership_mut(&session_token, room_token.as_str(), &room_name)?;

        // Listing the roster is member activity; refresh this room's last_seen.
        if let Some(me) = room_state.members.get_mut(&member_name) {
            me.last_seen = Instant::now();
        }

        let members: Vec<String> = room_state.members.keys().cloned().collect();
        members
    }; // app lock released

    members.sort();

    info!(
        room = %room_name,
        session = %session_id(&session_token),
        member_count = members.len(),
        "members served"
    );

    Ok(RosterResult { room_name, members })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rooms::RoomStore,
        state::AppState,
        test_support::TEST_MASTER_SECRET,
        tools::{connect::handle_connect, join::handle_join_room_token},
    };
    use tempfile::TempDir;

    async fn setup(dir: &TempDir) -> (SharedAppState, Arc<RwLock<RoomStore>>, String) {
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        let room = store
            .create("testroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let token = room.token.to_string();
        let store = Arc::new(RwLock::new(store));
        let app = AppState::new();

        let s1 = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(&app, &store, s1, token.clone(), "alice".into())
            .await
            .unwrap();
        let s2 = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(&app, &store, s2, token.clone(), "bob".into())
            .await
            .unwrap();
        let carol = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(&app, &store, carol.clone(), token, "carol".into())
            .await
            .unwrap();
        (app, store, carol)
    }

    #[tokio::test]
    async fn test_members_returns_sorted_roster() {
        // Rubric 5.3: a connected member sees every member of its own room.
        let dir = TempDir::new().unwrap();
        let (app, store, carol_session) = setup(&dir).await;

        let result = handle_members(&app, &store, carol_session, "testroom".into())
            .await
            .unwrap();

        assert_eq!(result.room_name, "testroom");
        assert_eq!(result.members, vec!["alice", "bob", "carol"]);
    }

    #[tokio::test]
    async fn test_members_rejects_room_not_joined() {
        // Rubric 5.3: naming a room the session does not belong to is rejected — an
        // agent cannot enumerate a room it has not joined.
        let dir = TempDir::new().unwrap();
        let (app, store, carol_session) = setup(&dir).await;
        store
            .write()
            .await
            .create("other-room".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();

        let err = handle_members(&app, &store, carol_session, "other-room".into())
            .await
            .unwrap_err();

        assert!(matches!(err, Error::NotConnectedToRoom(r) if r == "other-room"));
    }

    #[tokio::test]
    async fn test_members_rejects_unknown_room_name() {
        let dir = TempDir::new().unwrap();
        let (app, store, carol_session) = setup(&dir).await;

        let err = handle_members(&app, &store, carol_session, "no-such-room".into())
            .await
            .unwrap_err();

        assert!(matches!(err, Error::RoomNotFound(_)));
    }

    #[tokio::test]
    async fn test_members_rejects_invalid_session() {
        let dir = TempDir::new().unwrap();
        let (app, store, _carol_session) = setup(&dir).await;

        let err = handle_members(&app, &store, "not-a-session".into(), "testroom".into())
            .await
            .unwrap_err();

        assert!(matches!(err, Error::InvalidSessionToken));
    }

    #[tokio::test]
    async fn test_members_set_membership_allows_multiple_rooms() {
        // TDD 5.3 (multi-room): a session that has joined two rooms can list
        // either one, not just "the" one room.
        let dir = TempDir::new().unwrap();
        let (app, store, carol_session) = setup(&dir).await;
        let second_room_token = {
            let mut s = store.write().await;
            s.create("second-room".into(), 30, 50, TEST_MASTER_SECRET)
                .await
                .unwrap()
                .token
                .to_string()
        };
        handle_join_room_token(
            &app,
            &store,
            carol_session.clone(),
            second_room_token,
            "carol2".into(),
        )
        .await
        .unwrap();

        let first = handle_members(&app, &store, carol_session.clone(), "testroom".into())
            .await
            .unwrap();
        assert_eq!(first.members, vec!["alice", "bob", "carol"]);

        let second = handle_members(&app, &store, carol_session, "second-room".into())
            .await
            .unwrap();
        assert_eq!(second.members, vec!["carol2"]);
    }
}
