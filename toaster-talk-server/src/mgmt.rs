use crate::{
    config::{Config, MasterSecret},
    error::Error,
    rooms::{RoomStore, RoomToken},
    state::{DiagnosticSnapshot, SharedAppState},
};
use arc_swap::ArcSwap;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};
use toaster_talk_common::parse_quoted_line;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot, RwLock},
};
use tracing::{debug, error, info, warn};

/// Trigger channel for the mgmt `RELOAD` command (TDD 8.17): each element is the
/// completion `oneshot` the caller awaits before replying to the operator. Owned by
/// `main.rs::run_command_reloader`; the mgmt listener only ever sends on it.
pub type ReloadSender = mpsc::UnboundedSender<oneshot::Sender<()>>;

/// Configuration bundle passed to the management listener and its sub-handlers.
/// Mirrors the pattern used by `send.rs` (passing `&Config`) — adding a policy
/// knob requires one struct field instead of four signature edits.
#[derive(Clone)]
pub struct MgmtConfig {
    pub max_rooms: usize,
    pub max_room_ttl_days: u64,
    pub default_room_ttl_days: u64,
    /// Operator-configured idle TTL for non-seat members. Used by `DUMP` to show
    /// each room-token member's countdown to idle eviction.
    pub member_idle_ttl_secs: u64,
    /// Dead-seat reaper threshold (0 = disabled). Used by `DUMP` to show a seat's
    /// countdown to reaping, or `exempt` when disabled.
    pub seat_idle_ttl_secs: u64,
    pub master_secret: MasterSecret,
}

impl MgmtConfig {
    /// Snapshot the mgmt-relevant knobs from the live config. Taken fresh per
    /// connection so a `SIGHUP` reload retunes room limits and TTLs for subsequent
    /// management commands. `master_secret` is restart-only, so it never changes.
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_rooms: config.max_rooms,
            max_room_ttl_days: config.max_room_ttl_days,
            default_room_ttl_days: config.default_room_ttl_days,
            member_idle_ttl_secs: config.member_idle_ttl_secs,
            seat_idle_ttl_secs: config.seat_idle_ttl_secs,
            master_secret: config.master_secret.clone(),
        }
    }
}

/// Start the Unix socket management listener. Accepts one connection at a time;
/// each connection handles one request/response pair then closes.
///
/// Returns `Err(reason)` if socket setup fails (bind, chmod, stale-socket removal).
/// The server supervisor uses this to distinguish a setup failure from a task panic.
#[cfg(unix)]
pub async fn run_mgmt_listener(
    socket_path: PathBuf,
    store: Arc<RwLock<RoomStore>>,
    app: SharedAppState,
    config: Arc<ArcSwap<Config>>,
    reload_tx: ReloadSender,
) -> Result<(), String> {
    // Remove stale socket file from a previous run.
    // Verify it is actually a Unix socket before removal to mitigate a TOCTOU
    // symlink attack (SEC-6): a plain file or directory at the same path is
    // never touched.
    if socket_path.exists() {
        let is_socket = std::fs::symlink_metadata(&socket_path)
            .map(|m| std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()))
            .unwrap_or(false);
        if !is_socket {
            let msg = format!(
                "path exists but is not a Unix socket; refusing to remove — remove it manually ({})",
                socket_path.display()
            );
            error!("{msg}");
            return Err(msg);
        }
        if let Err(e) = std::fs::remove_file(&socket_path) {
            let msg = format!(
                "could not remove stale mgmt socket {}: {e}",
                socket_path.display()
            );
            error!("{msg}");
            return Err(msg);
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("could not bind mgmt socket {}: {e}", socket_path.display());
            error!("{msg}");
            return Err(msg);
        }
    };

    // Set socket permissions to 0660 so members of the owning group (operator)
    // can connect without elevated privileges. Failure is fatal — if the chmod
    // fails the socket may be world-accessible, granting any local user full
    // room management (SEC-7).
    if let Err(e) = std::fs::set_permissions(
        &socket_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o660),
    ) {
        let msg = format!(
            "could not set mgmt socket permissions to 0660 ({}): {e}",
            socket_path.display()
        );
        error!("{msg}");
        return Err(msg);
    }

    info!(path = %socket_path.display(), "mgmt socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // Snapshot the live config per connection so a reload (SIGHUP or the
                // RELOAD command) retunes room limits and TTLs for management commands too.
                let conn_config = MgmtConfig::from_config(&config.load());
                tokio::spawn(handle_connection(
                    stream,
                    store.clone(),
                    app.clone(),
                    conn_config,
                    reload_tx.clone(),
                ));
            }
            Err(e) => {
                error!(error = %e, "mgmt listener accept error");
            }
        }
    }
}

/// Start the Windows named-pipe management listener. Mirrors the Unix socket
/// listener above one instance at a time; each connection handles one
/// request/response pair then closes. `pipe_path` is interpreted as the pipe
/// name (e.g. `\\.\pipe\toaster-talk`), matching `Config::mgmt_socket`'s role
/// on Unix.
///
/// Returns `Err(reason)` if the initial pipe instance cannot be created. The
/// server supervisor uses this to distinguish a setup failure from a task panic.
#[cfg(windows)]
pub async fn run_mgmt_listener(
    pipe_path: PathBuf,
    store: Arc<RwLock<RoomStore>>,
    app: SharedAppState,
    config: Arc<ArcSwap<Config>>,
    reload_tx: ReloadSender,
) -> Result<(), String> {
    let pipe_name = pipe_path.to_string_lossy().into_owned();

    let mut server = match create_pipe_instance(&pipe_name, true) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("could not create mgmt named pipe {pipe_name}: {e}");
            error!("{msg}");
            return Err(msg);
        }
    };

    info!(pipe = %pipe_name, "mgmt named pipe listening");

    loop {
        match server.connect().await {
            Ok(()) => {
                let connected = server;
                // Queue up the next instance before handing this one off so a
                // second client is never turned away while this request is served.
                server = match create_pipe_instance(&pipe_name, false) {
                    Ok(s) => s,
                    Err(e) => {
                        let msg = format!("could not create next mgmt pipe instance: {e}");
                        error!("{msg}");
                        return Err(msg);
                    }
                };
                let conn_config = MgmtConfig::from_config(&config.load());
                tokio::spawn(handle_connection(
                    connected,
                    store.clone(),
                    app.clone(),
                    conn_config,
                    reload_tx.clone(),
                ));
            }
            Err(e) => {
                error!(error = %e, "mgmt pipe accept error");
            }
        }
    }
}

/// Create one named-pipe server instance restricted to the pipe's owner and the
/// built-in Administrators group — the Windows equivalent of the Unix socket's
/// 0660 operator-group gate (SEC-7). Remote clients are rejected outright; this
/// is a local-IPC-only channel on both platforms.
#[cfg(windows)]
fn create_pipe_instance(pipe_name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    use std::{ffi::c_void, mem, ptr};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SECURITY_ATTRIBUTES,
        },
    };

    // Protected DACL (the leading "P") blocks inheritance from the parent
    // process token; only the pipe's creator-owner and built-in Administrators
    // are granted access (generic all — read, write, and connect).
    const SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;BA)";
    let wide_sddl: Vec<u16> = SDDL.encode_utf16().chain(std::iter::once(0)).collect();

    let mut sd: *mut c_void = ptr::null_mut();
    // SAFETY: `wide_sddl` is a valid, NUL-terminated wide string for the
    // duration of this call; `sd` is an out-param the API populates on success.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            1, // SDDL_REVISION_1
            &mut sd,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };

    // SAFETY: `attrs` is a valid, fully-initialized SECURITY_ATTRIBUTES for the
    // duration of this call.
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(pipe_name, &mut attrs as *mut _ as *mut c_void)
    };

    // SAFETY: `sd` was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW
    // above (LocalAlloc-backed) and is freed exactly once here regardless of outcome.
    unsafe {
        LocalFree(sd);
    }

    result
}

async fn handle_connection<S>(
    stream: S,
    store: Arc<RwLock<RoomStore>>,
    app: SharedAppState,
    config: MgmtConfig,
    reload_tx: ReloadSender,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);

    // SEC-R5-2: bound the request read. A management command is a single short
    // line; cap the read at 64 KiB via `take` so a member of the operator group
    // cannot exhaust memory with an unbounded line. `read_line` stops at the
    // first newline so we do NOT wait for EOF — the CLI client keeps its write
    // half open while awaiting the response, so an EOF-terminated read would
    // deadlock. At the cap, `take` yields EOF and `read_line` returns what it has.
    const MAX_REQUEST_BYTES: u64 = 65_536;
    let mut raw = String::new();
    if let Err(e) = BufReader::new(reader.take(MAX_REQUEST_BYTES))
        .read_line(&mut raw)
        .await
    {
        error!(error = %e, "mgmt: error reading request");
        return;
    }
    let line = raw.trim_end_matches(['\r', '\n']).to_string();
    if line.is_empty() {
        return;
    }

    debug!(request = %line, "mgmt request received");

    let response = dispatch(&line, &store, &app, &config, &reload_tx).await;

    let response_line = format!("{response}\r\n");
    if let Err(e) = writer.write_all(response_line.as_bytes()).await {
        error!(error = %e, "mgmt: error writing response");
    }
}

async fn dispatch(
    line: &str,
    store: &Arc<RwLock<RoomStore>>,
    app: &SharedAppState,
    config: &MgmtConfig,
    reload_tx: &ReloadSender,
) -> String {
    let parts = parse_quoted_line(line);
    if parts.is_empty() {
        return err_response("empty command");
    }

    match parts[0].to_ascii_uppercase().as_str() {
        "ADDRM" => cmd_addrm(&parts[1..], store, config).await,
        "DELRM" => cmd_delrm(&parts[1..], store, app).await,
        "LSRM" => cmd_lsrm(store, app).await,
        "GTRM" => cmd_gtrm(&parts[1..], store, app).await,
        "KCKM" => cmd_kckm(&parts[1..], store, app).await,
        "KCKALL" => cmd_kckall(&parts[1..], store, app).await,
        "ADDMB" => cmd_addmb(&parts[1..], store, config.master_secret.as_bytes()).await,
        "DELMB" => cmd_delmb(&parts[1..], store, app).await,
        "LSMB" => cmd_lsmb(&parts[1..], store).await,
        "RELOAD" => cmd_reload(reload_tx).await,
        "DUMP" => {
            cmd_dump(
                store,
                app,
                config.member_idle_ttl_secs,
                config.seat_idle_ttl_secs,
            )
            .await
        }
        unknown => err_response(&format!("unknown command: {unknown}")),
    }
}

// ── ADDRM ─────────────────────────────────────────────────────────────────────

async fn cmd_addrm(args: &[String], store: &Arc<RwLock<RoomStore>>, config: &MgmtConfig) -> String {
    if args.is_empty() {
        return err_response("ADDRM requires a room name");
    }

    let name = args[0].clone();

    // Parse optional keyword arg: TTL <days>
    let ttl_days = match parse_ttl_arg(args, 1) {
        Ok(Some(v)) => v,
        Ok(None) => config.default_room_ttl_days,
        Err(e) => return err_response(&e),
    };

    if ttl_days > config.max_room_ttl_days {
        return err_response(
            &Error::RoomTtlExceeded {
                given_days: ttl_days,
                max_days: config.max_room_ttl_days,
            }
            .to_string(),
        );
    }

    let result = {
        let mut store_write = store.write().await;
        store_write
            .create(
                name.clone(),
                ttl_days,
                config.max_rooms,
                config.master_secret.as_bytes(),
            )
            .await
    };

    match result {
        Ok(room) => {
            info!(room = %name, "room created via CLI");
            // Send the token back so the CLI can write the token file on its host.
            // Name is double-quoted so space-containing names parse correctly (F-006).
            format!(
                "+ROOM_CREATED \"{}\" {} {}",
                room.name, room.token, room.expires_at_secs
            )
        }
        Err(e) => {
            warn!(room = %name, error = %e, "ADDRM failed");
            err_response(&e.to_string())
        }
    }
}

// ── DELRM ─────────────────────────────────────────────────────────────────────

async fn cmd_delrm(
    args: &[String],
    store: &Arc<RwLock<RoomStore>>,
    app: &SharedAppState,
) -> String {
    if args.is_empty() {
        return err_response("DELRM requires a room name");
    }
    let name = &args[0];

    // Find token before locking store for delete (to evict from AppState).
    // `.ok()` because a missing room is reported by store.delete() below, not here.
    let token = resolve_room_token(store, name).await.ok();

    let result = {
        let mut store_write = store.write().await;
        store_write.delete(name).await
    };

    match result {
        Err(e) => {
            warn!(room = %name, error = %e, "DELRM failed");
            err_response(&e.to_string())
        }
        Ok(room) => {
            let members_evicted = if let Some(tok) = token {
                let mut app_write = app.write().await;
                app_write.evict_room(&tok, &room.name, "room deleted").await
            } else {
                0
            };

            info!(room = %name, members_evicted, "room deleted via CLI");
            format!(
                "+ROOM_DELETED \"{}\" evicted:{}",
                room.name, members_evicted
            )
        }
    }
}

// ── LSRM ──────────────────────────────────────────────────────────────────────

async fn cmd_lsrm(store: &Arc<RwLock<RoomStore>>, app: &SharedAppState) -> String {
    let store_read = store.read().await;
    let app_read = app.read().await;
    let rooms = store_read.list();

    if rooms.is_empty() {
        return "*0".to_string();
    }

    // Count line + one data line per room (no room tokens in output)
    let lines: Vec<String> = rooms
        .iter()
        .map(|r| {
            let members = app_read
                .rooms
                .get(&r.token)
                .map(|rs| rs.members.len())
                .unwrap_or(0);
            format!(
                "\"{}\" created:{} expires:{} members:{}",
                r.name, r.created_at_secs, r.expires_at_secs, members
            )
        })
        .collect();

    let mut out = format!("*{}\r\n", lines.len());
    for line in lines {
        out.push_str(&line);
        out.push_str("\r\n");
    }
    // Trim trailing \r\n — handle_connection appends its own
    out.trim_end_matches("\r\n").to_string()
}

// ── GTRM ──────────────────────────────────────────────────────────────────────

async fn cmd_gtrm(args: &[String], store: &Arc<RwLock<RoomStore>>, app: &SharedAppState) -> String {
    if args.is_empty() {
        return err_response("GTRM requires a room name");
    }
    let name = &args[0];

    let store_read = store.read().await;
    let app_read = app.read().await;

    let room = match store_read.get_by_name(name) {
        Some(r) => r,
        None => return err_response(&format!("room not found: {name}")),
    };

    let mut members: Vec<String> = app_read
        .rooms
        .get(&room.token)
        .map(|rs| rs.members.keys().cloned().collect())
        .unwrap_or_default();
    members.sort();

    let mut out = format!(
        "+ROOM \"{}\" created:{} expires:{} members:{}\r\n",
        room.name,
        room.created_at_secs,
        room.expires_at_secs,
        members.len()
    );
    for member in &members {
        out.push_str(member);
        out.push_str("\r\n");
    }
    // Trim trailing \r\n — handle_connection appends its own
    out.trim_end_matches("\r\n").to_string()
}

// ── KCKM ──────────────────────────────────────────────────────────────────────

async fn cmd_kckm(args: &[String], store: &Arc<RwLock<RoomStore>>, app: &SharedAppState) -> String {
    if args.len() < 2 {
        return err_response("KCKM requires a room name and at least one member name");
    }
    let room_name = &args[0];
    let member_names = &args[1..];

    let token = match resolve_room_token(store, room_name).await {
        Ok(t) => t,
        Err(e) => return e,
    };

    let mut app_write = app.write().await;
    let mut kicked: Vec<String> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    for member_name in member_names {
        // Ignore a name repeated on the same command line — it has already been
        // accounted for by its first occurrence, so don't double-report it.
        if kicked.contains(member_name) || not_found.contains(member_name) {
            continue;
        }
        if app_write
            .evict_member(&token, room_name, member_name, "kicked by operator")
            .await
        {
            kicked.push(member_name.clone());
        } else {
            not_found.push(member_name.clone());
        }
    }

    if kicked.is_empty() {
        return err_response(&format!("member(s) not found: {}", not_found.join(", ")));
    }

    info!(room = %room_name, kicked = ?kicked, not_found = ?not_found, "member(s) kicked via CLI");

    let mut resp = format!("+KICKED \"{room_name}\"");
    for member in &kicked {
        resp.push_str(&format!(" kicked:{member}"));
    }
    for member in &not_found {
        resp.push_str(&format!(" notfound:{member}"));
    }
    resp
}

// ── KCKALL ────────────────────────────────────────────────────────────────────

async fn cmd_kckall(
    args: &[String],
    store: &Arc<RwLock<RoomStore>>,
    app: &SharedAppState,
) -> String {
    if args.is_empty() {
        return err_response("KCKALL requires a room name");
    }
    let room_name = &args[0];

    let token = match resolve_room_token(store, room_name).await {
        Ok(t) => t,
        Err(e) => return e,
    };

    let mut app_write = app.write().await;
    let count = app_write
        .evict_all_members(&token, room_name, "all members kicked by operator")
        .await;
    info!(room = %room_name, members_kicked = count, "all members kicked via CLI");
    format!("+KICKED_ALL \"{}\" evicted:{}", room_name, count)
}

// ── ADDMB ─────────────────────────────────────────────────────────────────────

async fn cmd_addmb(
    args: &[String],
    store: &Arc<RwLock<RoomStore>>,
    master_secret: &[u8],
) -> String {
    if args.len() < 2 {
        return err_response("ADDMB requires a room name and member name");
    }
    let room_name = args[0].clone();
    let member_name = args[1].clone();

    let ttl_days = match parse_ttl_arg(args, 2) {
        Ok(Some(v)) => v,
        Ok(None) => 0, // 0 = inherit the room's remaining lifetime
        Err(e) => return err_response(&e),
    };

    let result = {
        let mut store_write = store.write().await;
        store_write
            .create_seat(&room_name, &member_name, ttl_days, master_secret)
            .await
    };

    match result {
        Ok((seat_token, expires_at_secs)) => {
            info!(room = %room_name, member = %member_name, "seat token created via CLI");
            format!(
                "+SEAT_CREATED \"{}\" \"{}\" {} {}",
                room_name, member_name, seat_token, expires_at_secs
            )
        }
        Err(e) => {
            warn!(room = %room_name, member = %member_name, error = %e, "ADDMB failed");
            err_response(&e.to_string())
        }
    }
}

// ── DELMB ─────────────────────────────────────────────────────────────────────

async fn cmd_delmb(
    args: &[String],
    store: &Arc<RwLock<RoomStore>>,
    app: &SharedAppState,
) -> String {
    if args.len() < 2 {
        return err_response("DELMB requires a room name and member name");
    }
    let room_name = &args[0];
    let member_name = &args[1];

    let room_token = match resolve_room_token(store, room_name).await {
        Ok(t) => t,
        Err(e) => return e,
    };

    let result = {
        let mut store_write = store.write().await;
        store_write.delete_seat(room_name, member_name).await
    };

    match result {
        Err(e) => {
            warn!(room = %room_name, member = %member_name, error = %e, "DELMB failed");
            err_response(&e.to_string())
        }
        Ok(()) => {
            let mut app_write = app.write().await;
            app_write
                .evict_member_superseded(&room_token, room_name, member_name)
                .await;
            info!(room = %room_name, member = %member_name, "seat token deleted via CLI");
            format!("+SEAT_DELETED \"{}\" \"{}\"", room_name, member_name)
        }
    }
}

// ── LSMB ──────────────────────────────────────────────────────────────────────

async fn cmd_lsmb(args: &[String], store: &Arc<RwLock<RoomStore>>) -> String {
    if args.is_empty() {
        return err_response("LSMB requires a room name");
    }
    let room_name = &args[0];

    let store_read = store.read().await;
    let seats = match store_read.list_seats(room_name) {
        Some(s) => s,
        None => return err_response(&format!("room not found: {room_name}")),
    };

    if seats.is_empty() {
        return "*0".to_string();
    }

    let mut out = format!("*{}\r\n", seats.len());
    for seat in seats {
        out.push_str(&format!(
            "\"{}\" created:{} expires:{}\r\n",
            seat.member_name, seat.created_at_secs, seat.expires_at_secs
        ));
    }
    out.trim_end_matches("\r\n").to_string()
}

// ── RELOAD ────────────────────────────────────────────────────────────────────

/// Trigger a configuration + room-store reload through the same path `SIGHUP`
/// uses on Unix (TDD 8.17) — the only reload trigger on Windows, and an explicit
/// alternative to `SIGHUP` on Unix. Waits for the reloader task to confirm the
/// reload was actually applied before replying, so the operator's `reload`
/// command reflects the new state by the time it returns.
async fn cmd_reload(reload_tx: &ReloadSender) -> String {
    let (ack_tx, ack_rx) = oneshot::channel();
    if reload_tx.send(ack_tx).is_err() {
        return err_response("reload task is not running");
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), ack_rx).await {
        Ok(Ok(())) => "+RELOADED".to_string(),
        Ok(Err(_)) => err_response("reload task dropped before completing"),
        Err(_) => err_response("reload timed out"),
    }
}

// ── DUMP ──────────────────────────────────────────────────────────────────────

/// Take a read-only diagnostic snapshot of live in-memory state and return it as
/// a YAML body (TDD section 13). Read-only: holds only read guards, mutates
/// nothing. Room names are resolved from the store so room tokens never appear in
/// the output. Response shape: `+DUMP` header line followed by the YAML body.
async fn cmd_dump(
    store: &Arc<RwLock<RoomStore>>,
    app: &SharedAppState,
    member_idle_ttl_secs: u64,
    seat_idle_ttl_secs: u64,
) -> String {
    let now = Instant::now();

    // Map room tokens → names so the dump can label rooms without emitting tokens.
    let room_names: HashMap<RoomToken, String> = {
        let store_read = store.read().await;
        store_read
            .list()
            .iter()
            .map(|r| (r.token.clone(), r.name.clone()))
            .collect()
    };

    let snapshot = {
        let app_read = app.read().await;
        app_read
            .diagnostic_snapshot(now, &room_names, member_idle_ttl_secs, seat_idle_ttl_secs)
            .await
    };

    let yaml = render_dump_yaml(&snapshot);

    // `+DUMP` header, then the YAML body. handle_connection appends the final CRLF.
    let mut out = String::from("+DUMP\r\n");
    for line in yaml.lines() {
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.trim_end_matches("\r\n").to_string()
}

/// Quote and escape a string as a YAML double-quoted scalar. Room and member
/// names may contain YAML-significant characters, so every string value is quoted.
fn yaml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Render a `DiagnosticSnapshot` as well-formed YAML (TDD 13.8). Hand-rendered to
/// avoid pulling a serde derive into the snapshot types; the structure is small
/// and fixed.
///
/// TDD 13.11: the shared inbox is rendered once per session under `sessions:`,
/// not duplicated under every room row — `rooms:` shows only per-room-membership
/// metadata (last_seen, auth, idle countdown), cross-referencing back to the
/// owning session by its redacted prefix.
fn render_dump_yaml(snap: &DiagnosticSnapshot) -> String {
    let mut out = String::new();

    if snap.rooms.is_empty() {
        out.push_str("rooms: []\n");
    } else {
        out.push_str("rooms:\n");
        for room in &snap.rooms {
            out.push_str(&format!("  - name: {}\n", yaml_quote(&room.name)));
            out.push_str(&format!("    member_count: {}\n", room.member_count));
            if room.members.is_empty() {
                out.push_str("    members: []\n");
            } else {
                out.push_str("    members:\n");
                for m in &room.members {
                    out.push_str(&format!(
                        "      - member_name: {}\n",
                        yaml_quote(&m.member_name)
                    ));
                    out.push_str(&format!(
                        "        session_prefix: {}\n",
                        yaml_quote(&m.session_prefix)
                    ));
                    out.push_str(&format!("        session_in_map: {}\n", m.session_in_map));
                    out.push_str(&format!("        last_seen_secs: {}\n", m.last_seen_secs));
                    out.push_str(&format!(
                        "        registered_secs_ago: {}\n",
                        m.registered_secs_ago
                    ));
                    out.push_str(&format!(
                        "        auth_method: {}\n",
                        yaml_quote(m.auth_method)
                    ));
                    // Seat-authed members are exempt; room-token members show a
                    // countdown to idle eviction.
                    match m.idle_countdown_secs {
                        Some(secs) => {
                            out.push_str(&format!("        idle_eviction_secs: {secs}\n"))
                        }
                        None => out.push_str("        idle_eviction_secs: \"exempt\"\n"),
                    }
                }
            }
        }
    }

    if snap.sessions.is_empty() {
        out.push_str("sessions: []\n");
    } else {
        out.push_str("sessions:\n");
        for s in &snap.sessions {
            out.push_str(&format!(
                "  - session_prefix: {}\n",
                yaml_quote(&s.session_prefix)
            ));
            out.push_str(&format!(
                "    connected_secs_ago: {}\n",
                s.connected_secs_ago
            ));
            if s.memberships.is_empty() {
                out.push_str("    memberships: []\n");
            } else {
                out.push_str("    memberships:\n");
                for label in &s.memberships {
                    out.push_str(&format!("      - {}\n", yaml_quote(label)));
                }
            }
            let ib = &s.inbox;
            out.push_str("    inbox:\n");
            out.push_str(&format!("      queue_depth: {}\n", ib.queue_depth));
            out.push_str(&format!("      receive_active: {}\n", ib.receive_active));
            out.push_str(&format!("      active_gen: {}\n", ib.active_gen));
            out.push_str(&format!("      gen_counter: {}\n", ib.gen_counter));
            out.push_str(&format!(
                "      abort_handle_present: {}\n",
                ib.abort_handle_present
            ));
            out.push_str(&format!(
                "      arc_strong_count: {}\n",
                ib.arc_strong_count
            ));
            out.push_str(&format!("      reply_waiters: {}\n", ib.reply_waiters));
            if ib.messages.is_empty() {
                out.push_str("      messages: []\n");
            } else {
                out.push_str("      messages:\n");
                for msg in &ib.messages {
                    out.push_str(&format!(
                        "        - id_prefix: {}\n",
                        yaml_quote(&msg.id_prefix)
                    ));
                    out.push_str(&format!("          from: {}\n", yaml_quote(&msg.from)));
                    out.push_str(&format!("          room: {}\n", yaml_quote(&msg.room)));
                    out.push_str(&format!("          age_secs: {}\n", msg.age_secs));
                    out.push_str(&format!(
                        "          ttl_remaining_secs: {}\n",
                        msg.ttl_remaining_secs
                    ));
                }
            }
        }
    }

    out.push_str(&format!("session_map_size: {}\n", snap.session_map_size));

    if snap.superseded_session_prefixes.is_empty() {
        out.push_str("superseded_sessions: []\n");
    } else {
        out.push_str("superseded_sessions:\n");
        for prefix in &snap.superseded_session_prefixes {
            out.push_str(&format!("  - {}\n", yaml_quote(prefix)));
        }
    }

    out.push_str("invariants:\n");
    for inv in &snap.invariants {
        out.push_str(&format!("  - {}\n", yaml_quote(inv)));
    }

    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn err_response(msg: &str) -> String {
    format!("-{msg}")
}

/// Parse an optional `TTL <days>` keyword argument starting at `args[start]`.
/// Returns `Ok(None)` when no TTL keyword is present, `Ok(Some(days))` when one
/// is, and `Err(message)` for malformed input. Shared by ADDRM and ADDMB (M-R4-C04).
fn parse_ttl_arg(args: &[String], start: usize) -> Result<Option<u64>, String> {
    let mut ttl_days: Option<u64> = None;
    let mut i = start;
    while i < args.len() {
        match args[i].to_ascii_uppercase().as_str() {
            "TTL" => {
                i += 1;
                if i >= args.len() {
                    return Err("TTL requires a value".to_string());
                }
                match args[i].parse::<u64>() {
                    Ok(v) => ttl_days = Some(v),
                    Err(_) => return Err("TTL value must be a positive integer".to_string()),
                }
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
        i += 1;
    }
    Ok(ttl_days)
}

/// Resolve a room name to its token, or an `err_response` string if not found.
/// Deduplicates the resolve-room-by-name block shared by KCKM/KCKALL/DELMB/DELRM (M-R4-C05).
async fn resolve_room_token(
    store: &Arc<RwLock<RoomStore>>,
    room_name: &str,
) -> Result<RoomToken, String> {
    store
        .read()
        .await
        .get_by_name(room_name)
        .map(|r| r.token.clone())
        .ok_or_else(|| err_response(&format!("room not found: {room_name}")))
}
