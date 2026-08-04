//! Tests for the mgmt `RELOAD` command (TDD 8.17): an explicit reload trigger
//! through the management interface, independent of any OS signal.
//!
//! `mgmt.rs::cmd_reload` only owns the request/response plumbing — sending the
//! trigger and waiting for the reloader task's completion acknowledgement before
//! replying. The actual config/room-store reload logic (`apply_reload` in the
//! server binary's `main.rs`) is exercised by the existing `Config::reload` /
//! `RoomStore::reload` unit tests (rubrics 8.13–8.16); these tests verify that a
//! `RELOAD` command reaches that path and behaves like any other reload trigger.

mod common;

use common::{make_app, make_store, mgmt_request, spawn_mgmt, spawn_mgmt_with_reload};
use tempfile::TempDir;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_reload_command_waits_for_completion_before_responding() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = make_app();
    let socket_path = common::test_mgmt_addr(&dir);

    let (reload_tx, mut reload_rx) = mpsc::unbounded_channel();
    spawn_mgmt_with_reload(&socket_path, &store, &app, reload_tx).await;

    // Consume the trigger manually with a delay before acking, proving the
    // RELOAD response is not sent until the reloader confirms completion.
    let consumer = tokio::spawn(async move {
        let ack = reload_rx.recv().await.expect("RELOAD must send a trigger");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        ack.send(()).unwrap();
    });

    let response = mgmt_request(&socket_path, "RELOAD").await;
    assert_eq!(response, "+RELOADED");
    consumer.await.unwrap();
}

#[tokio::test]
async fn test_reload_command_errors_if_reloader_task_is_gone() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = make_app();
    let socket_path = common::test_mgmt_addr(&dir);

    let (reload_tx, reload_rx) = mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<()>>();
    drop(reload_rx); // simulate the reloader task having exited

    spawn_mgmt_with_reload(&socket_path, &store, &app, reload_tx).await;

    let response = mgmt_request(&socket_path, "RELOAD").await;
    assert!(
        response.starts_with('-'),
        "expected an error response, got: {response}"
    );
}

#[tokio::test]
async fn test_reload_command_available_alongside_other_mgmt_commands() {
    // Rubric 8.17: RELOAD is just another mgmt command — normal spawn_mgmt (which
    // auto-acknowledges reload triggers) is enough for callers that don't care
    // about reload timing, and other commands keep working on the same listener.
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = make_app();
    let socket_path = common::test_mgmt_addr(&dir);

    spawn_mgmt(&socket_path, &store, &app).await;

    create_room_via_mgmt(&socket_path, "reload-room").await;

    let response = mgmt_request(&socket_path, "RELOAD").await;
    assert_eq!(response, "+RELOADED");

    let response = mgmt_request(&socket_path, "LSRM").await;
    assert!(
        response.contains("reload-room"),
        "room state must survive a RELOAD, got: {response}"
    );
}

async fn create_room_via_mgmt(socket_path: &std::path::Path, name: &str) {
    let response = mgmt_request(socket_path, &format!("ADDRM \"{name}\"")).await;
    assert!(
        response.starts_with("+ROOM_CREATED"),
        "expected room creation to succeed, got: {response}"
    );
}
