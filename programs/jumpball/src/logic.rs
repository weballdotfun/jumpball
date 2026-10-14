//! Jumpball's rules as pure functions, so they're tested on their own.

use solana_program::{hash::hashv, pubkey::Pubkey};

/// A revealed secret is the next link of the committed chain: it hashes to the current head.
pub fn is_next_link(secret: &[u8; 32], head: &[u8; 32]) -> bool {
    hashv(&[secret]).to_bytes() == *head
}

/// The random result of a draw: the revealed secret mixed with the target slot's hash and the
/// draw's own address (so two draws sharing a slot never get the same result).
pub fn draw_result(secret: &[u8; 32], slot_hash: &[u8; 32], draw: &Pubkey) -> [u8; 32] {
    hashv(&[b"jumpball", secret, slot_hash, draw.as_ref()]).to_bytes()
}

/// The result of a forced draw (`force_settle`): the target slot's hash mixed with the fallback
/// slot's hash and the draw's address. Its domain tag differs from `draw_result`'s, and the two
/// preimages have different lengths (15 + 96 bytes against 8 + 96), so a forced result can never
/// equal a revealed one short of a SHA-256 collision.
///
/// Why this can't be steered (README "Known limitation"):
/// - Withholding a reveal gives the operator a choice between the known revealed result R and an
///   unknown fallback result F: one re-roll per draw. It cannot pick F: the fallback slot comes
///   `fallback_delay_slots` after the reveal window closes, so its hash doesn't exist when the
///   operator decides not to reveal. Every forced draw is public (`Feed::forced`); consumers should
///   keep the value decided by any single draw small (caps) and publish the forced count.
/// - `force_settle` is permissionless and one honest caller suffices: the result is fixed by the
///   slot hashes, whoever calls it. Reaching `expire` instead (a refund) needs everyone to abstain
///   for the whole fallback window.
/// - A skipped fallback slot rolls on to the next produced slot, as target slots do; validators keep
///   the slot-hash influence they have today (skipping their slot), and a forced draw has no secret
///   in it, so that influence is all that stands between a validator and the result.
pub fn forced_result(target_hash: &[u8; 32], fallback_hash: &[u8; 32], draw: &Pubkey) -> [u8; 32] {
    hashv(&[
        b"jumpball:forced",
        target_hash,
        fallback_hash,
        draw.as_ref(),
    ])
    .to_bytes()
}

/// The slot whose hash (or the next produced one's) settles a withheld draw.
pub fn fallback_slot(target_slot: u64, timeout_slots: u32, fallback_delay_slots: u32) -> u64 {
    target_slot
        .saturating_add(timeout_slots as u64)
        .saturating_add(fallback_delay_slots as u64)
}

/// A feed's slot settings must keep every hash a draw needs inside the SlotHashes sysvar: the
/// reveal window, and with a fallback also its delay and its window (the reveal window's length
/// again), since `force_settle` reads the target slot's hash at the window's end. `max` is the
/// program's cap, which leaves headroom under the sysvar's 512 slots.
pub fn slot_settings_fit(
    delay_slots: u32,
    timeout_slots: u32,
    fallback_delay_slots: u32,
    max: u32,
) -> bool {
    let fallback = if fallback_delay_slots == 0 {
        0
    } else {
        (fallback_delay_slots as u64) + (timeout_slots as u64)
    };
    delay_slots > 0
        && timeout_slots > 0
        && (delay_slots as u64) + (timeout_slots as u64) + fallback <= max as u64
}

/// The hash of the first slot at or after `slot` in the SlotHashes sysvar's data (a u64 count,
/// then (slot u64, hash [32]) entries, newest first), with that slot. A skipped slot has no hash,
/// so the next produced slot stands in: it is just as unknown when entries close. Read directly
/// (the sysvar is ~20 KB; deserialising it all is costly).
pub fn first_slot_hash_from(data: &[u8], slot: u64) -> Option<(u64, [u8; 32])> {
    let count = u64::from_le_bytes(data.get(0..8)?.try_into().ok()?) as usize;
    let entry = |i: usize| -> Option<(u64, [u8; 32])> {
        let at = 8 + i * 40;
        let s = u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?);
        Some((s, data.get(at + 8..at + 40)?.try_into().ok()?))
    };
    // Newest first, so slots fall as the index rises. Find the last index whose slot is >= `slot`.
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if entry(mid)?.0 >= slot {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 {
        return None; // nothing at or after `slot` yet
    }
    let (s, h) = entry(lo - 1)?;
    // The sysvar must still reach back to `slot` itself or earlier, or a skipped run can't be told
    // from history it no longer keeps.
    let oldest = entry(count.checked_sub(1)?)?.0;
    if oldest > slot {
        return None;
    }
    Some((s, h))
}

/// Where a pending draw stands at `now`.
#[derive(Debug, PartialEq, Eq)]
pub enum Timing {
    /// The target slot hasn't passed: its hash isn't known yet.
    TooEarly,
    /// The target slot has passed and the reveal is in time.
    Open,
    /// The reveal window closed: the draw has failed.
    TimedOut,
}

pub fn timing(now: u64, target_slot: u64, timeout_slots: u32) -> Timing {
    if now <= target_slot {
        Timing::TooEarly
    } else if now <= target_slot.saturating_add(timeout_slots as u64) {
        Timing::Open
    } else {
        Timing::TimedOut
    }
}

/// Where a withheld draw stands for `force_settle` at `now` (only on a feed with a fallback).
#[derive(Debug, PartialEq, Eq)]
pub enum ForceTiming {
    /// The fallback slot hasn't passed: its hash doesn't exist yet.
    TooEarly,
    /// The fallback slot has passed and the window is open: anyone may force-settle.
    Open,
    /// The fallback window closed too: the draw can only be expired (refund).
    Closed,
}

pub fn force_timing(
    now: u64,
    target_slot: u64,
    timeout_slots: u32,
    fallback_delay_slots: u32,
) -> ForceTiming {
    let fallback = fallback_slot(target_slot, timeout_slots, fallback_delay_slots);
    if now <= fallback {
        ForceTiming::TooEarly
    } else if now <= fallback.saturating_add(timeout_slots as u64) {
        ForceTiming::Open
    } else {
        ForceTiming::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(n: usize) -> Vec<[u8; 32]> {
        // s_n is the seed; s_{i-1} = H(s_i); returned as [s_0, s_1, …, s_n].
        let mut links = vec![[42u8; 32]];
        for _ in 0..n {
            let next = hashv(&[links.last().unwrap()]).to_bytes();
            links.push(next);
        }
        links.reverse();
        links
    }

    #[test]
    fn reveals_must_follow_the_chain_in_order() {
        let c = chain(5);
        assert!(is_next_link(&c[1], &c[0]));
        assert!(is_next_link(&c[2], &c[1]));
        assert!(!is_next_link(&c[2], &c[0]), "skipping a link is refused");
        assert!(
            !is_next_link(&[0u8; 32], &c[0]),
            "a made-up secret is refused"
        );
    }

    #[test]
    fn results_depend_on_the_secret_the_slot_hash_and_the_draw() {
        let (s, h, d) = ([1u8; 32], [2u8; 32], Pubkey::new_unique());
        let r = draw_result(&s, &h, &d);
        assert_eq!(r, draw_result(&s, &h, &d));
        assert_ne!(r, draw_result(&[3u8; 32], &h, &d));
        assert_ne!(r, draw_result(&s, &[3u8; 32], &d));
        assert_ne!(r, draw_result(&s, &h, &Pubkey::new_unique()));
    }

    #[test]
    fn finds_the_first_slot_hash_at_or_after_the_target() {
        // Newest first: slots 110, 109, 107, 106 (108 was skipped).
        let mut data = 4u64.to_le_bytes().to_vec();
        for (slot, b) in [(110u64, 1u8), (109, 2), (107, 3), (106, 4)] {
            data.extend(slot.to_le_bytes());
            data.extend([b; 32]);
        }
        assert_eq!(first_slot_hash_from(&data, 109), Some((109, [2; 32])));
        assert_eq!(
            first_slot_hash_from(&data, 108),
            Some((109, [2; 32])),
            "a skipped slot: the next one"
        );
        assert_eq!(first_slot_hash_from(&data, 106), Some((106, [4; 32])));
        assert_eq!(first_slot_hash_from(&data, 111), None, "not produced yet");
        assert_eq!(
            first_slot_hash_from(&data, 99),
            None,
            "older than the sysvar keeps"
        );
        assert_eq!(first_slot_hash_from(&[], 1), None);
    }

    #[test]
    fn forced_results_depend_on_both_slot_hashes_and_the_draw() {
        let (t, f, d) = ([1u8; 32], [2u8; 32], Pubkey::new_unique());
        let r = forced_result(&t, &f, &d);
        assert_eq!(r, forced_result(&t, &f, &d));
        assert_ne!(
            r,
            forced_result(&[3u8; 32], &f, &d),
            "the target slot's hash"
        );
        assert_ne!(
            r,
            forced_result(&t, &[3u8; 32], &d),
            "the fallback slot's hash"
        );
        assert_ne!(r, forced_result(&t, &f, &Pubkey::new_unique()), "the draw");
        assert_ne!(r, forced_result(&f, &t, &d), "the order of the hashes");
    }

    #[test]
    fn a_forced_result_differs_from_a_revealed_one_for_the_same_inputs() {
        let (a, b, d) = ([1u8; 32], [2u8; 32], Pubkey::new_unique());
        assert_ne!(forced_result(&a, &b, &d), draw_result(&a, &b, &d));
    }

    #[test]
    fn force_settle_timing() {
        // Target 100, a 20-slot reveal window, then 8 slots: fallback slot 128, open until 148.
        assert_eq!(fallback_slot(100, 20, 8), 128);
        assert_eq!(
            force_timing(120, 100, 20, 8),
            ForceTiming::TooEarly,
            "reveal window still open"
        );
        assert_eq!(
            force_timing(121, 100, 20, 8),
            ForceTiming::TooEarly,
            "window closed, fallback not yet"
        );
        assert_eq!(
            force_timing(128, 100, 20, 8),
            ForceTiming::TooEarly,
            "the fallback slot itself has no hash yet"
        );
        assert_eq!(force_timing(129, 100, 20, 8), ForceTiming::Open);
        assert_eq!(force_timing(148, 100, 20, 8), ForceTiming::Open);
        assert_eq!(force_timing(149, 100, 20, 8), ForceTiming::Closed);
        // Whenever force_settle is open, the operator's reveal window has closed.
        for now in 0..200 {
            if force_timing(now, 100, 20, 1) == ForceTiming::Open {
                assert_eq!(timing(now, 100, 20), Timing::TimedOut);
            }
        }
    }

    #[test]
    fn slot_settings_must_fit_the_sysvar() {
        assert!(
            slot_settings_fit(2, 150, 0, 400),
            "no fallback: delay + window"
        );
        assert!(slot_settings_fit(2, 398, 0, 400));
        assert!(!slot_settings_fit(2, 399, 0, 400));
        assert!(
            slot_settings_fit(2, 150, 98, 400),
            "2 + 150 + 98 + 150 = 400"
        );
        assert!(!slot_settings_fit(2, 150, 99, 400));
        assert!(
            !slot_settings_fit(0, 150, 0, 400),
            "at least one slot of delay"
        );
        assert!(!slot_settings_fit(2, 0, 0, 400), "a reveal window");
        assert!(!slot_settings_fit(2, 150, u32::MAX, 400), "no overflow");
    }

    #[test]
    fn timing_of_a_draw() {
        assert_eq!(timing(100, 100, 150), Timing::TooEarly);
        assert_eq!(timing(101, 100, 150), Timing::Open);
        assert_eq!(timing(250, 100, 150), Timing::Open);
        assert_eq!(timing(251, 100, 150), Timing::TimedOut);
    }
}
