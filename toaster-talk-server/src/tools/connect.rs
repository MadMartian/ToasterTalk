use crate::{
    error::Error,
    state::{SessionToken, SharedAppState},
    tools::{new_session_token, session_id},
};
use tracing::info;

#[derive(Debug)]
pub struct ConnectResult {
    pub session_token: SessionToken,
}

/// Mint a session token — identity only, never a room membership (TDD 1a).
///
/// `connect` takes at most one optional input, `old_session_token`, and nothing
/// else: no room token, no seat token, no member name, ever. Every room
/// membership an agent ever holds — including its very first — is created
/// through `join`, not `connect`.
///
/// - No `old_session_token`: mints a fresh, roomless session token. Nothing to
///   register, nothing to evict (TDD 1a.1, 1a.2).
/// - `old_session_token` supplied and still valid: invalidates that session —
///   and *every* room membership under it — immediately, and mints a fresh,
///   roomless token in its place (TDD 1a.4). Deliberate no-carryover: a
///   bearer-only `session_token` presentation must never be sufficient, by
///   itself, to recover any room — recovery always goes back through `join`,
///   using the room or seat token the agent still holds. A stale/unrecognized
///   `old_session_token` is not an error — it is simply ignored, since it names
///   nothing worth invalidating and a fresh mint is issued regardless.
pub async fn handle_connect(
    app: &SharedAppState,
    old_session_token: Option<SessionToken>,
) -> Result<ConnectResult, Error> {
    let session_token = new_session_token();
    let sid = session_id(&session_token);

    let mut app_write = app.write().await;

    if let Some(old) = old_session_token.as_ref() {
        if app_write.session_exists(old) {
            app_write.invalidate_session_fully(old).await;
        }
    }

    app_write.register_session(session_token.clone());
    drop(app_write);

    info!(session = %sid, rotated = old_session_token.is_some(), "session minted");

    Ok(ConnectResult { session_token })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    #[tokio::test]
    async fn test_connect_mints_roomless_session() {
        // Rubric 1a.1: connect with no old_session_token mints a fresh, roomless
        // session token.
        let app = AppState::new();

        let result = handle_connect(&app, None).await.unwrap();
        assert_eq!(result.session_token.as_str().len(), 64);

        let state = app.read().await;
        assert!(state.sessions.contains_key(&result.session_token));
        assert!(state
            .session_memberships(&result.session_token)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn test_connect_post_restart_is_indistinguishable_from_first_connect() {
        // Rubric 1a.2: a "reconnect" after a server restart is just an ordinary
        // connect — there is no room state left in a fresh AppState to reconcile.
        let app = AppState::new();
        let result = handle_connect(&app, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_connect_old_session_token_invalidates_and_rotates() {
        // Rubric 1a.4: supplying a valid old_session_token invalidates it
        // immediately (and every room membership under it) and mints a fresh,
        // roomless token in its place.
        let app = AppState::new();

        let first = handle_connect(&app, None).await.unwrap().session_token;
        // Give the first session a room membership directly (join is tested
        // elsewhere) so we can prove it does NOT carry over.
        {
            let mut state = app.write().await;
            state.register_room_membership(
                crate::rooms::RoomToken::from("rt".to_string()),
                crate::test_support::mn("alice"),
                &first,
                std::time::Instant::now(),
                crate::state::AuthMethod::RoomToken,
            );
        }

        let second = handle_connect(&app, Some(first.clone()))
            .await
            .unwrap()
            .session_token;

        assert_ne!(first, second);
        let state = app.read().await;
        assert!(
            !state.sessions.contains_key(&first),
            "old session must be invalidated"
        );
        assert!(
            !state.rooms.get("rt").unwrap().members.contains_key("alice"),
            "old session's room membership must not survive rotation"
        );
        assert!(state.sessions.contains_key(&second));
        assert!(
            state.session_memberships(&second).unwrap().is_empty(),
            "the new session must be roomless — recovery goes back through join"
        );
    }

    #[tokio::test]
    async fn test_connect_unknown_old_session_token_is_ignored_not_an_error() {
        // A stale/unrecognized old_session_token names nothing to invalidate; connect
        // still succeeds with a fresh roomless mint rather than erroring.
        let app = AppState::new();
        let result = handle_connect(&app, Some("nonexistent-token".into())).await;
        assert!(result.is_ok());
    }
}
