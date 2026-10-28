use crate::{
    config::Config,
    rooms::{RoomStore, RoomToken},
    state::{AuthMethod, MemberName, SharedAppState},
};
use arc_swap::ArcSwap;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Execute one sweep cycle: evict expired messages, expired rooms, expired seat
/// tokens, idle non-seat members, and (when `seat_idle_ttl_secs > 0`) dead seat
/// members.
/// Extracted from `run_sweeper` so integration tests can trigger a sweep directly.
pub async fn do_sweep(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    member_idle_ttl_secs: u64,
    seat_idle_ttl_secs: u64,
) {
    let now = Instant::now();

    // ── Step 1: sweep expired messages ───────────────────────────────────────
    let mut total_evicted_messages = 0usize;
    {
        let app_read = app.read().await;
        for room_state in app_read.rooms.values() {
            for (member_name, member_state) in &room_state.members {
                let evicted = member_state.inbox.evict_expired(now).await;
                if evicted > 0 {
                    debug!(
                        member = %member_name,
                        evicted,
                        "expired messages evicted from inbox"
                    );
                    total_evicted_messages += evicted;
                }
            }
        }
    }

    // ── Step 2: sweep expired rooms ──────────────────────────────────────────
    let expired_rooms = {
        let mut store_write = store.write().await;
        match store_write.remove_expired().await {
            Ok(rooms) => rooms,
            Err(e) => {
                warn!(error = %e, "TTL sweeper failed to remove expired rooms");
                vec![]
            }
        }
    };

    if !expired_rooms.is_empty() {
        let mut app_write = app.write().await;
        for room in &expired_rooms {
            app_write
                .evict_room(&room.token, &room.name, "room expired")
                .await;
        }
        warn!(
            evicted_messages = total_evicted_messages,
            evicted_rooms = expired_rooms.len(),
            "TTL sweep: rooms expired and members evicted"
        );
    } else if total_evicted_messages > 0 {
        info!(
            evicted_messages = total_evicted_messages,
            "TTL sweep: expired messages purged"
        );
    } else {
        debug!("TTL sweep complete — nothing to evict");
    }

    // ── Step 3: sweep expired seat tokens ────────────────────────────────────
    {
        let mut store_write = store.write().await;
        match store_write.remove_expired_seats().await {
            Ok(n) if n > 0 => info!(swept = n, "TTL sweep: expired seat tokens removed"),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "TTL sweeper failed to remove expired seat tokens"),
        }
    }

    // ── Step 4: sweep idle members (both tiers) ──────────────────────────────
    //
    // Each member is judged against the idle TTL for its auth tier, anchored on
    // `last_seen`:
    //   • room-token members → `member_idle_ttl_secs` (the ephemeral tier).
    //   • seat members        → `seat_idle_ttl_secs`, the *dead-seat* threshold
    //     (0 = disabled, seats fully exempt — the pre-reaper behavior). Seats are
    //     NOT exempt by default anymore: a seat whose client died would otherwise
    //     linger forever, black-holing every message sent to its inbox (the reaper
    //     closes the inbox so senders fail-closed instead — ANTI-PATTERNS #10).
    // A member actively parked in a blocking wait (`is_receive_active`) is ALWAYS
    // skipped, regardless of tier: its `last_seen` was stamped when the wait began,
    // and the wait is itself bounded by `max_receive_timeout_secs` — a dead-but-parked
    // agent self-terminates at its deadline and becomes evictable on the next sweep.
    // This is why a short seat TTL is safe: a legitimately-idle seat agent holds a
    // long receive and is never eligible. Run after the seat-token sweep so a member
    // whose seat was just removed is still judged by its auth method (provenance),
    // not by the now-absent seat. Room expiry (step 2) already caps every member.
    {
        // Token→name map for log lines, captured under a brief store read lock.
        let room_names: HashMap<RoomToken, String> = {
            let store_read = store.read().await;
            store_read
                .list()
                .iter()
                .map(|r| (r.token.clone(), r.name.clone()))
                .collect()
        };

        let member_ttl = Duration::from_secs(member_idle_ttl_secs);
        // seat_idle_ttl_secs == 0 disables seat reaping entirely.
        let seat_ttl = (seat_idle_ttl_secs > 0).then(|| Duration::from_secs(seat_idle_ttl_secs));
        let mut app_write = app.write().await;

        // Collect victims first, then evict — never mutate while iterating. A member
        // parked in a live receive is always skipped (see the block comment above).
        let mut victims: Vec<(RoomToken, MemberName, &'static str)> = Vec::new();
        for (room_token, room_state) in &app_write.rooms {
            for (member_name, member) in &room_state.members {
                if member.inbox.is_receive_active() {
                    continue;
                }
                let idle = member.last_seen.elapsed();
                let (over_ttl, reason) = match member.auth {
                    AuthMethod::RoomToken => (idle > member_ttl, "idle TTL expired"),
                    AuthMethod::Seat => match seat_ttl {
                        Some(t) => (idle > t, "seat idle TTL expired (dead seat reaped)"),
                        None => (false, ""),
                    },
                };
                if over_ttl {
                    victims.push((room_token.clone(), member_name.clone(), reason));
                }
            }
        }

        for (room_token, member_name, reason) in &victims {
            let room_name = room_names
                .get(room_token)
                .cloned()
                .unwrap_or_else(|| room_token.as_str().to_string());
            app_write
                .evict_member(room_token.as_str(), &room_name, member_name, reason)
                .await;
        }

        if !victims.is_empty() {
            info!(count = victims.len(), "TTL sweep: idle members evicted");
        }
    }

    // ── Step 5: reap idle roomless sessions ──────────────────────────────────
    //
    // A session minted by `connect` but never carried through `join` has no room
    // membership, so step 4 above never sees it — it would otherwise live
    // forever. Measured against `member_idle_ttl_secs`, anchored on connect
    // time (see `AppState::reap_idle_roomless_sessions`).
    {
        let mut app_write = app.write().await;
        let reaped = app_write
            .reap_idle_roomless_sessions(now, member_idle_ttl_secs)
            .await;
        if reaped > 0 {
            info!(count = reaped, "TTL sweep: idle roomless sessions reaped");
        }
    }

    // ── Step 6: prune stale seat-contention history ──────────────────────────
    //
    // A seat's flap history is kept only while it is frozen or still holds a
    // supersession inside its detection window; past that it carries no signal,
    // so drop it (every piece of server data has a TTL).
    {
        let mut app_write = app.write().await;
        let pruned = app_write.prune_seat_flaps(now);
        if pruned > 0 {
            debug!(
                count = pruned,
                "TTL sweep: stale seat-contention history pruned"
            );
        }
    }
}

/// Background task: wakes every `sweep_interval_secs` and runs one sweep cycle.
///
/// Reads `sweep_interval_secs`, `member_idle_ttl_secs`, and `seat_idle_ttl_secs`
/// from the live config cell at the top of every cycle, so a `SIGHUP` reload retunes
/// all three knobs — the new interval takes effect from the next sleep onward.
pub async fn run_sweeper(
    app: SharedAppState,
    store: Arc<RwLock<RoomStore>>,
    config: Arc<ArcSwap<Config>>,
) {
    info!(
        interval_secs = config.load().sweep_interval_secs,
        member_idle_ttl_secs = config.load().member_idle_ttl_secs,
        "TTL sweeper started"
    );

    loop {
        let (interval_secs, member_idle_ttl_secs, seat_idle_ttl_secs) = {
            let cfg = config.load();
            (
                cfg.sweep_interval_secs,
                cfg.member_idle_ttl_secs,
                cfg.seat_idle_ttl_secs,
            )
        };
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        do_sweep(&app, &store, member_idle_ttl_secs, seat_idle_ttl_secs).await;
    }
}
