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
    fn timing_of_a_draw() {
        assert_eq!(timing(100, 100, 150), Timing::TooEarly);
        assert_eq!(timing(101, 100, 150), Timing::Open);
        assert_eq!(timing(250, 100, 150), Timing::Open);
        assert_eq!(timing(251, 100, 150), Timing::TimedOut);
    }
}
