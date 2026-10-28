use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
/// ToasterTalk CLI — translates subcommands into wire requests sent to the
/// running toaster-talk-server over its management transport (a Unix domain
/// socket on Unix, a named pipe on Windows).
///
/// Token files are written and deleted locally by this binary; the server
/// never touches the operator's filesystem.
use toaster_talk_common::{parse_quoted_line, sanitize_filename};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;
#[cfg(unix)]
use tokio::net::UnixStream;

#[derive(Debug, Parser)]
#[command(name = "toaster-talk-cli", about = "ToasterTalk management CLI")]
struct Cli {
    /// Path to the management socket on Unix, or the named pipe name on Windows
    /// (default: /run/toaster-talk/mgmt.sock on Linux, /usr/local/var/run/toaster-talk/mgmt.sock
    /// on macOS, or \\.\pipe\toaster-talk on Windows)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Directory where token files are stored (default: ~/.toaster-talk/tokens)
    #[arg(long, global = true)]
    token_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create one or more rooms and write a token file locally for each
    Addrm {
        /// Human-readable room name(s) (e.g. "qa-review") — one or more
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
        /// Room TTL in days, applied to every room (default: server default)
        #[arg(long)]
        ttl: Option<u64>,
        /// Token filename without extension (default: derived from name);
        /// only valid when creating a single room
        #[arg(long)]
        file: Option<String>,
    },
    /// Delete one or more rooms
    Delrm {
        /// Room name(s) to delete — one or more
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
    },
    /// List all active rooms
    Lsrm,
    /// Get details about a specific room (including member names)
    Gtrm {
        /// Room name to inspect
        name: String,
    },
    /// Kick one or more named members from a room (does not delete the room)
    Kckm {
        /// Room name
        name: String,
        /// Member name(s) to kick — one or more
        #[arg(required = true, num_args = 1..)]
        members: Vec<String>,
    },
    /// Kick all members from one or more rooms (does not delete the rooms)
    Kckall {
        /// Room name(s) — one or more
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
    },
    /// Create seat tokens (member credentials) for one or more members of a room
    #[command(name = "addmb")]
    AddMember {
        /// Room name the seats belong to
        name: String,
        /// Member name(s) to authorise (1–40 chars, [a-zA-Z0-9_-]) — one or more
        #[arg(required = true, num_args = 1..)]
        members: Vec<String>,
        /// Seat TTL in days, applied to every member; 0 or omit to inherit room lifetime
        #[arg(long)]
        ttl: Option<u64>,
        /// Token filename hint without extension, written under tokens/user/<room>/
        /// (default: the member name); only valid when adding a single member
        #[arg(long)]
        file: Option<String>,
    },
    /// Delete seat tokens (evicts each member if connected)
    #[command(name = "delmb")]
    DeleteMember {
        /// Room name
        name: String,
        /// Member name(s) whose seat to delete — one or more
        #[arg(required = true, num_args = 1..)]
        members: Vec<String>,
    },
    /// List all seat tokens for a room
    #[command(name = "lsmb")]
    ListMembers {
        /// Room name to inspect
        name: String,
    },
    /// Dump a read-only YAML snapshot of the server's live in-memory state
    /// (rooms, members, inboxes, sessions) for diagnosing stuck deliveries.
    /// Output is YAML — pipe to `yq` for analysis.
    Dump,
    /// Trigger a live configuration + room-store reload (TDD 8.17) — the same
    /// effect as `systemctl reload` (SIGHUP) on Unix, and the only reload
    /// trigger on Windows. Blocks until the reload has actually been applied.
    Reload,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let socket_path = cli.socket.unwrap_or_else(default_socket);
    let token_dir = cli.token_dir.unwrap_or_else(default_token_dir);

    match &cli.command {
        Command::Addrm { names, ttl, file } => {
            require_single_for_file(file.as_deref(), names.len());
            let mut req = batch_request("ADDRM", &[], names);
            if let Some(t) = ttl {
                req.push_str(&format!(" TTL {t}"));
            }
            match send_request(&socket_path, &req).await {
                Ok(response) => handle_addrm_response(&response, file.as_deref(), &token_dir),
                Err(e) => fatal(e),
            }
        }
        Command::Delrm { names } => {
            let req = batch_request("DELRM", &[], names);
            match send_request(&socket_path, &req).await {
                Ok(response) => handle_delrm_response(&response),
                Err(e) => fatal(e),
            }
        }
        Command::Lsrm => match send_request(&socket_path, "LSRM").await {
            Ok(response) => handle_lsrm_response(&response),
            Err(e) => fatal(e),
        },
        Command::Gtrm { name } => {
            let req = format!("GTRM \"{name}\"");
            match send_request(&socket_path, &req).await {
                Ok(response) => handle_gtrm_response(&response),
                Err(e) => fatal(e),
            }
        }
        Command::Kckm { name, members } => {
            let mut req = format!("KCKM \"{name}\"");
            for member in members {
                req.push_str(&format!(" \"{member}\""));
            }
            match send_request(&socket_path, &req).await {
                Ok(response) => handle_kckm_response(&response),
                Err(e) => fatal(e),
            }
        }
        Command::Kckall { names } => {
            let req = batch_request("KCKALL", &[], names);
            match send_request(&socket_path, &req).await {
                Ok(response) => handle_kckall_response(&response),
                Err(e) => fatal(e),
            }
        }
        Command::AddMember {
            name,
            members,
            ttl,
            file,
        } => {
            require_single_for_file(file.as_deref(), members.len());
            let mut req = batch_request("ADDMB", &[name], members);
            if let Some(t) = ttl {
                req.push_str(&format!(" TTL {t}"));
            }
            match send_request(&socket_path, &req).await {
                Ok(response) => handle_addmb_response(&response, file.as_deref(), &token_dir),
                Err(e) => fatal(e),
            }
        }
        Command::DeleteMember { name, members } => {
            let req = batch_request("DELMB", &[name], members);
            match send_request(&socket_path, &req).await {
                Ok(response) => handle_delmb_response(&response),
                Err(e) => fatal(e),
            }
        }
        Command::ListMembers { name } => {
            let req = format!("LSMB \"{name}\"");
            match send_request(&socket_path, &req).await {
                Ok(response) => handle_lsmb_response(&response),
                Err(e) => fatal(e),
            }
        }
        Command::Dump => match send_request(&socket_path, "DUMP").await {
            Ok(response) => handle_dump_response(&response),
            Err(e) => fatal(e),
        },
        Command::Reload => match send_request(&socket_path, "RELOAD").await {
            Ok(response) => handle_reload_response(&response),
            Err(e) => fatal(e),
        },
    }
}

/// Build `COMMAND "<lead>" ... "<item>" ...` with every argument double-quoted.
fn batch_request(command: &str, lead: &[&String], items: &[String]) -> String {
    let mut req = command.to_string();
    for arg in lead.iter().copied().chain(items.iter()) {
        req.push_str(&format!(" \"{arg}\""));
    }
    req
}

/// `--file` names one token file, so it cannot be applied to several items.
fn require_single_for_file(file: Option<&str>, count: usize) {
    if file.is_some() && count > 1 {
        eprintln!("error: --file names a single token file and cannot be used with {count} names");
        std::process::exit(2);
    }
}

/// Split a (possibly multi-line) mgmt response into its per-item lines, printing
/// each `-` failure line as it is met. Returns the `+` lines and whether any
/// item failed; the caller finishes by calling `exit_if_failed`.
fn split_batch_response(response: &str) -> (Vec<&str>, bool) {
    let mut ok_lines = Vec::new();
    let mut failed = false;
    for line in response.lines().filter(|l| !l.is_empty()) {
        if let Some(msg) = line.strip_prefix('-') {
            eprintln!("Server error: {msg}");
            failed = true;
        } else {
            ok_lines.push(line.strip_prefix('+').unwrap_or(line));
        }
    }
    (ok_lines, failed)
}

fn exit_if_failed(failed: bool) {
    if failed {
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn default_socket() -> PathBuf {
    // Matches RUN_DIR in the Makefile's _install-macos recipe: NOT /run, which
    // macOS clears on every boot with no systemd RuntimeDirectory= equivalent.
    PathBuf::from("/usr/local/var/run/toaster-talk/mgmt.sock")
}
#[cfg(all(unix, not(target_os = "macos")))]
fn default_socket() -> PathBuf {
    PathBuf::from("/run/toaster-talk/mgmt.sock")
}
#[cfg(windows)]
fn default_socket() -> PathBuf {
    PathBuf::from(r"\\.\pipe\toaster-talk")
}

fn default_token_dir() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".toaster-talk/tokens")
}

fn fatal(msg: String) -> ! {
    eprintln!("error: {msg}");
    eprintln!("Is toaster-talk-server running? {}", server_status_hint());
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn server_status_hint() -> &'static str {
    "(sudo launchctl list | grep toaster-talk)"
}
#[cfg(all(unix, not(target_os = "macos")))]
fn server_status_hint() -> &'static str {
    "(systemctl status toaster-talk)"
}
#[cfg(windows)]
fn server_status_hint() -> &'static str {
    "(check with: Get-Service ToasterTalk)"
}

/// Open the management transport: a Unix domain socket on Unix, a named pipe on
/// Windows. Kept separate from `send_request` so the retry-on-busy loop needed
/// only on Windows (`ERROR_PIPE_BUSY`) doesn't leak into the shared protocol code.
#[cfg(unix)]
async fn connect(socket_path: &Path) -> Result<UnixStream, String> {
    UnixStream::connect(socket_path)
        .await
        .map_err(|e| format!("could not connect to {}: {e}", socket_path.display()))
}

#[cfg(windows)]
async fn connect(
    pipe_path: &Path,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, String> {
    let pipe_name = pipe_path.to_string_lossy().into_owned();
    // ERROR_PIPE_BUSY (231): a server is listening but every instance is
    // momentarily taken — the mgmt listener always keeps one spare instance ready
    // (see toaster-talk-server::mgmt::run_mgmt_listener), so this clears within a
    // request or two. Retry briefly instead of failing the call outright.
    const ERROR_PIPE_BUSY: i32 = 231;
    const MAX_ATTEMPTS: u32 = 20;
    let mut last_err = None;
    for _ in 0..MAX_ATTEMPTS {
        match ClientOptions::new().open(&pipe_name) {
            Ok(client) => return Ok(client),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(format!("could not connect to {pipe_name}: {e}")),
        }
    }
    Err(format!(
        "could not connect to {pipe_name}: still busy after {MAX_ATTEMPTS} attempts ({})",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

async fn send_request(socket_path: &Path, request: &str) -> Result<String, String> {
    let stream = connect(socket_path).await?;

    let (reader, mut writer) = tokio::io::split(stream);

    writer
        .write_all(format!("{request}\r\n").as_bytes())
        .await
        .map_err(|e| format!("write error: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("flush error: {e}"))?;

    let mut lines = BufReader::new(reader).lines();
    let mut response_lines = Vec::new();
    loop {
        match lines.next_line().await {
            Ok(Some(l)) => response_lines.push(l.trim_end_matches('\r').to_string()),
            Ok(None) => break,
            Err(e) => return Err(format!("read error: {e}")),
        }
    }

    Ok(response_lines.join("\n"))
}

// ── ADDRM ─────────────────────────────────────────────────────────────────────

fn handle_addrm_response(response: &str, file_hint: Option<&str>, token_dir: &Path) {
    let (ok_lines, failed) = split_batch_response(response);
    for msg in ok_lines {
        // ROOM_CREATED "<name>" <token> <expires_unix>
        let fields = parse_quoted_line(msg);
        if fields.first().map(|s| s.as_str()) == Some("ROOM_CREATED") && fields.len() >= 3 {
            let name = &fields[1];
            let token = &fields[2];
            let expires = fields.get(3).map(|s| s.as_str()).unwrap_or("?");
            let (tok_path, reused) = write_token_file(name, token, file_hint, token_dir);
            println!("Room created.");
            println!("  Name:       {name}");
            println!("  Token file: {}{}", tok_path.display(), reuse_note(reused));
            println!("  Expires:    {}", format_unix_ts(expires));
        } else {
            println!("{msg}");
        }
    }
    exit_if_failed(failed);
}

/// Suffix for a printed token path when an identical file was already there.
fn reuse_note(reused: bool) -> &'static str {
    if reused {
        " (unchanged: identical file already present)"
    } else {
        ""
    }
}

/// Write a 2-line room token file under `token_dir` with mode 0600. Returns the path.
/// The flag is `true` when an identical file already existed and none was written.
fn write_token_file(
    name: &str,
    token: &str,
    file_hint: Option<&str>,
    token_dir: &Path,
) -> (PathBuf, bool) {
    let base = sanitize_filename(file_hint.unwrap_or(name));
    let content = format!("{name}\n{token}\n");
    write_token_file_inner(token_dir, &base, &content, name)
}

// ── DELRM ─────────────────────────────────────────────────────────────────────

fn handle_delrm_response(response: &str) {
    let (ok_lines, failed) = split_batch_response(response);
    for msg in ok_lines {
        // ROOM_DELETED "<name>" evicted:<count>
        let fields = parse_quoted_line(msg);
        if fields.first().map(|s| s.as_str()) == Some("ROOM_DELETED") {
            println!(
                "Room deleted: {}",
                fields.get(1).map(|s| s.as_str()).unwrap_or("?")
            );
            for field in &fields[1..] {
                if let Some(n) = field.strip_prefix("evicted:") {
                    println!("  Members evicted: {n}");
                }
            }
        } else {
            println!("{msg}");
        }
    }
    exit_if_failed(failed);
}

// ── LSRM ──────────────────────────────────────────────────────────────────────

fn handle_lsrm_response(response: &str) {
    let lines = multiline_response_lines(response);
    let first = lines[0];

    if let Some(count_str) = first.strip_prefix('*') {
        let count: usize = match count_str.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!(
                    "error: malformed server response (expected room count, got {:?})",
                    count_str.trim()
                );
                std::process::exit(1);
            }
        };
        if count == 0 {
            println!("No active rooms.");
            return;
        }
        println!("{count} active room(s):\n");
        for line in &lines[1..] {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let raw = fields.first().copied().unwrap_or("?");
            let name = raw.trim_matches('"');
            let created = fields
                .iter()
                .find_map(|f| f.strip_prefix("created:"))
                .unwrap_or("?");
            let expires = fields
                .iter()
                .find_map(|f| f.strip_prefix("expires:"))
                .unwrap_or("?");
            let members = fields
                .iter()
                .find_map(|f| f.strip_prefix("members:"))
                .unwrap_or("?");
            println!("  {name}");
            println!("    Created:  {}", format_unix_ts(created));
            println!("    Expires:  {}", format_unix_ts(expires));
            println!("    Members:  {members}");
        }
    } else {
        println!("{response}");
    }
}

// ── GTRM ──────────────────────────────────────────────────────────────────────

fn handle_gtrm_response(response: &str) {
    let lines = multiline_response_lines(response);
    let first = lines[0];

    if let Some(rest) = first.strip_prefix("+ROOM ") {
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let raw_name = fields.first().copied().unwrap_or("?");
        let name = raw_name.trim_matches('"');
        let created = fields
            .iter()
            .find_map(|f| f.strip_prefix("created:"))
            .unwrap_or("?");
        let expires = fields
            .iter()
            .find_map(|f| f.strip_prefix("expires:"))
            .unwrap_or("?");
        let members_count = fields
            .iter()
            .find_map(|f| f.strip_prefix("members:"))
            .unwrap_or("0");

        println!("Room: {name}");
        println!("  Created:  {}", format_unix_ts(created));
        println!("  Expires:  {}", format_unix_ts(expires));
        println!("  Members:  {members_count}");

        let member_lines: Vec<&str> = lines[1..]
            .iter()
            .copied()
            .filter(|l| !l.is_empty())
            .collect();
        if !member_lines.is_empty() {
            println!("  Member names:");
            for member in member_lines {
                println!("    - {member}");
            }
        }
    } else {
        println!("{response}");
    }
}

// ── KCKM ──────────────────────────────────────────────────────────────────────

fn handle_kckm_response(response: &str) {
    if let Some(msg) = response.strip_prefix('+') {
        // +KICKED "<room>" kicked:<member> [kicked:<member> ...] [notfound:<member> ...]
        let fields = parse_quoted_line(msg);
        if fields.first().map(|s| s.as_str()) == Some("KICKED") {
            let kicked: Vec<&str> = fields
                .iter()
                .filter_map(|f| f.strip_prefix("kicked:"))
                .collect();
            let not_found: Vec<&str> = fields
                .iter()
                .filter_map(|f| f.strip_prefix("notfound:"))
                .collect();
            if kicked.len() == 1 && not_found.is_empty() {
                println!("Member kicked: {}", kicked[0]);
            } else {
                println!("Members kicked ({}): {}", kicked.len(), kicked.join(", "));
            }
            if !not_found.is_empty() {
                println!("Not found ({}): {}", not_found.len(), not_found.join(", "));
            }
        } else {
            println!("{msg}");
        }
    } else if let Some(msg) = response.strip_prefix('-') {
        eprintln!("Server error: {msg}");
        std::process::exit(1);
    } else {
        println!("{response}");
    }
}

// ── KCKALL ────────────────────────────────────────────────────────────────────

fn handle_kckall_response(response: &str) {
    let (ok_lines, failed) = split_batch_response(response);
    for msg in ok_lines {
        // KICKED_ALL "<room>" evicted:N
        let fields = parse_quoted_line(msg);
        if fields.first().map(|s| s.as_str()) == Some("KICKED_ALL") {
            let count = fields
                .iter()
                .find_map(|f| f.strip_prefix("evicted:"))
                .unwrap_or("?");
            println!(
                "All members kicked: {}",
                fields.get(1).map(|s| s.as_str()).unwrap_or("?")
            );
            println!("  Members evicted: {count}");
        } else {
            println!("{msg}");
        }
    }
    exit_if_failed(failed);
}

// ── ADDMB ─────────────────────────────────────────────────────────────────────

fn handle_addmb_response(response: &str, file_hint: Option<&str>, token_dir: &Path) {
    let (ok_lines, failed) = split_batch_response(response);
    for msg in ok_lines {
        // SEAT_CREATED "<room>" "<member>" <seat_token> <expires_unix>
        let fields = parse_quoted_line(msg);
        if fields.first().map(|s| s.as_str()) == Some("SEAT_CREATED") && fields.len() >= 4 {
            let (room, member, seat_token) = (&fields[1], &fields[2], &fields[3]);
            let expires = fields.get(4).map(|s| s.as_str()).unwrap_or("?");
            let (tok_path, reused) =
                write_seat_token_file(room, member, seat_token, file_hint, token_dir);
            println!("Seat token created.");
            println!("  Room:       {room}");
            println!("  Member:     {member}");
            println!("  Token file: {}{}", tok_path.display(), reuse_note(reused));
            println!("  Expires:    {}", format_unix_ts(expires));
        } else {
            println!("{msg}");
        }
    }
    exit_if_failed(failed);
}

/// Write a 3-line seat token file at `token_dir/user/<room>/<member>.tok`, mode 0600.
///
/// The room name becomes a per-room subdirectory and the member name the filename,
/// so seats are grouped by room and self-describing on disk. Both components are
/// passed through `sanitize_filename` (room names may legally contain characters
/// like `/` that are unsafe in a path). If `file_hint` is supplied it overrides the
/// member-name filename. Collisions are resolved by the shared unique-name rule
/// (`_1`, `_2`, …) within the room subdirectory.
fn write_seat_token_file(
    room: &str,
    member: &str,
    seat_token: &str,
    file_hint: Option<&str>,
    token_dir: &Path,
) -> (PathBuf, bool) {
    let room_dir = token_dir.join("user").join(sanitize_filename(room));
    let base = sanitize_filename(file_hint.unwrap_or(member));
    let content = format!("{room}\n{member}\n{seat_token}\n");
    write_token_file_inner(&room_dir, &base, &content, member)
}

// ── DELMB ─────────────────────────────────────────────────────────────────────

fn handle_delmb_response(response: &str) {
    let (ok_lines, failed) = split_batch_response(response);
    let mut deleted = 0;
    for msg in ok_lines {
        // SEAT_DELETED "<room>" "<member>"
        let fields = parse_quoted_line(msg);
        if fields.first().map(|s| s.as_str()) == Some("SEAT_DELETED") && fields.len() >= 3 {
            println!("Seat token deleted: {}", fields[2]);
            deleted += 1;
        } else {
            println!("{msg}");
        }
    }
    if deleted > 0 {
        println!("  Note: local token files (if any) were NOT removed.");
    }
    exit_if_failed(failed);
}

// ── LSMB ──────────────────────────────────────────────────────────────────────

fn handle_lsmb_response(response: &str) {
    let lines = multiline_response_lines(response);
    let first = lines[0];

    if let Some(count_str) = first.strip_prefix('*') {
        let count: usize = match count_str.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!(
                    "error: malformed server response (expected count, got {:?})",
                    count_str.trim()
                );
                std::process::exit(1);
            }
        };
        if count == 0 {
            println!("No seat tokens registered.");
            return;
        }
        println!("{count} seat token(s):\n");
        for line in &lines[1..] {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let raw = fields.first().copied().unwrap_or("?");
            let member = raw.trim_matches('"');
            let created = fields
                .iter()
                .find_map(|f| f.strip_prefix("created:"))
                .unwrap_or("?");
            let expires = fields
                .iter()
                .find_map(|f| f.strip_prefix("expires:"))
                .unwrap_or("?");
            println!("  {member}");
            println!("    Created: {}", format_unix_ts(created));
            println!("    Expires: {}", format_unix_ts(expires));
        }
    } else {
        println!("{response}");
    }
}

// ── DUMP ──────────────────────────────────────────────────────────────────────

/// Print the YAML diagnostic body verbatim so it can be piped to `yq`. The
/// server response is a `+DUMP` header line followed by the YAML body; only the
/// body is printed (the header is stripped).
fn handle_dump_response(response: &str) {
    let lines = multiline_response_lines(response);
    if lines[0].trim() == "+DUMP" {
        for line in &lines[1..] {
            println!("{line}");
        }
    } else {
        // Unexpected shape — print as-is rather than silently swallowing.
        println!("{response}");
    }
}

// ── RELOAD ────────────────────────────────────────────────────────────────────

fn handle_reload_response(response: &str) {
    if response.trim() == "+RELOADED" {
        println!("Configuration and room store reloaded.");
    } else if let Some(msg) = response.strip_prefix('-') {
        eprintln!("Server error: {msg}");
        std::process::exit(1);
    } else {
        println!("{response}");
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Split a multi-line server response into its lines, handling the shared
/// preamble for the LSRM/GTRM/LSMB list handlers (M-R4-C06): exit on an empty
/// response or a server-error (`-`) first line. Returns the lines on success.
fn multiline_response_lines(response: &str) -> Vec<&str> {
    let lines: Vec<&str> = response.lines().collect();
    if lines.is_empty() {
        eprintln!("error: empty response from server");
        std::process::exit(1);
    }
    if let Some(msg) = lines[0].strip_prefix('-') {
        eprintln!("Server error: {msg}");
        std::process::exit(1);
    }
    lines
}

/// Open a new token file at `path`, permissioned so only the owning account can
/// read it — mode `0600` on Unix; on Windows, `create_new` under the user's own
/// profile inherits a same-user ACL, the accepted equivalent (no `chmod`-analog
/// call is needed).
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

/// Shared inner writer: collision-safe, permissioned token file creation.
/// Finds a free path under `dir` with base `base`, writes `content`, returns the path.
fn write_token_file_inner(dir: &Path, base: &str, content: &str, label: &str) -> (PathBuf, bool) {
    use std::io::Write;

    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!(
            "warning: could not create token directory {}: {e}",
            dir.display()
        );
    }

    // Walk `<base>.tok`, `<base>_1.tok`, … : the first identical file is reused
    // (re-running a command must not litter near-duplicates), the first free slot
    // is where a new file goes.
    let names =
        std::iter::once(format!("{base}.tok")).chain((1..=999).map(|n| format!("{base}_{n}.tok")));
    let mut path = None;
    for name in names {
        let candidate = dir.join(name);
        if !candidate.exists() {
            path = Some(candidate);
            break;
        }
        if std::fs::read_to_string(&candidate).is_ok_and(|existing| existing == content) {
            return (candidate, true);
        }
    }
    let Some(path) = path else {
        eprintln!(
            "error: no free token filename for '{label}' in {}",
            dir.display()
        );
        std::process::exit(1);
    };

    match open_token_file(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(content.as_bytes()) {
                eprintln!(
                    "warning: could not write token file {}: {e}",
                    path.display()
                );
            }
        }
        Err(e) => {
            eprintln!(
                "warning: could not create token file {}: {e}",
                path.display()
            );
        }
    }

    (path, false)
}

/// Convert a raw Unix timestamp string (e.g. "1749340800") to a human-readable
/// local-timezone date/time string (e.g. "2025-06-08 14:30 +0200"). Non-numeric
/// values are returned unchanged so callers need not special-case missing/unknown
/// fields.
fn format_unix_ts(raw: &str) -> String {
    use chrono::{DateTime, Local, TimeZone};
    match raw.trim().parse::<i64>() {
        Ok(secs) => Local
            .timestamp_opt(secs, 0)
            .single()
            .map(|dt: DateTime<Local>| dt.format("%Y-%m-%d %H:%M %z").to_string())
            .unwrap_or_else(|| raw.to_string()),
        Err(_) => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_unix_ts_looks_like_date() {
        // Result is local-timezone dependent, so just verify the shape.
        let result = format_unix_ts("1767225600");
        assert!(
            !result.chars().all(|c| c.is_ascii_digit()),
            "must not be a raw integer"
        );
        // Expect "YYYY-MM-DD HH:MM ±HHMM"
        assert!(result.contains('-'), "must contain date separators");
        assert!(
            result.contains('+') || result.contains('-'),
            "must contain a UTC offset"
        );
    }

    #[test]
    fn test_format_unix_ts_passthrough_non_numeric() {
        assert_eq!(format_unix_ts("?"), "?");
        assert_eq!(format_unix_ts("unknown"), "unknown");
    }

    #[test]
    fn test_seat_token_file_path_layout() {
        // Default layout: tokens/user/<room>/<member>.tok with the 3-line body.
        let dir = tempfile::TempDir::new().unwrap();
        let (path, reused) = write_seat_token_file("qa-room", "alice", "tok-abc", None, dir.path());
        assert!(!reused);
        assert_eq!(
            path,
            dir.path().join("user").join("qa-room").join("alice.tok")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "qa-room\nalice\ntok-abc\n"
        );
    }

    #[test]
    fn test_seat_token_file_collision_gets_suffix_within_room_dir() {
        // A second seat for the same room+member gets a numeric suffix in the
        // same room subdirectory, not an overwrite.
        let dir = tempfile::TempDir::new().unwrap();
        let (p1, _) = write_seat_token_file("room1", "bob", "t1", None, dir.path());
        let (p2, _) = write_seat_token_file("room1", "bob", "t2", None, dir.path());
        assert_eq!(p1, dir.path().join("user").join("room1").join("bob.tok"));
        assert_eq!(p2, dir.path().join("user").join("room1").join("bob_1.tok"));
    }

    #[test]
    fn test_seat_token_file_hint_overrides_member_filename() {
        // --file overrides the member-name filename but keeps the room subdirectory.
        let dir = tempfile::TempDir::new().unwrap();
        let (path, _) = write_seat_token_file("room1", "carol", "tok", Some("custom"), dir.path());
        assert_eq!(
            path,
            dir.path().join("user").join("room1").join("custom.tok")
        );
    }

    #[test]
    fn test_seat_token_file_identical_content_is_reused() {
        // Re-running addmb with the same credential must not write bob_1.tok.
        let dir = tempfile::TempDir::new().unwrap();
        let (p1, r1) = write_seat_token_file("room1", "bob", "t1", None, dir.path());
        let (p2, r2) = write_seat_token_file("room1", "bob", "t1", None, dir.path());
        assert!(!r1 && r2, "second identical write must be reported reused");
        assert_eq!(p1, p2);
        assert!(!p1.with_file_name("bob_1.tok").exists());
    }

    #[test]
    fn test_seat_token_file_reuses_identical_suffixed_file() {
        // bob.tok differs, bob_1.tok matches: reuse bob_1.tok rather than make bob_2.tok.
        let dir = tempfile::TempDir::new().unwrap();
        write_seat_token_file("room1", "bob", "t1", None, dir.path());
        let (p2, _) = write_seat_token_file("room1", "bob", "t2", None, dir.path());
        let (p3, reused) = write_seat_token_file("room1", "bob", "t2", None, dir.path());
        assert!(reused);
        assert_eq!(p2, p3);
    }

    #[test]
    fn test_room_token_file_identical_content_is_reused() {
        let dir = tempfile::TempDir::new().unwrap();
        let (p1, _) = write_token_file("alpha", "tok", None, dir.path());
        let (p2, reused) = write_token_file("alpha", "tok", None, dir.path());
        assert!(reused);
        assert_eq!(p1, p2);
        let (p3, reused) = write_token_file("alpha", "other", None, dir.path());
        assert!(!reused);
        assert_eq!(p3, dir.path().join("alpha_1.tok"));
    }
}
