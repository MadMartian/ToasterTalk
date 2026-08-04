//! Shared helpers for in-crate unit tests (`#[cfg(test)]` only).
//! Integration tests in `tests/` use the parallel helpers in `tests/common/mod.rs`.

use crate::config::{Config, MasterSecret};
use std::path::PathBuf;

/// Canonical test master secret used across all unit test modules.
/// Integration tests in `tests/` use `common::TEST_MASTER_SECRET`.
pub const TEST_MASTER_SECRET: &[u8] = b"test-secret-for-toaster-talk-unit-tests-ok!!";

/// A `Config` populated with safe defaults for unit tests. Single definition
/// (M-R4-C09) shared by the `send` and any other in-crate test module.
pub fn test_config() -> Config {
    Config {
        bind: "127.0.0.1".into(),
        port: 0,
        log_level: "error".into(),
        store_path: PathBuf::from("/tmp/rooms.yaml"),
        max_rooms: 50,
        default_room_ttl_days: 30,
        max_room_ttl_days: 365,
        default_message_ttl_secs: 300,
        max_message_ttl_secs: 3600,
        sweep_interval_secs: 60,
        idle_threshold_secs: 300,
        member_idle_ttl_secs: 1800,
        seat_idle_ttl_secs: 0, // reaper disabled in unit tests unless a test opts in
        default_receive_timeout_secs: 30,
        max_await_reply_secs: 3600,
        max_receive_timeout_secs: 3600,
        max_inbox_messages: 100,
        mgmt_socket: PathBuf::from("/tmp/mgmt.sock"),
        master_secret: MasterSecret::new(TEST_MASTER_SECRET),
    }
}
