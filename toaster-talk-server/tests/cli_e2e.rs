//! End-to-end CLI tests: spawn `toaster-talk-cli` as a subprocess against a
//! live management socket and verify filesystem and server state.
//!
//! These tests require the CLI binary to be compiled. Run with:
//!   cargo test --workspace
//! or build first:
//!   cargo build --workspace && cargo test -p toaster-talk-server
//!
//! Test → rubric mapping:
//!   test_delrm_leaves_token_file_intact    → TDD 10.5
//!   test_lsrm_timestamps_are_human_readable → TDD 10.6
//!   test_kckm_removes_member               → TDD 10.8
//!   test_kckm_removes_multiple_members     → TDD 10.8
//!   test_kckall_removes_all_members        → TDD 10.9
//!   test_addrm_addmb_batch_*, test_delrm_delmb_kckall_batch_* → TDD 10.23–10.25

mod common;

use toaster_talk_common::sanitize_filename;
use toaster_talk_server::{
    state::AppState,
    tools::{connect::handle_connect, join::handle_join_room_token},
};
// Shared test helpers (M-R6-B1): make_store / create_room / spawn_mgmt live in
// tests/common so every integration test file uses one definition.
use common::{create_room, make_store, spawn_mgmt};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn cli_bin() -> PathBuf {
    // CARGO_MANIFEST_DIR is .../toaster-talk-server; the CLI binary sits in the
    // workspace-level target directory. Pick the profile subdirectory matching how
    // these tests were built so `cargo test --release` (the project build pipeline)
    // spawns the freshly-built release CLI, not a stale debug one.
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let bin_name = if cfg!(windows) {
        "toaster-talk-cli.exe"
    } else {
        "toaster-talk-cli"
    };
    manifest
        .parent()
        .unwrap()
        .join(format!("target/{profile}"))
        .join(bin_name)
}

/// Write a token file at the expected path (mirrors the CLI's write_token_file logic).
fn write_token_file(token_dir: &Path, name: &str, token: &str) -> PathBuf {
    std::fs::create_dir_all(token_dir).unwrap();
    let base = sanitize_filename(name);
    let path = token_dir.join(format!("{base}.tok"));
    let content = format!("{name}\n{token}\n");
    open_token_file(&path)
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    path
}

#[cfg(unix)]
fn open_token_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
#[cfg(windows)]
fn open_token_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

// ── TDD Rubric 10.5 ───────────────────────────────────────────────────────────

/// Rubric 10.5: When a room is deleted via `toaster-talk-cli delrm`, the server
/// invalidates the token but the local token file is left intact on disk.
#[tokio::test]
async fn test_delrm_leaves_token_file_intact() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);
    let token_dir = dir.path().join("tokens");

    // Pre-create the room and a corresponding token file.
    let room_token = create_room(&store, "keep-file").await;
    let tok_path = write_token_file(&token_dir, "keep-file", &room_token);
    assert!(tok_path.exists(), "token file must exist before deletion");

    spawn_mgmt(&socket_path, &store, &app).await;

    // Invoke the CLI binary as a subprocess.
    let status = tokio::process::Command::new(cli_bin())
        .args([
            "--socket",
            socket_path.to_str().unwrap(),
            "delrm",
            "keep-file",
        ])
        .status()
        .await
        .expect("failed to spawn toaster-talk-cli — run `cargo build --workspace` first");

    assert!(status.success(), "CLI delrm must exit 0");

    // Server must have invalidated the token.
    assert!(
        store.read().await.verify_token(&room_token).is_none(),
        "room token must be invalidated after delrm"
    );

    // Token file must still be on disk — the CLI must not remove it.
    assert!(
        tok_path.exists(),
        "token file must NOT be removed by delrm (rubric 10.5)"
    );
}

// ── TDD Rubric 10.6 ───────────────────────────────────────────────────────────

/// Rubric 10.6: timestamps in CLI output must be human-readable, not raw Unix
/// integer strings.
#[tokio::test]
async fn test_lsrm_timestamps_are_human_readable() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);

    create_room(&store, "ts-test-room").await;

    spawn_mgmt(&socket_path, &store, &app).await;

    let output = tokio::process::Command::new(cli_bin())
        .args(["--socket", socket_path.to_str().unwrap(), "lsrm"])
        .output()
        .await
        .expect("failed to spawn toaster-talk-cli — run `cargo build --workspace` first");

    assert!(output.status.success(), "CLI lsrm must exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed
            .strip_prefix("Created:")
            .or_else(|| trimmed.strip_prefix("Expires:"))
        {
            let val = val.trim();
            assert!(
                !val.chars().all(|c| c.is_ascii_digit()),
                "timestamp must not be a raw Unix integer; got: {val:?}"
            );
            // Expect "YYYY-MM-DD HH:MM ±HHMM" — contains hyphens and a UTC offset.
            assert!(
                val.contains('-') && (val.contains('+') || val.ends_with(|c: char| c.is_ascii_digit())),
                "timestamp must look like a formatted local date (YYYY-MM-DD HH:MM ±HHMM); got: {val:?}"
            );
        }
    }
}

// ── TDD Rubric 10.8 ───────────────────────────────────────────────────────────

/// Rubric 10.8: kckm removes a named member and invalidates their session,
/// leaving the room and any other members intact.
#[tokio::test]
async fn test_kckm_removes_member() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);

    let room_token = create_room(&store, "kick-room").await;
    let alice_session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_room_token(
        &app,
        &store,
        alice_session.clone(),
        room_token.clone(),
        "alice".into(),
    )
    .await
    .unwrap();
    let bob_session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_room_token(
        &app,
        &store,
        bob_session.clone(),
        room_token.clone(),
        "bob".into(),
    )
    .await
    .unwrap();

    spawn_mgmt(&socket_path, &store, &app).await;

    let status = tokio::process::Command::new(cli_bin())
        .args([
            "--socket",
            socket_path.to_str().unwrap(),
            "kckm",
            "kick-room",
            "alice",
        ])
        .status()
        .await
        .expect("failed to spawn toaster-talk-cli — run `cargo build --workspace` first");

    assert!(status.success(), "CLI kckm must exit 0");

    let app_read = app.read().await;
    let room_state = app_read
        .rooms
        .get(room_token.as_str())
        .expect("room must still exist after kckm");

    assert!(
        !room_state.members.contains_key("alice"),
        "alice must be removed from room after kckm"
    );
    assert!(
        room_state.members.contains_key("bob"),
        "bob must remain in room after kckm"
    );

    // M-R5-T02: the kick must invalidate alice's session, not just remove her
    // from the room — a stale session token must no longer resolve.
    assert!(
        !app_read.sessions.contains_key(&alice_session),
        "alice's session must be invalidated after kckm"
    );
    assert!(
        app_read.sessions.contains_key(&bob_session),
        "bob's session must remain valid after kckm"
    );
}

/// Rubric 10.8 (multi-member): a single `kckm` with several member names removes
/// every named member that is present and invalidates their sessions, while
/// leaving unnamed members and the room intact.
#[tokio::test]
async fn test_kckm_removes_multiple_members() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);

    let room_token = create_room(&store, "multi-kick-room").await;
    let alice_session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_room_token(
        &app,
        &store,
        alice_session.clone(),
        room_token.clone(),
        "alice".into(),
    )
    .await
    .unwrap();
    let bob_session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_room_token(
        &app,
        &store,
        bob_session.clone(),
        room_token.clone(),
        "bob".into(),
    )
    .await
    .unwrap();
    let carol_session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_room_token(
        &app,
        &store,
        carol_session.clone(),
        room_token.clone(),
        "carol".into(),
    )
    .await
    .unwrap();

    spawn_mgmt(&socket_path, &store, &app).await;

    // Kick alice and bob in one command; "ghost" is not a member and must be
    // tolerated (reported as not-found) without failing the command.
    let status = tokio::process::Command::new(cli_bin())
        .args([
            "--socket",
            socket_path.to_str().unwrap(),
            "kckm",
            "multi-kick-room",
            "alice",
            "bob",
            "ghost",
        ])
        .status()
        .await
        .expect("failed to spawn toaster-talk-cli — run `cargo build --workspace` first");

    assert!(
        status.success(),
        "CLI kckm must exit 0 when at least one member is kicked"
    );

    let app_read = app.read().await;
    let room_state = app_read
        .rooms
        .get(room_token.as_str())
        .expect("room must still exist after multi-member kckm");

    assert!(
        !room_state.members.contains_key("alice"),
        "alice must be removed after multi-member kckm"
    );
    assert!(
        !room_state.members.contains_key("bob"),
        "bob must be removed after multi-member kckm"
    );
    assert!(
        room_state.members.contains_key("carol"),
        "carol (unnamed) must remain after multi-member kckm"
    );

    assert!(
        !app_read.sessions.contains_key(&alice_session),
        "alice's session must be invalidated"
    );
    assert!(
        !app_read.sessions.contains_key(&bob_session),
        "bob's session must be invalidated"
    );
    assert!(
        app_read.sessions.contains_key(&carol_session),
        "carol's session must remain valid"
    );
}

// ── TDD Rubric 10.9 ───────────────────────────────────────────────────────────

/// Rubric 10.9: kckall removes all members and invalidates their sessions,
/// but the room itself remains.
#[tokio::test]
async fn test_kckall_removes_all_members() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);

    let room_token = create_room(&store, "kick-all-room").await;
    let alice_session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_room_token(
        &app,
        &store,
        alice_session.clone(),
        room_token.clone(),
        "alice".into(),
    )
    .await
    .unwrap();
    let bob_session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_room_token(
        &app,
        &store,
        bob_session.clone(),
        room_token.clone(),
        "bob".into(),
    )
    .await
    .unwrap();

    spawn_mgmt(&socket_path, &store, &app).await;

    let status = tokio::process::Command::new(cli_bin())
        .args([
            "--socket",
            socket_path.to_str().unwrap(),
            "kckall",
            "kick-all-room",
        ])
        .status()
        .await
        .expect("failed to spawn toaster-talk-cli — run `cargo build --workspace` first");

    assert!(status.success(), "CLI kckall must exit 0");

    let app_read = app.read().await;
    let room_state = app_read
        .rooms
        .get(room_token.as_str())
        .expect("room must still exist after kckall");

    assert!(
        room_state.members.is_empty(),
        "all members must be removed after kckall; remaining: {:?}",
        room_state.members.keys().collect::<Vec<_>>()
    );
    // M-R5-T02: every kicked member's session must be invalidated.
    assert!(
        !app_read.sessions.contains_key(&alice_session),
        "alice's session must be invalidated after kckall"
    );
    assert!(
        !app_read.sessions.contains_key(&bob_session),
        "bob's session must be invalidated after kckall"
    );
    // Room token must still be valid in the store.
    assert!(
        store.read().await.verify_token(&room_token).is_some(),
        "room token must remain valid after kckall"
    );
}

// ── Batch management commands (TDD 10.23–10.25) ───────────────────────────────

async fn run_cli(socket: &Path, token_dir: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(cli_bin())
        .args(["--socket", socket.to_str().unwrap()])
        .args(["--token-dir", token_dir.to_str().unwrap()])
        .args(args)
        .output()
        .await
        .expect("failed to spawn toaster-talk-cli — run `cargo build --workspace` first")
}

/// Rubric 10.23/10.24: addrm / addmb take several names in one command, create
/// each, and write one token file per item; repeating the identical command does
/// not write any additional (`_1`) file.
#[tokio::test]
async fn test_addrm_addmb_batch_create_and_reuse_identical_token_files() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);
    let token_dir = dir.path().join("tokens");
    spawn_mgmt(&socket_path, &store, &app).await;

    let out = run_cli(&socket_path, &token_dir, &["addrm", "r-one", "r-two"]).await;
    assert!(out.status.success(), "addrm batch must exit 0");
    assert!(store.read().await.get_by_name("r-one").is_some());
    assert!(store.read().await.get_by_name("r-two").is_some());
    assert!(token_dir.join("r-one.tok").exists());
    assert!(token_dir.join("r-two.tok").exists());

    let out = run_cli(
        &socket_path,
        &token_dir,
        &["addmb", "r-one", "alice", "bob"],
    )
    .await;
    assert!(out.status.success(), "addmb batch must exit 0");
    let seats = token_dir.join("user").join("r-one");
    assert!(seats.join("alice.tok").exists());
    assert!(seats.join("bob.tok").exists());

    // Same seats again: the server regenerates identical deterministic tokens and
    // the CLI must recognise the identical files rather than add `_1` copies.
    let out = run_cli(
        &socket_path,
        &token_dir,
        &["addmb", "r-one", "alice", "bob"],
    )
    .await;
    assert!(out.status.success());
    assert!(
        !seats.join("alice_1.tok").exists() && !seats.join("bob_1.tok").exists(),
        "identical seat files must be reused, not duplicated"
    );

    // Re-adding existing rooms re-derives the same deterministic token, so the
    // identical room token files must be reused too.
    let out = run_cli(&socket_path, &token_dir, &["addrm", "r-one", "r-two"]).await;
    assert!(out.status.success());
    assert!(!token_dir.join("r-one_1.tok").exists());
    assert!(!token_dir.join("r-two_1.tok").exists());
}

/// Rubric 10.25: a partly failing batch applies the items that can succeed,
/// reports the rest, and exits non-zero.
#[tokio::test]
async fn test_delrm_delmb_kckall_batch_partial_failure() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);
    let token_dir = dir.path().join("tokens");
    create_room(&store, "b-one").await;
    create_room(&store, "b-two").await;
    spawn_mgmt(&socket_path, &store, &app).await;

    run_cli(
        &socket_path,
        &token_dir,
        &["addmb", "b-one", "alice", "bob"],
    )
    .await;

    let out = run_cli(
        &socket_path,
        &token_dir,
        &["delmb", "b-one", "alice", "ghost", "bob"],
    )
    .await;
    assert!(
        !out.status.success(),
        "a failed item must make delmb exit non-zero"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("ghost"));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("alice")
            && String::from_utf8_lossy(&out.stdout).contains("bob"),
        "the items that could be deleted must still be reported"
    );

    let out = run_cli(&socket_path, &token_dir, &["kckall", "b-one", "b-two"]).await;
    assert!(
        out.status.success(),
        "kckall over existing rooms must exit 0"
    );

    let out = run_cli(
        &socket_path,
        &token_dir,
        &["delrm", "b-one", "nope", "b-two"],
    )
    .await;
    assert!(
        !out.status.success(),
        "unknown room must make delrm exit non-zero"
    );
    assert!(store.read().await.get_by_name("b-one").is_none());
    assert!(store.read().await.get_by_name("b-two").is_none());
}

/// `--file` names one file and is refused when several items are given.
#[tokio::test]
async fn test_file_flag_rejected_for_multiple_names() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);
    let token_dir = dir.path().join("tokens");
    spawn_mgmt(&socket_path, &store, &app).await;

    let out = run_cli(
        &socket_path,
        &token_dir,
        &["addrm", "x", "y", "--file", "z"],
    )
    .await;
    assert!(!out.status.success());
    assert!(store.read().await.get_by_name("x").is_none());
}
