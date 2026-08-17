use crate::error::Error;
use clap::Parser;
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, trace, warn};

// ── MasterSecret ─────────────────────────────────────────────────────────────

/// Wrapper for the HMAC master secret. Redacts `Debug` to prevent the secret
/// from leaking in log lines, panic messages, or `dbg!()` calls.
/// Backed by `Arc<[u8]>` so cloning is a pointer increment, not a heap copy.
#[derive(Clone)]
pub struct MasterSecret(Arc<[u8]>);

impl MasterSecret {
    pub fn new(bytes: &[u8]) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for MasterSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterSecret([REDACTED])")
    }
}

// ── Defaults ────────────────────────────────────────────────────────────────

const DEFAULT_BIND: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 6669;
const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_MAX_ROOMS: usize = 50;
const DEFAULT_ROOM_TTL_DAYS: u64 = 30;
const DEFAULT_MAX_ROOM_TTL_DAYS: u64 = 365;
const DEFAULT_MESSAGE_TTL_SECS: u64 = 660; // 11 minutes
const DEFAULT_MAX_MESSAGE_TTL_SECS: u64 = 3600;
const DEFAULT_SWEEP_INTERVAL_SECS: u64 = 60;
const DEFAULT_IDLE_THRESHOLD_SECS: u64 = 300; // 5 minutes
const DEFAULT_RECEIVE_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_AWAIT_REPLY_SECS: u64 = 3600;
const DEFAULT_MAX_RECEIVE_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_MAX_INBOX_MESSAGES: usize = 100;
/// Default dead-seat reaper threshold: 24 h. A seat idle this long **with no active
/// receive** is reaped so its inbox stops black-holing messages (ANTI-PATTERNS #10).
/// Generous by design — seats are the privileged, persistent tier — but bounded so a
/// crashed seat can't linger forever. `0` disables the reaper (seats fully exempt).
const DEFAULT_SEAT_IDLE_TTL_SECS: u64 = 86_400;

/// Bounds on `member_idle_ttl_secs` (the non-seat member idle TTL). There is **no**
/// default: the operator must choose a value inside this range or the server
/// refuses to start. 30 min floor stops abusive churn; 24 h ceiling keeps a
/// crashed agent from lingering as a phantom member for days.
const MIN_MEMBER_IDLE_TTL_SECS: u64 = 1_800; // 30 minutes
const MAX_MEMBER_IDLE_TTL_SECS: u64 = 86_400; // 24 hours

// ── Resolved config (what the server uses) ───────────────────────────────────

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub log_level: String,
    pub store_path: PathBuf,
    pub max_rooms: usize,
    pub default_room_ttl_days: u64,
    pub max_room_ttl_days: u64,
    pub default_message_ttl_secs: u64,
    pub max_message_ttl_secs: u64,
    pub sweep_interval_secs: u64,
    /// Idle threshold: if a member has not been seen for this many seconds,
    /// their last_seen is included in relevant tool responses.
    pub idle_threshold_secs: u64,
    /// Idle TTL for non-seat (room-token-authed) members: a member not seen for
    /// this many seconds is evicted by the sweeper. Operator-only, mandatory, and
    /// bounded to `1800..=86400`; seat-authed members are exempt. No default.
    pub member_idle_ttl_secs: u64,
    /// Idle TTL for seat-authed members. `None` (the default) means seats are
    /// exempt from idle eviction entirely — the historical behavior. When set, a
    /// seat member not seen for this many seconds **and** not currently parked in
    /// a blocking receive is reaped, closing its inbox so senders fail-closed
    /// (`RecipientEvicted`) instead of black-holing messages into a queue no live
    /// client will ever drain. Defaults to `DEFAULT_SEAT_IDLE_TTL_SECS` (24 h);
    /// **`0` disables the reaper** (seats fully exempt, the pre-reaper behavior).
    /// Independent of `member_idle_ttl_secs`: this is a *dead-seat* detection
    /// threshold, not a privilege level — a legitimately idle seat agent parks in a
    /// blocking receive, which the sweeper always skips, so only a seat with no
    /// active receive is ever eligible. Operator-only; agents have no wire path to
    /// it. See ANTI-PATTERNS.md #10.
    pub seat_idle_ttl_secs: u64,
    pub default_receive_timeout_secs: u64,
    /// Hard ceiling on `await_reply_secs` in `send` — applied server-side regardless of transport.
    pub max_await_reply_secs: u64,
    /// Hard ceiling on `timeout_secs` in `receive` — applied server-side regardless of transport.
    pub max_receive_timeout_secs: u64,
    /// Maximum number of messages a single member's inbox may hold at once.
    /// Sends that would exceed this limit are rejected with InboxFull.
    pub max_inbox_messages: usize,
    pub mgmt_socket: PathBuf,
    /// Master secret used for HMAC-SHA256 token derivation. Loaded from file at startup;
    /// never logged, never written to rooms.yaml.
    pub master_secret: MasterSecret,
}

// ── File config (TOML, all fields optional) ──────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    bind: Option<String>,
    port: Option<u16>,
    log_level: Option<String>,
    rooms: Option<FileRoomsConfig>,
    messages: Option<FileMessagesConfig>,
    mgmt_socket: Option<String>,
    /// Path to master secret key file; relative to this config file's directory.
    master_secret_file: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileRoomsConfig {
    store_path: Option<String>,
    max_rooms: Option<usize>,
    default_ttl_days: Option<u64>,
    max_ttl_days: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct FileMessagesConfig {
    default_ttl_secs: Option<u64>,
    max_ttl_secs: Option<u64>,
    sweep_interval_secs: Option<u64>,
    idle_threshold_secs: Option<u64>,
    member_idle_ttl_secs: Option<u64>,
    seat_idle_ttl_secs: Option<u64>,
    default_receive_timeout_secs: Option<u64>,
    max_await_reply_secs: Option<u64>,
    max_receive_timeout_secs: Option<u64>,
    max_inbox_messages: Option<usize>,
}

// ── CLI args ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Parser)]
#[command(name = "toaster-talk-server", about = "ToasterTalk MCP Server")]
pub struct CliArgs {
    /// Path to config file
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Address to bind the MCP HTTP server to (e.g. 0.0.0.0 or 127.0.0.1)
    #[arg(long)]
    pub bind: Option<String>,

    /// Port to listen on
    #[arg(long)]
    pub port: Option<u16>,

    /// Log level: trace | debug | info | warn | error
    #[arg(long)]
    pub log_level: Option<String>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Resolve the operator's home directory (`$HOME` on Unix/macOS, `%USERPROFILE%`
/// on Windows) via `std::env::home_dir` — falling back to the platform temp dir
/// on the rare host where neither resolves.
fn home_dir() -> PathBuf {
    std::env::home_dir().unwrap_or_else(std::env::temp_dir)
}

/// `%ProgramData%` on Windows (falling back to the conventional `C:\ProgramData`
/// if the variable is unset) — the Windows analogue of `/etc` and `/var/lib` for
/// machine-wide, non-user-specific application data.
#[cfg(windows)]
fn program_data_dir() -> PathBuf {
    std::env::var("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData"))
}

/// Default config file path: `/etc/toaster-talk/config.toml` on Unix,
/// `%ProgramData%\ToasterTalk\config.toml` on Windows.
#[cfg(unix)]
fn default_config_path() -> PathBuf {
    PathBuf::from("/etc/toaster-talk/config.toml")
}
#[cfg(windows)]
fn default_config_path() -> PathBuf {
    program_data_dir().join("ToasterTalk").join("config.toml")
}

/// Default room store path: `/var/lib/toaster-talk/rooms.yaml` on Unix,
/// `%ProgramData%\ToasterTalk\rooms.yaml` on Windows.
#[cfg(unix)]
fn default_store_path() -> PathBuf {
    PathBuf::from("/var/lib/toaster-talk/rooms.yaml")
}
#[cfg(windows)]
fn default_store_path() -> PathBuf {
    program_data_dir().join("ToasterTalk").join("rooms.yaml")
}

/// Default management transport address: a Unix domain socket path on Unix,
/// a named-pipe name on Windows (`mgmt.rs` interprets this value differently per
/// platform — see `Config::mgmt_socket`). macOS uses a persistent `/usr/local`
/// parent because launchd has no `RuntimeDirectory=` equivalent and clears `/run`
/// at boot.
#[cfg(target_os = "macos")]
fn default_mgmt_socket() -> PathBuf {
    PathBuf::from("/usr/local/var/run/toaster-talk/mgmt.sock")
}
#[cfg(all(unix, not(target_os = "macos")))]
fn default_mgmt_socket() -> PathBuf {
    PathBuf::from("/run/toaster-talk/mgmt.sock")
}
#[cfg(windows)]
fn default_mgmt_socket() -> PathBuf {
    PathBuf::from(r"\\.\pipe\toaster-talk")
}

fn expand(s: &str) -> PathBuf {
    if s == "~" {
        home_dir()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(s)
    }
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Result<Option<T>, Error> {
    match std::env::var(key) {
        Err(_) => Ok(None),
        Ok(v) => v.parse::<T>().map(Some).map_err(|_| {
            Error::Config(format!(
                "environment variable {key} has an invalid value: expected {}",
                std::any::type_name::<T>()
            ))
        }),
    }
}

/// Apply a 3-layer configuration merge: default → file → env → CLI (last wins).
fn layer<T>(default: T, file: Option<T>, env: Option<T>, cli: Option<T>) -> T {
    cli.or(env).or(file).unwrap_or(default)
}

fn load_master_secret_file(path: &std::path::Path) -> Result<MasterSecret, Error> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(format!(
            "could not read master_secret_file '{}': {e}",
            path.display()
        ))
    })?;
    validate_master_secret_bytes(raw.trim())
}

fn validate_master_secret_bytes(stripped: &str) -> Result<MasterSecret, Error> {
    let len = stripped.chars().count();
    if !(40..=120).contains(&len) {
        return Err(Error::Config(format!(
            "master secret must be 40–120 characters after whitespace stripping (got {len})"
        )));
    }
    Ok(MasterSecret::new(stripped.as_bytes()))
}

// ── Config load (file → env → CLI, last wins) ────────────────────────────────

impl Config {
    pub fn load(args: &CliArgs) -> Result<Self, Error> {
        // Layer 1: load file config
        let config_path = args.config.clone().unwrap_or_else(default_config_path);

        let file: FileConfig = if config_path.exists() {
            trace!(path = %config_path.display(), "loading config file");
            let raw = std::fs::read_to_string(&config_path)?;
            toml::from_str(&raw)?
        } else {
            trace!(path = %config_path.display(), "config file not found, using defaults");
            FileConfig::default()
        };

        let fr = file.rooms.unwrap_or_default();
        let fm = file.messages.unwrap_or_default();

        // Layer 2 + 3: apply env then CLI (each later layer wins).
        // `layer(default, file, env, cli)` — rightmost Some wins.
        let bind = layer(
            DEFAULT_BIND.to_string(),
            file.bind,
            env_str("TOASTER_BIND"),
            args.bind.clone(),
        );
        let port = layer(
            DEFAULT_PORT,
            file.port,
            env_parse::<u16>("TOASTER_PORT")?,
            args.port,
        );
        let log_level = layer(
            DEFAULT_LOG_LEVEL.to_string(),
            file.log_level,
            env_str("TOASTER_LOG_LEVEL"),
            args.log_level.clone(),
        );
        let store_path = layer(
            default_store_path(),
            fr.store_path.as_deref().map(expand),
            env_str("TOASTER_STORE_PATH").as_deref().map(expand),
            None,
        );
        let max_rooms = layer(
            DEFAULT_MAX_ROOMS,
            fr.max_rooms,
            env_parse::<usize>("TOASTER_MAX_ROOMS")?,
            None,
        );
        let default_room_ttl_days = layer(
            DEFAULT_ROOM_TTL_DAYS,
            fr.default_ttl_days,
            env_parse::<u64>("TOASTER_DEFAULT_ROOM_TTL_DAYS")?,
            None,
        );
        let max_room_ttl_days = layer(
            DEFAULT_MAX_ROOM_TTL_DAYS,
            fr.max_ttl_days,
            env_parse::<u64>("TOASTER_MAX_ROOM_TTL_DAYS")?,
            None,
        );
        let default_message_ttl_secs = layer(
            DEFAULT_MESSAGE_TTL_SECS,
            fm.default_ttl_secs,
            env_parse::<u64>("TOASTER_DEFAULT_MESSAGE_TTL_SECS")?,
            None,
        );
        let max_message_ttl_secs = layer(
            DEFAULT_MAX_MESSAGE_TTL_SECS,
            fm.max_ttl_secs,
            env_parse::<u64>("TOASTER_MAX_MESSAGE_TTL_SECS")?,
            None,
        );
        let sweep_interval_secs = layer(
            DEFAULT_SWEEP_INTERVAL_SECS,
            fm.sweep_interval_secs,
            env_parse::<u64>("TOASTER_SWEEP_INTERVAL_SECS")?,
            None,
        );
        let idle_threshold_secs = layer(
            DEFAULT_IDLE_THRESHOLD_SECS,
            fm.idle_threshold_secs,
            env_parse::<u64>("TOASTER_IDLE_THRESHOLD_SECS")?,
            None,
        );
        let default_receive_timeout_secs = layer(
            DEFAULT_RECEIVE_TIMEOUT_SECS,
            fm.default_receive_timeout_secs,
            env_parse::<u64>("TOASTER_DEFAULT_RECEIVE_TIMEOUT_SECS")?,
            None,
        );
        let max_await_reply_secs = layer(
            DEFAULT_MAX_AWAIT_REPLY_SECS,
            fm.max_await_reply_secs,
            env_parse::<u64>("TOASTER_MAX_AWAIT_REPLY_SECS")?,
            None,
        );
        let max_receive_timeout_secs = layer(
            DEFAULT_MAX_RECEIVE_TIMEOUT_SECS,
            fm.max_receive_timeout_secs,
            env_parse::<u64>("TOASTER_MAX_RECEIVE_TIMEOUT_SECS")?,
            None,
        );
        let max_inbox_messages = layer(
            DEFAULT_MAX_INBOX_MESSAGES,
            fm.max_inbox_messages,
            env_parse::<usize>("TOASTER_MAX_INBOX_MESSAGES")?,
            None,
        );
        let mgmt_socket = layer(
            default_mgmt_socket(),
            file.mgmt_socket.as_deref().map(expand),
            env_str("TOASTER_MGMT_SOCKET").as_deref().map(expand),
            None,
        );

        // Master secret resolution order:
        //   TOASTER_MASTER_SECRET      (inline, TEST BUILDS ONLY — see below) →
        //   TOASTER_MASTER_SECRET_FILE (env file path) →
        //   master_secret_file in config.toml (relative to config dir) →
        //   fatal error
        //
        // NV-R5-1: the inline env var is gated behind `#[cfg(test)]`. Passing a
        // secret as a plain environment variable exposes it via /proc/<pid>/environ,
        // so production builds never read it — operators must use a 0600 secret file.
        #[cfg(test)]
        let inline_secret = std::env::var("TOASTER_MASTER_SECRET").ok();
        #[cfg(not(test))]
        let inline_secret: Option<String> = None;

        let master_secret = if let Some(inline) = inline_secret {
            validate_master_secret_bytes(inline.trim())?
        } else if let Some(env_path) = env_str("TOASTER_MASTER_SECRET_FILE") {
            load_master_secret_file(std::path::Path::new(&env_path))?
        } else if let Some(ref rel_path) = file.master_secret_file {
            let abs = config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(rel_path);
            load_master_secret_file(&abs)?
        } else {
            return Err(Error::Config(
                "master_secret_file is required in config.toml (or set TOASTER_MASTER_SECRET / TOASTER_MASTER_SECRET_FILE)".into(),
            ));
        };

        // member_idle_ttl_secs is mandatory (like master_secret): there is no
        // default. The operator must consciously choose the non-seat member idle
        // TTL — agents have no wire path to it, so the server must not pick a value
        // on their behalf. Resolve [messages].member_idle_ttl_secs or the env var.
        let member_idle_ttl_secs = match env_parse::<u64>("TOASTER_MEMBER_IDLE_TTL_SECS")?
            .or(fm.member_idle_ttl_secs)
        {
            Some(v) => v,
            None => {
                return Err(Error::Config(
                    "member_idle_ttl_secs must be set explicitly in [messages] (no default — \
                     the operator must choose the non-seat member idle TTL, between 1800 and 86400 seconds)"
                        .into(),
                ))
            }
        };

        // Seat idle TTL — the dead-seat reaper threshold. Defaults to 24 h so the
        // reaper is active out of the box (fixes the black-holed-message defect);
        // `0` disables it (seats fully exempt). See ANTI-PATTERNS.md #10.
        let seat_idle_ttl_secs = layer(
            DEFAULT_SEAT_IDLE_TTL_SECS,
            fm.seat_idle_ttl_secs,
            env_parse::<u64>("TOASTER_SEAT_IDLE_TTL_SECS")?,
            None,
        );

        // Validate consistency
        if max_inbox_messages == 0 {
            return Err(Error::Config(
                "max_inbox_messages must be at least 1".into(),
            ));
        }
        if !(MIN_MEMBER_IDLE_TTL_SECS..=MAX_MEMBER_IDLE_TTL_SECS).contains(&member_idle_ttl_secs) {
            return Err(Error::Config(format!(
                "member_idle_ttl_secs ({member_idle_ttl_secs}) must be between \
                 {MIN_MEMBER_IDLE_TTL_SECS} (30 min) and {MAX_MEMBER_IDLE_TTL_SECS} (24h)"
            )));
        }
        // The soft "peer has gone quiet" reporting line must never sit above the
        // hard "reap the member" line — otherwise a member could be reaped before
        // the idle hint ever surfaces.
        if idle_threshold_secs > member_idle_ttl_secs {
            return Err(Error::Config(format!(
                "idle_threshold_secs ({idle_threshold_secs}) must not exceed \
                 member_idle_ttl_secs ({member_idle_ttl_secs})"
            )));
        }
        if default_room_ttl_days > max_room_ttl_days {
            return Err(Error::Config(format!(
                "default_room_ttl_days ({default_room_ttl_days}) exceeds max_room_ttl_days ({max_room_ttl_days})"
            )));
        }
        if default_message_ttl_secs > max_message_ttl_secs {
            return Err(Error::Config(format!(
                "default_message_ttl_secs ({default_message_ttl_secs}) exceeds max_message_ttl_secs ({max_message_ttl_secs})"
            )));
        }

        let cfg = Config {
            bind,
            port,
            log_level,
            store_path,
            max_rooms,
            default_room_ttl_days,
            max_room_ttl_days,
            default_message_ttl_secs,
            max_message_ttl_secs,
            sweep_interval_secs,
            idle_threshold_secs,
            member_idle_ttl_secs,
            seat_idle_ttl_secs,
            default_receive_timeout_secs,
            max_await_reply_secs,
            max_receive_timeout_secs,
            max_inbox_messages,
            mgmt_socket,
            master_secret,
        };

        debug!(
            bind = %cfg.bind,
            port = cfg.port,
            max_rooms = cfg.max_rooms,
            default_room_ttl_days = cfg.default_room_ttl_days,
            default_message_ttl_secs = cfg.default_message_ttl_secs,
            store_path = %cfg.store_path.display(),
            mgmt_socket = %cfg.mgmt_socket.display(),
            "config resolved"
        );

        Ok(cfg)
    }

    /// Re-resolve configuration from disk (file → env → CLI) for a live `SIGHUP`
    /// reload, preserving the fields that cannot change without a restart.
    ///
    /// Returns the validated replacement config. On any error the caller must keep
    /// the running config unchanged — a malformed edit must never take down a live
    /// server.
    ///
    /// Immutable-at-runtime fields — `bind`, `port`, `mgmt_socket`, `store_path`,
    /// and `master_secret` — are either bound to a live OS resource (the TCP
    /// listener, the Unix mgmt socket, the open room store) or underpin every token
    /// already issued (`master_secret` is the HMAC key; rotating it would invalidate
    /// every live session and seat token). They cannot be honoured in-process, so a
    /// changed value is logged at WARN and the `startup` value is retained.
    pub fn reload(startup: &Config, args: &CliArgs) -> Result<Config, Error> {
        let mut cfg = Config::load(args)?;

        if cfg.bind != startup.bind {
            warn!(old = %startup.bind, new = %cfg.bind,
                "config reload: `bind` change ignored — requires a full restart");
            cfg.bind = startup.bind.clone();
        }
        if cfg.port != startup.port {
            warn!(
                old = startup.port,
                new = cfg.port,
                "config reload: `port` change ignored — requires a full restart"
            );
            cfg.port = startup.port;
        }
        if cfg.mgmt_socket != startup.mgmt_socket {
            warn!(old = %startup.mgmt_socket.display(), new = %cfg.mgmt_socket.display(),
                "config reload: `mgmt_socket` change ignored — requires a full restart");
            cfg.mgmt_socket = startup.mgmt_socket.clone();
        }
        if cfg.store_path != startup.store_path {
            warn!(old = %startup.store_path.display(), new = %cfg.store_path.display(),
                "config reload: `store_path` change ignored — requires a full restart");
            cfg.store_path = startup.store_path.clone();
        }
        if cfg.master_secret.as_bytes() != startup.master_secret.as_bytes() {
            warn!(
                "config reload: `master_secret` change ignored — requires a full restart \
                   (rotating it in-process would invalidate every live session and seat token)"
            );
            cfg.master_secret = startup.master_secret.clone();
        }

        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests that mutate env vars — env is global process state.
    static ENV_LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));

    fn no_args() -> CliArgs {
        CliArgs {
            // Point at a guaranteed-nonexistent path so no real config file is loaded.
            config: Some(PathBuf::from("/tmp/toaster-test-no-config.toml")),
            bind: None,
            port: None,
            log_level: None,
        }
    }

    fn set_test_secret() {
        std::env::set_var(
            "TOASTER_MASTER_SECRET",
            std::str::from_utf8(crate::test_support::TEST_MASTER_SECRET).unwrap(),
        );
        // member_idle_ttl_secs is mandatory with no default — every success-path
        // test must supply it or Config::load aborts.
        std::env::set_var("TOASTER_MEMBER_IDLE_TTL_SECS", "1800");
    }

    fn clear_test_secret() {
        std::env::remove_var("TOASTER_MASTER_SECRET");
        std::env::remove_var("TOASTER_MEMBER_IDLE_TTL_SECS");
    }

    #[test]
    fn test_defaults_when_no_config_no_env() {
        // Rubric 8.1: server uses default configuration when none is provided.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("TOASTER_BIND");
        std::env::remove_var("TOASTER_PORT");
        std::env::remove_var("TOASTER_LOG_LEVEL");
        set_test_secret();

        let config = Config::load(&no_args()).unwrap();
        clear_test_secret();
        assert_eq!(config.bind, "127.0.0.1");
        assert_eq!(config.port, 6669);
        assert_eq!(config.max_rooms, 50);
        assert_eq!(config.default_message_ttl_secs, 660);
        // No default — set_test_secret supplies the mandatory value.
        assert_eq!(config.member_idle_ttl_secs, 1800);
    }

    #[test]
    fn test_default_mgmt_socket_is_platform_native() {
        #[cfg(target_os = "macos")]
        assert_eq!(
            default_mgmt_socket(),
            PathBuf::from("/usr/local/var/run/toaster-talk/mgmt.sock")
        );

        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(
            default_mgmt_socket(),
            PathBuf::from("/run/toaster-talk/mgmt.sock")
        );

        #[cfg(windows)]
        assert_eq!(
            default_mgmt_socket(),
            PathBuf::from(r"\\.\pipe\toaster-talk")
        );
    }

    #[test]
    fn test_env_overrides_default() {
        // Rubric 8.4: environment variable takes precedence over config file / defaults.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_test_secret();
        std::env::set_var("TOASTER_PORT", "12345");
        let config = Config::load(&no_args()).unwrap();
        std::env::remove_var("TOASTER_PORT");
        clear_test_secret();
        assert_eq!(config.port, 12345);
    }

    #[test]
    fn test_cli_overrides_env() {
        // Rubric 8.5: CLI flag takes precedence over environment variable.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_test_secret();
        std::env::set_var("TOASTER_BIND", "192.168.1.1");
        let args = CliArgs {
            config: Some(PathBuf::from("/tmp/toaster-test-no-config.toml")),
            bind: Some("127.0.0.1".into()),
            port: None,
            log_level: None,
        };
        let config = Config::load(&args).unwrap();
        std::env::remove_var("TOASTER_BIND");
        clear_test_secret();
        assert_eq!(config.bind, "127.0.0.1");
    }

    #[test]
    fn test_reload_applies_tunable_keeps_immutable() {
        // Rubric 8.13: a SIGHUP reload re-resolves the tunable keys (here the default
        // message TTL) but retains the restart-only fields (here `port`) from the
        // startup config, even when the on-disk value for a restart-only field changed.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_test_secret();
        std::env::remove_var("TOASTER_PORT");
        std::env::remove_var("TOASTER_DEFAULT_MESSAGE_TTL_SECS");

        // Startup baseline: default message TTL (660) and default port (6669).
        let startup = Config::load(&no_args()).unwrap();
        assert_eq!(startup.default_message_ttl_secs, 660);
        assert_eq!(startup.port, 6669);

        // Operator edit: a tunable key changes, and a restart-only key (port) is also
        // changed — the latter must be ignored on reload.
        std::env::set_var("TOASTER_DEFAULT_MESSAGE_TTL_SECS", "1200");
        std::env::set_var("TOASTER_PORT", "12345");
        let reloaded = Config::reload(&startup, &no_args()).unwrap();
        std::env::remove_var("TOASTER_DEFAULT_MESSAGE_TTL_SECS");
        std::env::remove_var("TOASTER_PORT");
        clear_test_secret();

        assert_eq!(
            reloaded.default_message_ttl_secs, 1200,
            "tunable key must take effect on reload"
        );
        assert_eq!(
            reloaded.port, startup.port,
            "restart-only `port` must be retained from startup, not the changed on-disk value"
        );
    }

    #[test]
    fn test_file_config_is_overridden_by_env() {
        // Rubric 8.4 (file→env layer): env var takes precedence over a value set in
        // the config file. This exercises the layer that the other tests skip
        // (they use a non-existent config path, so the file layer is never loaded).
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_test_secret();
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "port = 9999\n").unwrap();

        std::env::set_var("TOASTER_PORT", "12345");
        let args = CliArgs {
            config: Some(config_path),
            bind: None,
            port: None,
            log_level: None,
        };
        let config = Config::load(&args).unwrap();
        std::env::remove_var("TOASTER_PORT");
        clear_test_secret();

        // File set 9999, env set 12345 — env must win.
        assert_eq!(config.port, 12345);
    }

    #[test]
    fn test_invalid_env_var_returns_error() {
        // Rubric 8.3: malformed config value → descriptive error, server cannot start.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_test_secret();
        std::env::set_var("TOASTER_PORT", "not-a-number");
        let result = Config::load(&no_args());
        std::env::remove_var("TOASTER_PORT");
        clear_test_secret();
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_master_secret_is_fatal() {
        // Rubric 8.x: server refuses to start if no master secret is configured.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("TOASTER_MASTER_SECRET");
        std::env::remove_var("TOASTER_MASTER_SECRET_FILE");
        let result = Config::load(&no_args());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("master_secret_file"),
            "error should mention master_secret_file, got: {msg}"
        );
    }

    #[test]
    fn test_master_secret_too_short_is_fatal() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("TOASTER_MASTER_SECRET", "short");
        let result = Config::load(&no_args());
        clear_test_secret();
        assert!(result.is_err());
    }

    #[test]
    fn test_master_secret_file_roundtrip() {
        use crate::test_support::TEST_MASTER_SECRET;
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("TOASTER_MASTER_SECRET");
        std::env::remove_var("TOASTER_MASTER_SECRET_FILE");
        std::env::remove_var("TOASTER_MEMBER_IDLE_TTL_SECS");
        let dir = tempfile::TempDir::new().unwrap();
        let key_path = dir.path().join("secret.key");
        std::fs::write(
            &key_path,
            format!("{}\n", std::str::from_utf8(TEST_MASTER_SECRET).unwrap()),
        )
        .unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "master_secret_file = \"secret.key\"\n\
             [messages]\nmember_idle_ttl_secs = 1800\n",
        )
        .unwrap();
        let args = CliArgs {
            config: Some(config_path),
            bind: None,
            port: None,
            log_level: None,
        };
        let config = Config::load(&args).unwrap();
        assert_eq!(config.master_secret.as_bytes(), TEST_MASTER_SECRET);
    }

    // ── member_idle_ttl_secs (rubric 8.x) ─────────────────────────────────────

    /// Helper: load with the master secret present but a specific (or absent)
    /// member_idle_ttl_secs env value. `None` removes the var entirely.
    fn load_with_member_idle(value: Option<&str>) -> Result<Config, Error> {
        std::env::set_var(
            "TOASTER_MASTER_SECRET",
            std::str::from_utf8(crate::test_support::TEST_MASTER_SECRET).unwrap(),
        );
        match value {
            Some(v) => std::env::set_var("TOASTER_MEMBER_IDLE_TTL_SECS", v),
            None => std::env::remove_var("TOASTER_MEMBER_IDLE_TTL_SECS"),
        }
        let result = Config::load(&no_args());
        std::env::remove_var("TOASTER_MASTER_SECRET");
        std::env::remove_var("TOASTER_MEMBER_IDLE_TTL_SECS");
        result
    }

    #[test]
    fn test_member_idle_ttl_absent_is_fatal() {
        // Rubric 8.x: no default — the operator must choose the value.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let err = load_with_member_idle(None).unwrap_err().to_string();
        assert!(
            err.contains("member_idle_ttl_secs"),
            "error must name the missing key, got: {err}"
        );
    }

    #[test]
    fn test_member_idle_ttl_below_minimum_is_fatal() {
        // Rubric 8.x: < 1800s (30 min) is out of bounds.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(load_with_member_idle(Some("1799")).is_err());
        assert!(load_with_member_idle(Some("0")).is_err());
    }

    #[test]
    fn test_member_idle_ttl_above_maximum_is_fatal() {
        // Rubric 8.x: > 86400s (24h) is out of bounds.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(load_with_member_idle(Some("86401")).is_err());
    }

    #[test]
    fn test_member_idle_ttl_in_range_is_accepted() {
        // Rubric 8.x: a value in [1800, 86400] is accepted and resolved.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = load_with_member_idle(Some("1800")).unwrap();
        assert_eq!(cfg.member_idle_ttl_secs, 1800);
        let cfg = load_with_member_idle(Some("86400")).unwrap();
        assert_eq!(cfg.member_idle_ttl_secs, 86400);
    }

    // ── seat_idle_ttl_secs (optional; dead-seat reaper threshold) ─────────────

    /// Helper: load with the mandatory keys present plus a specific (or absent)
    /// seat_idle_ttl_secs env value. `None` removes the var.
    fn load_with_seat_idle(value: Option<&str>) -> Result<Config, Error> {
        std::env::set_var(
            "TOASTER_MASTER_SECRET",
            std::str::from_utf8(crate::test_support::TEST_MASTER_SECRET).unwrap(),
        );
        std::env::set_var("TOASTER_MEMBER_IDLE_TTL_SECS", "1800");
        match value {
            Some(v) => std::env::set_var("TOASTER_SEAT_IDLE_TTL_SECS", v),
            None => std::env::remove_var("TOASTER_SEAT_IDLE_TTL_SECS"),
        }
        let result = Config::load(&no_args());
        std::env::remove_var("TOASTER_MASTER_SECRET");
        std::env::remove_var("TOASTER_MEMBER_IDLE_TTL_SECS");
        std::env::remove_var("TOASTER_SEAT_IDLE_TTL_SECS");
        result
    }

    #[test]
    fn test_seat_idle_ttl_absent_defaults_to_24h() {
        // Unset → reaper active by default (24 h), fixing the black-holed-message defect.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = load_with_seat_idle(None).unwrap();
        assert_eq!(cfg.seat_idle_ttl_secs, 86_400);
    }

    #[test]
    fn test_seat_idle_ttl_zero_disables_reaper() {
        // 0 is the explicit opt-out: seats fully exempt (pre-reaper behavior). Valid.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = load_with_seat_idle(Some("0")).unwrap();
        assert_eq!(cfg.seat_idle_ttl_secs, 0);
    }

    #[test]
    fn test_seat_idle_ttl_accepted_and_decoupled_from_member_ttl() {
        // Any value is accepted, INCLUDING one below member_idle_ttl_secs (1800):
        // the seat TTL is a dead-seat threshold, not a privilege level.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = load_with_seat_idle(Some("15")).unwrap();
        assert_eq!(cfg.seat_idle_ttl_secs, 15);
        let cfg = load_with_seat_idle(Some("604800")).unwrap();
        assert_eq!(cfg.seat_idle_ttl_secs, 604_800);
    }

    #[test]
    fn test_idle_threshold_above_member_idle_ttl_is_fatal() {
        // Coherence check: the soft idle-hint line must not sit above the hard
        // reap line. idle_threshold default is 300; force it above the TTL via env.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(
            "TOASTER_MASTER_SECRET",
            std::str::from_utf8(crate::test_support::TEST_MASTER_SECRET).unwrap(),
        );
        std::env::set_var("TOASTER_MEMBER_IDLE_TTL_SECS", "1800");
        std::env::set_var("TOASTER_IDLE_THRESHOLD_SECS", "1801");
        let result = Config::load(&no_args());
        std::env::remove_var("TOASTER_MASTER_SECRET");
        std::env::remove_var("TOASTER_MEMBER_IDLE_TTL_SECS");
        std::env::remove_var("TOASTER_IDLE_THRESHOLD_SECS");
        assert!(result.is_err());
    }
}
