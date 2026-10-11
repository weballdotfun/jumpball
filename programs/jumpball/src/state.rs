//! Jumpball's two accounts, in fixed little-endian layouts (no framework).
//!
//! A **feed** is one operator's committed hash chain. Its `head` starts as the chain's last link
//! (`s_0`) and becomes each revealed secret in turn, so every reveal must hash to the current head.
//! A **draw** is one request for randomness from a consuming program: it is assigned the feed's
//! next secret, locks at the slot it was made, and uses the hash of a later slot.

use solana_program::{program_error::ProgramError, pubkey::Pubkey};

pub const FEED_SEED: &[u8] = b"feed";
pub const DRAW_SEED: &[u8] = b"draw";

const FEED_TAG: [u8; 8] = *b"JBFEED01";
const DRAW_TAG: [u8; 8] = *b"JBDRAW01";

/// 0 tag [8] · 8 bump u8 · 9 pad [7] · 16 authority [32] · 48 head [32] · 80 id u64 ·
/// 88 assigned u64 · 96 revealed u64 · 104 failures u64 · 112 delay_slots u32 · 116 timeout_slots u32 ·
/// 120 fee_account [32] · 152 fee_amount u64 · 160 burn_bps u16 · 162 fee_kind u8 · 163 reserved [21].
pub const FEED_LEN: usize = 184;

/// What a feed's fee is paid in: SOL to a wallet, or a token (the mint of `fee_account`), part of
/// which may be burned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FeeKind {
    Sol = 0,
    Token = 1,
}
/// 0 tag [8] · 8 bump u8 · 9 status u8 · 10 pad [6] · 16 feed [32] · 48 requester [32] · 80 payer [32] ·
/// 112 nonce [32] · 144 secret_index u64 · 152 lock_slot u64 · 160 target_slot u64 · 168 result [32] ·
/// 200 reserved [32].
pub const DRAW_LEN: usize = 232;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DrawStatus {
    Pending = 0,
    Fulfilled = 1,
    Failed = 2,
}

impl DrawStatus {
    fn from_u8(v: u8) -> Result<Self, ProgramError> {
        match v {
            0 => Ok(Self::Pending),
            1 => Ok(Self::Fulfilled),
            2 => Ok(Self::Failed),
            _ => Err(ProgramError::InvalidAccountData),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    pub bump: u8,
    /// Who reveals secrets and can recommit a new chain.
    pub authority: Pubkey,
    /// The last revealed secret, or the chain's committed end before any reveal.
    pub head: [u8; 32],
    pub id: u64,
    /// Secrets handed to draws so far (the next draw gets `assigned + 1`).
    pub assigned: u64,
    /// Secrets revealed so far (each must hash to the previous head).
    pub revealed: u64,
    /// Draws that ran out of time (public: a high count means the operator isn't revealing).
    pub failures: u64,
    /// The draw uses the hash of `lock_slot + delay_slots`.
    pub delay_slots: u32,
    /// After `target_slot + timeout_slots` without a reveal the draw fails.
    pub timeout_slots: u32,
    /// The fee for each request (0 = free). SOL: a wallet; token: a token account of the fee mint.
    pub fee_kind: FeeKind,
    pub fee_account: Pubkey,
    pub fee_amount: u64,
    /// The share of a token fee burned from the payer, in basis points; the rest goes to `fee_account`.
    pub burn_bps: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draw {
    pub bump: u8,
    pub status: DrawStatus,
    pub feed: Pubkey,
    /// Who asked (a consuming program's signer); the only one who may close it.
    pub requester: Pubkey,
    /// Paid the rent; gets it back on close.
    pub payer: Pubkey,
    pub nonce: [u8; 32],
    pub secret_index: u64,
    pub lock_slot: u64,
    pub target_slot: u64,
    /// The random result once fulfilled; zero otherwise.
    pub result: [u8; 32],
}

fn u64_at(d: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(d[at..at + 8].try_into().unwrap())
}
fn u32_at(d: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(d[at..at + 4].try_into().unwrap())
}
fn key_at(d: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(d[at..at + 32].try_into().unwrap())
}
fn b32_at(d: &[u8], at: usize) -> [u8; 32] {
    d[at..at + 32].try_into().unwrap()
}

impl Feed {
    pub fn write(&self, out: &mut [u8]) -> Result<(), ProgramError> {
        if out.len() != FEED_LEN {
            return Err(ProgramError::InvalidAccountData);
        }
        out.fill(0);
        out[0..8].copy_from_slice(&FEED_TAG);
        out[8] = self.bump;
        out[16..48].copy_from_slice(self.authority.as_ref());
        out[48..80].copy_from_slice(&self.head);
        out[80..88].copy_from_slice(&self.id.to_le_bytes());
        out[88..96].copy_from_slice(&self.assigned.to_le_bytes());
        out[96..104].copy_from_slice(&self.revealed.to_le_bytes());
        out[104..112].copy_from_slice(&self.failures.to_le_bytes());
        out[112..116].copy_from_slice(&self.delay_slots.to_le_bytes());
        out[116..120].copy_from_slice(&self.timeout_slots.to_le_bytes());
        out[120..152].copy_from_slice(self.fee_account.as_ref());
        out[152..160].copy_from_slice(&self.fee_amount.to_le_bytes());
        out[160..162].copy_from_slice(&self.burn_bps.to_le_bytes());
        out[162] = self.fee_kind as u8;
        Ok(())
    }

    pub fn read(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() != FEED_LEN || d[0..8] != FEED_TAG {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            bump: d[8],
            authority: key_at(d, 16),
            head: b32_at(d, 48),
            id: u64_at(d, 80),
            assigned: u64_at(d, 88),
            revealed: u64_at(d, 96),
            failures: u64_at(d, 104),
            delay_slots: u32_at(d, 112),
            timeout_slots: u32_at(d, 116),
            fee_account: key_at(d, 120),
            fee_amount: u64_at(d, 152),
            burn_bps: u16::from_le_bytes([d[160], d[161]]),
            fee_kind: match d[162] {
                0 => FeeKind::Sol,
                1 => FeeKind::Token,
                _ => return Err(ProgramError::InvalidAccountData),
            },
        })
    }
}

impl Draw {
    pub fn write(&self, out: &mut [u8]) -> Result<(), ProgramError> {
        if out.len() != DRAW_LEN {
            return Err(ProgramError::InvalidAccountData);
        }
        out.fill(0);
        out[0..8].copy_from_slice(&DRAW_TAG);
        out[8] = self.bump;
        out[9] = self.status as u8;
        out[16..48].copy_from_slice(self.feed.as_ref());
        out[48..80].copy_from_slice(self.requester.as_ref());
        out[80..112].copy_from_slice(self.payer.as_ref());
        out[112..144].copy_from_slice(&self.nonce);
        out[144..152].copy_from_slice(&self.secret_index.to_le_bytes());
        out[152..160].copy_from_slice(&self.lock_slot.to_le_bytes());
        out[160..168].copy_from_slice(&self.target_slot.to_le_bytes());
        out[168..200].copy_from_slice(&self.result);
        Ok(())
    }

    pub fn read(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() != DRAW_LEN || d[0..8] != DRAW_TAG {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            bump: d[8],
            status: DrawStatus::from_u8(d[9])?,
            feed: key_at(d, 16),
            requester: key_at(d, 48),
            payer: key_at(d, 80),
            nonce: b32_at(d, 112),
            secret_index: u64_at(d, 144),
            lock_slot: u64_at(d, 152),
            target_slot: u64_at(d, 160),
            result: b32_at(d, 168),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_and_draw_round_trip() {
        let f = Feed {
            bump: 254,
            authority: Pubkey::new_unique(),
            head: [7; 32],
            id: 3,
            assigned: 10,
            revealed: 9,
            failures: 1,
            delay_slots: 2,
            timeout_slots: 150,
            fee_kind: FeeKind::Token,
            fee_account: Pubkey::new_unique(),
            fee_amount: 1_000_000,
            burn_bps: 5_000,
        };
        let mut buf = vec![0u8; FEED_LEN];
        f.write(&mut buf).unwrap();
        assert_eq!(Feed::read(&buf).unwrap(), f);
        let d = Draw {
            bump: 253,
            status: DrawStatus::Fulfilled,
            feed: Pubkey::new_unique(),
            requester: Pubkey::new_unique(),
            payer: Pubkey::new_unique(),
            nonce: [9; 32],
            secret_index: 10,
            lock_slot: 1000,
            target_slot: 1002,
            result: [5; 32],
        };
        let mut buf = vec![0u8; DRAW_LEN];
        d.write(&mut buf).unwrap();
        assert_eq!(Draw::read(&buf).unwrap(), d);
        buf[0] ^= 1;
        assert!(Draw::read(&buf).is_err());
    }
}
