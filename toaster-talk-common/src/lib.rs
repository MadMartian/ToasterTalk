/// Constant-time byte-slice equality. Resistant to timing side-channels.
///
/// Returns `true` iff `a == b`. Runs in time proportional to `a.len()` regardless
/// of where the slices first differ. Unequal lengths always return `false`.
///
/// This is the single source of truth for constant-time comparison on the token
/// verification path — both `ct_str_eq` and `rooms::RoomStore` token checks
/// delegate here so a future hardening change lands in exactly one place (M-R6-F10).
pub fn ct_bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a
        .iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));
    diff == 0
}

/// Constant-time string equality. Thin wrapper over [`ct_bytes_eq`].
pub fn ct_str_eq(a: &str, b: &str) -> bool {
    ct_bytes_eq(a.as_bytes(), b.as_bytes())
}

/// Format a duration in seconds as a compact abbreviated string (e.g. "2h", "5d").
pub fn format_duration_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Format a `Duration` as a full-word human-readable string (e.g. "2 hours", "5 days").
pub fn format_duration_human(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        let s = if secs == 1 { "second" } else { "seconds" };
        format!("{secs} {s}")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = if m == 1 { "minute" } else { "minutes" };
        format!("{m} {s}")
    } else if secs < 86400 {
        let h = secs / 3600;
        let s = if h == 1 { "hour" } else { "hours" };
        format!("{h} {s}")
    } else {
        let d = secs / 86400;
        let s = if d == 1 { "day" } else { "days" };
        format!("{d} {s}")
    }
}

/// Derive a filesystem-safe filename from a room name.
/// Spaces → underscores; non-alphanumeric chars (except `-` and `_`) are stripped; lowercased.
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c == ' ' { '_' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect::<String>()
        .to_lowercase()
}

/// Returns true iff `name` is a valid member name: non-empty, ≤40 chars, and
/// composed only of ASCII alphanumerics plus `_` and `-`.
///
/// The single definition of the member-name rules; the server's validating
/// `MemberName::new` constructor wraps it.
pub fn is_valid_member_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Returns true iff `name` is a valid room name: non-empty, ≤40 chars, and free
/// of spaces, double-quote characters (`"`), and null bytes (`\0`). Room names are
/// deliberately more permissive than member names (other punctuation is allowed).
///
/// Shared via the common crate so the predicate and the `InvalidRoomName` error
/// message cannot drift apart (L-R6-AP1).
pub fn is_valid_room_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && !name.contains(' ')
        && !name.contains('"')
        && !name.contains('\0')
}

/// Parse a command/response line into tokens, treating double-quoted strings as
/// single tokens with the quotes stripped. This is the canonical implementation
/// of the management-protocol quoting convention, shared by the server's mgmt
/// dispatcher and the CLI's response parser so they cannot drift apart.
pub fn parse_quoted_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_format_duration_secs_seconds() {
        assert_eq!(format_duration_secs(45), "45s");
    }

    #[test]
    fn test_format_duration_secs_minutes() {
        assert_eq!(format_duration_secs(120), "2m");
    }

    #[test]
    fn test_format_duration_secs_hours() {
        assert_eq!(format_duration_secs(7200), "2h");
    }

    #[test]
    fn test_format_duration_secs_days() {
        assert_eq!(format_duration_secs(86400 * 5), "5d");
    }

    #[test]
    fn test_format_duration_human_seconds() {
        assert_eq!(format_duration_human(Duration::from_secs(1)), "1 second");
        assert_eq!(format_duration_human(Duration::from_secs(30)), "30 seconds");
    }

    #[test]
    fn test_format_duration_human_minutes() {
        assert_eq!(format_duration_human(Duration::from_secs(60)), "1 minute");
        assert_eq!(format_duration_human(Duration::from_secs(120)), "2 minutes");
    }

    #[test]
    fn test_format_duration_human_hours() {
        assert_eq!(format_duration_human(Duration::from_secs(3600)), "1 hour");
        assert_eq!(format_duration_human(Duration::from_secs(7200)), "2 hours");
    }

    #[test]
    fn test_format_duration_human_days() {
        assert_eq!(format_duration_human(Duration::from_secs(86400)), "1 day");
        assert_eq!(
            format_duration_human(Duration::from_secs(86400 * 3)),
            "3 days"
        );
    }

    #[test]
    fn test_sanitize_filename_spaces() {
        assert_eq!(sanitize_filename("hello world"), "hello_world");
    }

    #[test]
    fn test_sanitize_filename_strips_special_chars() {
        assert_eq!(sanitize_filename("room/test!@#"), "roomtest");
    }

    #[test]
    fn test_sanitize_filename_lowercases() {
        assert_eq!(sanitize_filename("MyRoom"), "myroom");
    }

    #[test]
    fn test_is_valid_member_name_accepts_alnum_underscore_hyphen() {
        assert!(is_valid_member_name("alice"));
        assert!(is_valid_member_name("qa-agent"));
        assert!(is_valid_member_name("dev_01"));
        assert!(is_valid_member_name("A"));
    }

    #[test]
    fn test_is_valid_member_name_rejects_empty_and_overlong() {
        assert!(!is_valid_member_name(""));
        assert!(!is_valid_member_name(&"a".repeat(41)));
        assert!(is_valid_member_name(&"a".repeat(40)));
    }

    #[test]
    fn test_is_valid_member_name_rejects_spaces_and_symbols() {
        assert!(!is_valid_member_name("my name"));
        assert!(!is_valid_member_name("name!"));
        assert!(!is_valid_member_name("a\"b"));
        assert!(!is_valid_member_name("a\0b"));
    }

    #[test]
    fn test_parse_quoted_line_splits_on_whitespace() {
        assert_eq!(
            parse_quoted_line("ADDRM foo TTL 7"),
            vec!["ADDRM", "foo", "TTL", "7"]
        );
    }

    #[test]
    fn test_parse_quoted_line_preserves_quoted_spaces() {
        assert_eq!(
            parse_quoted_line("KCKM \"my room\" \"the member\""),
            vec!["KCKM", "my room", "the member"]
        );
    }

    #[test]
    fn test_parse_quoted_line_empty_input() {
        assert!(parse_quoted_line("").is_empty());
        assert!(parse_quoted_line("   ").is_empty());
    }

    #[test]
    fn test_ct_bytes_eq() {
        assert!(ct_bytes_eq(b"abc", b"abc"));
        assert!(!ct_bytes_eq(b"abc", b"abd"));
        assert!(!ct_bytes_eq(b"abc", b"abcd"));
        assert!(ct_bytes_eq(b"", b""));
    }

    #[test]
    fn test_ct_str_eq_delegates() {
        assert!(ct_str_eq("token", "token"));
        assert!(!ct_str_eq("token", "tokem"));
        assert!(!ct_str_eq("token", "tokens"));
    }

    #[test]
    fn test_is_valid_room_name_accepts_permissive() {
        assert!(is_valid_room_name("qa-review"));
        assert!(is_valid_room_name("room.1"));
        assert!(is_valid_room_name("a/b"));
        assert!(is_valid_room_name(&"a".repeat(40)));
    }

    #[test]
    fn test_is_valid_room_name_rejects() {
        assert!(!is_valid_room_name(""));
        assert!(!is_valid_room_name(&"a".repeat(41)));
        assert!(!is_valid_room_name("my room"));
        assert!(!is_valid_room_name("a\"b"));
        assert!(!is_valid_room_name("a\0b"));
    }
}
