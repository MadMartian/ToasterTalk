//! Shared helpers for integration test crates.
#![allow(dead_code)]

use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;
use toaster_talk_server::{
    config::{Config, MasterSecret},
    rooms::RoomStore,
    state::{AppState, SharedAppState},
    tools::{connect::handle_connect, join::handle_join_room_token},
};
use tokio::sync::RwLock;

pub const TEST_MASTER_SECRET: &[u8] = b"test-secret-for-toaster-talk-unit-tests-ok!!";

// Room-config test defaults — single source of truth so `test_config`,
// `create_room`, and `spawn_mgmt` cannot drift apart (L-R7-DRY2).
pub const TEST_MAX_ROOMS: usize = 50;
pub const TEST_DEFAULT_ROOM_TTL_DAYS: u64 = 30;
pub const TEST_MAX_ROOM_TTL_DAYS: u64 = 365;

pub fn test_config() -> Config {
    Config {
        bind: "127.0.0.1".into(),
        port: 0,
        log_level: "error".into(),
        store_path: PathBuf::from("/tmp/rooms-test.yaml"),
        max_rooms: TEST_MAX_ROOMS,
        default_room_ttl_days: TEST_DEFAULT_ROOM_TTL_DAYS,
        max_room_ttl_days: TEST_MAX_ROOM_TTL_DAYS,
        default_message_ttl_secs: 300,
        max_message_ttl_secs: 3600,
        sweep_interval_secs: 60,
        idle_threshold_secs: 300,
        member_idle_ttl_secs: 1800,
        seat_idle_ttl_secs: 0, // reaper disabled by default in integration tests
        default_receive_timeout_secs: 30,
        max_await_reply_secs: 3600,
        max_receive_timeout_secs: 3600,
        max_inbox_messages: 100,
        mgmt_socket: PathBuf::from("/tmp/mgmt-test.sock"),
        master_secret: MasterSecret::new(TEST_MASTER_SECRET),
    }
}

pub fn make_store(dir: &TempDir) -> Arc<RwLock<RoomStore>> {
    Arc::new(RwLock::new(
        RoomStore::load(dir.path().join("rooms.yaml")).unwrap(),
    ))
}

pub async fn create_room(store: &Arc<RwLock<RoomStore>>, name: &str) -> String {
    store
        .write()
        .await
        .create(
            name.into(),
            TEST_DEFAULT_ROOM_TTL_DAYS,
            TEST_MAX_ROOMS,
            TEST_MASTER_SECRET,
        )
        .await
        .unwrap()
        .token
        .to_string()
}

/// Connect (mint a fresh, roomless session) and then join `room_token` as
/// `member` — the two-step flow every room membership now goes through
/// (PLAN.multi-receive.md). Kept under the historical name `join` since nearly
/// every integration test uses it purely as "get me a session in this room."
pub async fn join(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    room_token: &str,
    member: &str,
) -> String {
    let session_token = handle_connect(app, None).await.unwrap().session_token;
    handle_join_room_token(
        app,
        store,
        session_token.clone(),
        room_token.to_string(),
        member.to_string(),
    )
    .await
    .unwrap();
    session_token
}

pub fn make_app() -> SharedAppState {
    AppState::new()
}

/// Address for the management transport in tests: a Unix domain socket path
/// under `dir` on Unix; a uniquely-named pipe on Windows, where named pipes
/// live in a global namespace rather than the filesystem, so the name is
/// derived from `dir`'s own random suffix to stay unique across parallel tests.
#[cfg(unix)]
pub fn test_mgmt_addr(dir: &TempDir) -> PathBuf {
    dir.path().join("mgmt.sock")
}
#[cfg(windows)]
pub fn test_mgmt_addr(dir: &TempDir) -> PathBuf {
    let unique = dir.path().file_name().unwrap().to_string_lossy();
    PathBuf::from(format!(r"\\.\pipe\toaster-talk-test-{unique}"))
}

/// Connect to the management transport (a Unix socket on Unix, a named pipe on
/// Windows), send one request, collect all response lines until the server
/// closes the connection, and return them joined by newlines.
pub async fn mgmt_request(mgmt_addr: &std::path::Path, request: &str) -> String {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stream = connect_mgmt(mgmt_addr).await;
    let (reader, mut writer) = tokio::io::split(stream);

    writer
        .write_all(format!("{request}\r\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();
    drop(writer); // signal EOF so the server can close after responding

    let mut lines = BufReader::new(reader).lines();
    let mut response_lines = Vec::new();
    while let Ok(Some(l)) = lines.next_line().await {
        response_lines.push(l.trim_end_matches('\r').to_string());
    }
    response_lines.join("\n")
}

#[cfg(unix)]
async fn connect_mgmt(addr: &std::path::Path) -> tokio::net::UnixStream {
    tokio::net::UnixStream::connect(addr).await.unwrap()
}
#[cfg(windows)]
async fn connect_mgmt(addr: &std::path::Path) -> tokio::net::windows::named_pipe::NamedPipeClient {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = addr.to_string_lossy().into_owned();
    // ERROR_PIPE_BUSY (231) — mirrors toaster-talk-cli's retry loop; the listener
    // always keeps one spare instance ready, so a couple of retries suffice.
    const ERROR_PIPE_BUSY: i32 = 231;
    for _ in 0..50 {
        match ClientOptions::new().open(&name) {
            Ok(c) => return c,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => panic!("could not connect to test mgmt pipe {name}: {e}"),
        }
    }
    panic!("could not connect to test mgmt pipe {name}: still busy after retries");
}

/// Spawn the management listener with test defaults and wait for it to bind.
/// Reload triggers (the mgmt `RELOAD` command, TDD 8.17) are auto-acknowledged
/// immediately, so tests that don't care about reload timing get a working
/// `RELOAD` round trip for free. Use `spawn_mgmt_with_reload` to control the
/// acknowledgement yourself.
pub async fn spawn_mgmt(
    socket_path: &std::path::Path,
    store: &Arc<RwLock<RoomStore>>,
    app: &SharedAppState,
) {
    let (reload_tx, mut reload_rx) =
        tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<()>>();
    tokio::spawn(async move {
        while let Some(ack) = reload_rx.recv().await {
            let _ = ack.send(());
        }
    });
    spawn_mgmt_with_reload(socket_path, store, app, reload_tx).await;
}

/// Spawn the management listener with test defaults and wait for it to bind,
/// wiring the mgmt `RELOAD` command (TDD 8.17) to the caller-supplied trigger
/// channel instead of auto-acknowledging it.
pub async fn spawn_mgmt_with_reload(
    socket_path: &std::path::Path,
    store: &Arc<RwLock<RoomStore>>,
    app: &SharedAppState,
    reload_tx: toaster_talk_server::mgmt::ReloadSender,
) {
    use arc_swap::ArcSwap;
    use toaster_talk_server::mgmt::run_mgmt_listener;
    tokio::spawn(run_mgmt_listener(
        socket_path.to_path_buf(),
        store.clone(),
        app.clone(),
        Arc::new(ArcSwap::from_pointee(test_config())),
        reload_tx,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}
