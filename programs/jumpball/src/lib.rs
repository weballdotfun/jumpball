//! Jumpball: provably fair randomness for Solana programs.
//!
//! An operator commits to a hash chain (a **feed**). A consuming program **requests** a draw, which
//! locks at that slot and is assigned the feed's next secret. Once a slot after the lock has a
//! hash, the operator **reveals** that secret: Jumpball checks it is the chain's next link and
//! mixes it with the slot hash for the result. On a feed with a fallback, a draw not revealed in
//! time is **force-settled** by anyone from a later slot's hash; without one (or if nobody settles
//! it in the fallback window) it **fails**, so the consumer can refund. See README.md for what a
//! withheld reveal can and can't do.
//!
//! Instructions (first byte):
//! 0 `init_feed`  id u64 · head [32] · delay_slots u32 · timeout_slots u32 · chain_length u64 ·
//!   fallback_delay_slots u32 · commit_authority [32] · allowed_requester [32] (zero = anyone)
//!   accounts: payer (signer, writable) · authority (signer) · feed (writable) · system program
//! 1 `request`    nonce [32] · max_fee u64 (optional) · fee_kind u8 (optional)
//!   accounts: payer (signer, writable) · requester (signer) · feed (writable) · draw (writable) · system program
//! 2 `reveal`     secret [32]
//!   accounts: authority (signer) · feed (writable) · draw (writable) · SlotHashes sysvar
//! 3 `expire`     (none)
//!   accounts: feed (writable) · draw (writable)
//! 4 `close_draw` (none): only once the draw's secret has been revealed
//!   accounts: requester (signer) · draw (writable) · payer (writable, the draw's payer) · feed
//! 5 `commit_next` head [32] · chain_length u64: the next chain, ahead of time
//!   accounts: commit_authority (signer) · feed (writable)
//! 6 `set_fee`    kind u8 · amount u64 · burn_bps u16 · fee_account [32]
//!   accounts: authority (signer) · feed (writable)
//! 7 `force_settle` (none)
//!   accounts: caller (signer) · feed (writable) · draw (writable) · SlotHashes sysvar

pub mod logic;
pub mod state;

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    log::sol_log_data,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::{self, Sysvar},
};
use solana_system_interface::{instruction as system_instruction, program as system_program};

use crate::logic::{
    draw_result, fallback_slot, first_slot_hash_from, force_timing, forced_result, is_next_link,
    slot_settings_fit, timing, ForceTiming, Timing,
};
use crate::state::{Draw, DrawStatus, FeeKind, Feed, DRAW_LEN, DRAW_SEED, FEED_LEN, FEED_SEED};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

/// The most slots a feed's settings may span (delay + reveal window, plus the fallback's delay and
/// window when it has one): the SlotHashes sysvar keeps the last 512 slots, so a later reveal or
/// force_settle could no longer find the hashes it needs. 400 leaves headroom under 512.
pub const MAX_TIMEOUT_SLOTS: u32 = 400;

/// Errors, as `ProgramError::Custom(n)`.
#[repr(u32)]
pub enum JumpballError {
    /// The revealed secret isn't the chain's next link.
    NotNextLink = 1,
    /// This draw's secret isn't the next one to reveal (draws are revealed in order).
    OutOfOrder = 2,
    /// The draw's target slot hasn't passed yet.
    TooEarly = 3,
    /// The target slot's hash (or a later one) isn't in the SlotHashes sysvar.
    SlotHashUnavailable = 4,
    /// The draw isn't pending.
    NotPending = 5,
    /// The draw hasn't timed out.
    NotTimedOut = 6,
    /// An account isn't the one expected.
    WrongAccount = 7,
    /// The signer isn't the feed's authority, or the draw's requester.
    Unauthorized = 8,
    /// Draws were still waiting for their secrets (the old `recommit`; kept so codes stay stable).
    #[allow(dead_code)]
    DrawsPending = 9,
    /// A setting is out of range.
    BadSetting = 10,
    /// The feed's fee is more than the request said it would pay.
    FeeTooHigh = 11,
    /// The committed chain's secrets are all assigned: the operator must commit a new chain.
    ChainExhausted = 12,
    /// The feed has no fallback (`fallback_delay_slots` is 0): a withheld draw can only be expired.
    FallbackDisabled = 13,
    /// The fallback window has closed: the draw can only be expired (refund).
    FallbackClosed = 14,
    /// The draw's secret hasn't been revealed yet: closing it would leave the chain without the account
    /// its reveal needs (the internal review, 14 Oct 2026).
    NotRevealedYet = 15,
    /// A next chain is already committed and a draw is assigned to it, so it can't be replaced.
    NextChainPending = 16,
    /// The request expected another kind of fee than the feed charges.
    FeeKindChanged = 17,
}

/// The least fallback delay a feed may set: two leader windows (4 slots each), so the fallback slot
/// is never produced by the leader of the reveal window's last slot (the internal review, 14 Oct 2026).
pub const MIN_FALLBACK_DELAY_SLOTS: u32 = 8;

/// The two token programs a token fee may use (the same TransferChecked and BurnChecked layouts).
const SPL_TOKEN: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022: Pubkey = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

impl From<JumpballError> for ProgramError {
    fn from(e: JumpballError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    let (tag, data) = input
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match tag {
        0 => init_feed(program_id, accounts, data),
        1 => request(program_id, accounts, data),
        2 => reveal(program_id, accounts, data),
        3 => expire(program_id, accounts),
        4 => close_draw(program_id, accounts),
        5 => commit_next(program_id, accounts, data),
        6 => set_fee(program_id, accounts, data),
        7 => force_settle(program_id, accounts),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn arr32(d: &[u8], at: usize) -> Result<[u8; 32], ProgramError> {
    d.get(at..at + 32)
        .and_then(|s| s.try_into().ok())
        .ok_or(ProgramError::InvalidInstructionData)
}
fn u64_at(d: &[u8], at: usize) -> Result<u64, ProgramError> {
    Ok(u64::from_le_bytes(
        d.get(at..at + 8)
            .and_then(|s| s.try_into().ok())
            .ok_or(ProgramError::InvalidInstructionData)?,
    ))
}
fn u32_at(d: &[u8], at: usize) -> Result<u32, ProgramError> {
    Ok(u32::from_le_bytes(
        d.get(at..at + 4)
            .and_then(|s| s.try_into().ok())
            .ok_or(ProgramError::InvalidInstructionData)?,
    ))
}

fn load_feed(program_id: &Pubkey, acc: &AccountInfo) -> Result<Feed, ProgramError> {
    if acc.owner != program_id {
        return Err(JumpballError::WrongAccount.into());
    }
    Feed::read(&acc.try_borrow_data()?)
}
fn load_draw(program_id: &Pubkey, acc: &AccountInfo, feed: &Pubkey) -> Result<Draw, ProgramError> {
    if acc.owner != program_id {
        return Err(JumpballError::WrongAccount.into());
    }
    let d = Draw::read(&acc.try_borrow_data()?)?;
    if d.feed != *feed {
        return Err(JumpballError::WrongAccount.into());
    }
    Ok(d)
}

fn init_feed(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let payer = next_account_info(it)?;
    let authority = next_account_info(it)?;
    let feed_acc = next_account_info(it)?;
    let system = next_account_info(it)?;
    if !payer.is_signer || !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system.key != system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let id = u64_at(data, 0)?;
    let head = arr32(data, 8)?;
    let delay_slots = u32_at(data, 40)?;
    let timeout_slots = u32_at(data, 44)?;
    // How many secrets the chain holds (links after the head).
    let chain_length = u64_at(data, 48)?;
    // Slots after the reveal window before the fallback slot (0 = no fallback).
    let fallback_delay_slots = u32_at(data, 56)?;
    // Who commits later chains (never the operator), and the only requester served (zero = anyone).
    let commit_authority = Pubkey::new_from_array(arr32(data, 60)?);
    let allowed_requester = Pubkey::new_from_array(arr32(data, 92)?);
    if chain_length == 0
        || commit_authority == Pubkey::default()
        || commit_authority == *authority.key
        || (fallback_delay_slots != 0 && fallback_delay_slots < MIN_FALLBACK_DELAY_SLOTS)
    {
        return Err(JumpballError::BadSetting.into());
    }
    // At least one slot of delay (the lock slot's own hash is known to its leader), and a reveal
    // window (and fallback, if any) that stays inside what SlotHashes remembers.
    if !slot_settings_fit(
        delay_slots,
        timeout_slots,
        fallback_delay_slots,
        MAX_TIMEOUT_SLOTS,
    ) {
        return Err(JumpballError::BadSetting.into());
    }
    let id_bytes = id.to_le_bytes();
    let (key, bump) =
        Pubkey::find_program_address(&[FEED_SEED, authority.key.as_ref(), &id_bytes], program_id);
    if *feed_acc.key != key {
        return Err(JumpballError::WrongAccount.into());
    }
    create_pda(
        payer,
        feed_acc,
        system,
        program_id,
        FEED_LEN,
        &[FEED_SEED, authority.key.as_ref(), &id_bytes, &[bump]],
    )?;
    Feed {
        bump,
        authority: *authority.key,
        head,
        id,
        assigned: 0,
        revealed: 0,
        failures: 0,
        delay_slots,
        timeout_slots,
        // Free until the authority sets a fee.
        fee_kind: FeeKind::Sol,
        fee_account: Pubkey::default(),
        fee_amount: 0,
        burn_bps: 0,
        chain_end: chain_length,
        fallback_delay_slots,
        forced: 0,
        commit_authority,
        next_head: [0; 32],
        next_length: 0,
        allowed_requester,
    }
    .write(&mut feed_acc.try_borrow_mut_data()?)?;
    msg!("jumpball: feed {} committed", id);
    Ok(())
}

fn request(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let payer = next_account_info(it)?;
    let requester = next_account_info(it)?;
    let feed_acc = next_account_info(it)?;
    let draw_acc = next_account_info(it)?;
    let system = next_account_info(it)?;
    if !payer.is_signer || !requester.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system.key != system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let nonce = arr32(data, 0)?;
    // The most the requester will pay (optional; none means 0), so a fee raised after the request
    // was built can't charge more than it agreed to.
    let max_fee = if data.len() >= 40 {
        u64_at(data, 32)?
    } else {
        0
    };
    let mut feed = load_feed(program_id, feed_acc)?;
    if feed.allowed_requester != Pubkey::default() && feed.allowed_requester != *requester.key {
        return Err(JumpballError::Unauthorized.into());
    }
    if feed.fee_amount > max_fee {
        return Err(JumpballError::FeeTooHigh.into());
    }
    // The fee's kind, when the request names it: a kind switched after it was built can't charge it
    // in another unit.
    if let Some(kind) = data.get(40) {
        if *kind != feed.fee_kind as u8 {
            return Err(JumpballError::FeeKindChanged.into());
        }
    }
    // Never assign a secret past the committed chains (this one, and the next if it's committed).
    let capacity = feed
        .chain_end
        .checked_add(feed.next_length)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if feed.assigned >= capacity {
        return Err(JumpballError::ChainExhausted.into());
    }
    let (key, bump) = Pubkey::find_program_address(
        &[
            DRAW_SEED,
            feed_acc.key.as_ref(),
            requester.key.as_ref(),
            &nonce,
        ],
        program_id,
    );
    if *draw_acc.key != key {
        return Err(JumpballError::WrongAccount.into());
    }
    create_pda(
        payer,
        draw_acc,
        system,
        program_id,
        DRAW_LEN,
        &[
            DRAW_SEED,
            feed_acc.key.as_ref(),
            requester.key.as_ref(),
            &nonce,
            &[bump],
        ],
    )?;
    let now = Clock::get()?.slot;
    feed.assigned = feed
        .assigned
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let draw = Draw {
        bump,
        status: DrawStatus::Pending,
        feed: *feed_acc.key,
        requester: *requester.key,
        payer: *payer.key,
        nonce,
        secret_index: feed.assigned,
        lock_slot: now,
        target_slot: now + feed.delay_slots as u64,
        result: [0; 32],
        fallback_slot: 0,
    };
    draw.write(&mut draw_acc.try_borrow_mut_data()?)?;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    if feed.fee_amount > 0 {
        pay_fee(&feed, payer, system, draw_acc.key, it)?;
    }
    msg!(
        "jumpball: draw {} locked at slot {}, target {}",
        draw.secret_index,
        now,
        draw.target_slot
    );
    Ok(())
}

/// Takes the feed's fee from the payer. SOL: [fee wallet]. Token: [payer's token account, mint, fee
/// token account, token program]; `burn_bps` of it is burned from the payer, the rest transferred.
fn pay_fee<'a, 'b>(
    feed: &Feed,
    payer: &'b AccountInfo<'a>,
    system: &'b AccountInfo<'a>,
    draw: &Pubkey,
    it: &mut core::slice::Iter<'b, AccountInfo<'a>>,
) -> ProgramResult {
    let amount = feed.fee_amount;
    let mut burned = 0u64;
    match feed.fee_kind {
        FeeKind::Sol => {
            let wallet = next_account_info(it)?;
            if *wallet.key != feed.fee_account {
                return Err(JumpballError::WrongAccount.into());
            }
            invoke(
                &system_instruction::transfer(payer.key, wallet.key, amount),
                &[payer.clone(), wallet.clone(), system.clone()],
            )?;
        }
        FeeKind::Token => {
            let source = next_account_info(it)?;
            let mint = next_account_info(it)?;
            let dest = next_account_info(it)?;
            let token_program = next_account_info(it)?;
            let tp = *token_program.key;
            if tp != SPL_TOKEN && tp != TOKEN_2022 {
                return Err(ProgramError::IncorrectProgramId);
            }
            if *dest.key != feed.fee_account || *dest.owner != tp || *mint.owner != tp {
                return Err(JumpballError::WrongAccount.into());
            }
            // The fee account's mint (a token account starts with it) must be the mint passed.
            if dest.try_borrow_data()?.get(0..32) != Some(mint.key.as_ref()) {
                return Err(JumpballError::WrongAccount.into());
            }
            let decimals = *mint
                .try_borrow_data()?
                .get(44)
                .ok_or(ProgramError::InvalidAccountData)?;
            burned = ((amount as u128 * feed.burn_bps as u128) / 10_000) as u64;
            let rest = amount - burned;
            let ix = |tag: u8, value: u64, accounts: Vec<AccountMeta>| Instruction {
                program_id: tp,
                accounts,
                data: [&[tag][..], &value.to_le_bytes(), &[decimals]].concat(),
            };
            if burned > 0 {
                // BurnChecked: account, mint, owner.
                invoke(
                    &ix(
                        15,
                        burned,
                        vec![
                            AccountMeta::new(*source.key, false),
                            AccountMeta::new(*mint.key, false),
                            AccountMeta::new_readonly(*payer.key, true),
                        ],
                    ),
                    &[
                        source.clone(),
                        mint.clone(),
                        payer.clone(),
                        token_program.clone(),
                    ],
                )?;
            }
            if rest > 0 {
                // TransferChecked: source, mint, destination, owner.
                invoke(
                    &ix(
                        12,
                        rest,
                        vec![
                            AccountMeta::new(*source.key, false),
                            AccountMeta::new_readonly(*mint.key, false),
                            AccountMeta::new(*dest.key, false),
                            AccountMeta::new_readonly(*payer.key, true),
                        ],
                    ),
                    &[
                        source.clone(),
                        mint.clone(),
                        dest.clone(),
                        payer.clone(),
                        token_program.clone(),
                    ],
                )?;
            }
        }
    }
    sol_log_data(&[
        b"jumpball",
        b"fee",
        draw.as_ref(),
        &[feed.fee_kind as u8],
        &amount.to_le_bytes(),
        &burned.to_le_bytes(),
    ]);
    Ok(())
}

/// The feed authority sets the fee: kind u8 (0 SOL, 1 token) · amount u64 · burn_bps u16 ·
/// fee_account [32]. Amount 0 makes the feed free. SOL can't be burned, so SOL fees burn nothing.
fn set_fee(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let authority = next_account_info(it)?;
    let feed_acc = next_account_info(it)?;
    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let mut feed = load_feed(program_id, feed_acc)?;
    if feed.authority != *authority.key {
        return Err(JumpballError::Unauthorized.into());
    }
    let kind = match data.first() {
        Some(0) => FeeKind::Sol,
        Some(1) => FeeKind::Token,
        _ => return Err(JumpballError::BadSetting.into()),
    };
    let amount = u64_at(data, 1)?;
    let burn_bps = u16::from_le_bytes(
        data.get(9..11)
            .and_then(|s| s.try_into().ok())
            .ok_or(ProgramError::InvalidInstructionData)?,
    );
    let account = Pubkey::new_from_array(arr32(data, 11)?);
    if burn_bps > 10_000 || (kind == FeeKind::Sol && burn_bps > 0) {
        return Err(JumpballError::BadSetting.into());
    }
    if amount > 0 && account == Pubkey::default() {
        return Err(JumpballError::BadSetting.into());
    }
    feed.fee_kind = kind;
    feed.fee_amount = amount;
    feed.burn_bps = burn_bps;
    feed.fee_account = account;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    msg!(
        "jumpball: fee set to {} ({:?}, {} bps burned)",
        amount,
        kind,
        burn_bps
    );
    Ok(())
}

fn reveal(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let authority = next_account_info(it)?;
    let feed_acc = next_account_info(it)?;
    let draw_acc = next_account_info(it)?;
    let slot_hashes = next_account_info(it)?;
    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *slot_hashes.key != sysvar::slot_hashes::ID {
        return Err(JumpballError::WrongAccount.into());
    }
    let secret = arr32(data, 0)?;
    let mut feed = load_feed(program_id, feed_acc)?;
    if feed.authority != *authority.key {
        return Err(JumpballError::Unauthorized.into());
    }
    let mut draw = load_draw(program_id, draw_acc, feed_acc.key)?;
    // Past the end of this chain, the next (committed ahead) takes over: its first secret hashes to
    // its head.
    if feed.revealed == feed.chain_end && feed.next_length > 0 {
        feed.head = feed.next_head;
        feed.chain_end = feed
            .chain_end
            .checked_add(feed.next_length)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        feed.next_head = [0; 32];
        feed.next_length = 0;
    }
    if draw.secret_index != feed.revealed + 1 {
        return Err(JumpballError::OutOfOrder.into());
    }
    if !is_next_link(&secret, &feed.head) {
        return Err(JumpballError::NotNextLink.into());
    }
    let now = Clock::get()?.slot;
    match (
        draw.status,
        timing(now, draw.target_slot, feed.timeout_slots),
    ) {
        (_, Timing::TooEarly) => return Err(JumpballError::TooEarly.into()),
        (DrawStatus::Pending, Timing::Open) => {
            let (hash_slot, slot_hash) =
                first_slot_hash_from(&slot_hashes.try_borrow_data()?, draw.target_slot)
                    .ok_or(JumpballError::SlotHashUnavailable)?;
            draw.result = draw_result(&secret, &slot_hash, draw_acc.key);
            draw.status = DrawStatus::Fulfilled;
            // Everything needed to re-check the draw from the logs alone, after the account is closed:
            // H(secret) is the previous link, result = H("jumpball" ‖ secret ‖ slot_hash ‖ draw).
            sol_log_data(&[
                b"jumpball",
                b"reveal",
                draw_acc.key.as_ref(),
                &draw.secret_index.to_le_bytes(),
                &secret,
                &hash_slot.to_le_bytes(),
                &slot_hash,
                &draw.result,
            ]);
        }
        // Too late, no fallback: the draw fails (the consumer refunds), and the secret still advances
        // the chain so later draws can be revealed.
        (DrawStatus::Pending, Timing::TimedOut) if feed.fallback_delay_slots == 0 => {
            draw.status = DrawStatus::Failed;
            feed.failures += 1;
        }
        // Too late on a feed with a fallback: the draw stays pending for `force_settle` (a late
        // reveal must not fail it, or the operator could turn a withheld draw into a refund). The
        // secret only advances the chain.
        (DrawStatus::Pending, Timing::TimedOut) => {}
        // Already forced or failed: the reveal never changes the result, it only advances the chain.
        (DrawStatus::Forced, _) | (DrawStatus::Failed, _) => {}
        (DrawStatus::Fulfilled, _) => return Err(JumpballError::NotPending.into()),
    }
    feed.head = secret;
    feed.revealed += 1;
    draw.write(&mut draw_acc.try_borrow_mut_data()?)?;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    msg!("jumpball: draw {} {:?}", draw.secret_index, draw.status);
    Ok(())
}

/// Anyone may fail a draw whose reveal window has closed (and, on a feed with a fallback, whose
/// fallback window has closed too), so a consumer can refund without waiting on the operator.
fn expire(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let feed_acc = next_account_info(it)?;
    let draw_acc = next_account_info(it)?;
    let mut feed = load_feed(program_id, feed_acc)?;
    let mut draw = load_draw(program_id, draw_acc, feed_acc.key)?;
    if draw.status != DrawStatus::Pending {
        return Err(JumpballError::NotPending.into());
    }
    let now = Clock::get()?.slot;
    let timed_out = if feed.fallback_delay_slots == 0 {
        timing(now, draw.target_slot, feed.timeout_slots) == Timing::TimedOut
    } else {
        force_timing(
            now,
            draw.target_slot,
            feed.timeout_slots,
            feed.fallback_delay_slots,
        ) == ForceTiming::Closed
    };
    if !timed_out {
        return Err(JumpballError::NotTimedOut.into());
    }
    draw.status = DrawStatus::Failed;
    feed.failures += 1;
    draw.write(&mut draw_acc.try_borrow_mut_data()?)?;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    msg!(
        "jumpball: draw {} failed (no reveal in time)",
        draw.secret_index
    );
    Ok(())
}

/// Anyone settles a withheld draw from slot hashes alone, once its reveal window has closed and the
/// fallback slot has passed: result = H("jumpball:forced" ‖ target slot hash ‖ fallback slot hash ‖
/// draw). See `logic::forced_result` for why this can't be steered, and what it does allow (one
/// re-roll per withheld draw, publicly counted).
fn force_settle(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let caller = next_account_info(it)?;
    let feed_acc = next_account_info(it)?;
    let draw_acc = next_account_info(it)?;
    let slot_hashes = next_account_info(it)?;
    if !caller.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *slot_hashes.key != sysvar::slot_hashes::ID {
        return Err(JumpballError::WrongAccount.into());
    }
    let mut feed = load_feed(program_id, feed_acc)?;
    let mut draw = load_draw(program_id, draw_acc, feed_acc.key)?;
    if draw.status != DrawStatus::Pending {
        return Err(JumpballError::NotPending.into());
    }
    if feed.fallback_delay_slots == 0 {
        return Err(JumpballError::FallbackDisabled.into());
    }
    match force_timing(
        Clock::get()?.slot,
        draw.target_slot,
        feed.timeout_slots,
        feed.fallback_delay_slots,
    ) {
        ForceTiming::TooEarly => return Err(JumpballError::TooEarly.into()),
        ForceTiming::Closed => return Err(JumpballError::FallbackClosed.into()),
        ForceTiming::Open => {}
    }
    let data = slot_hashes.try_borrow_data()?;
    // The first produced slot at or after each: a skipped slot rolls on to the next one.
    let (fallback_used, fallback_hash) = first_slot_hash_from(
        &data,
        fallback_slot(
            draw.target_slot,
            feed.timeout_slots,
            feed.fallback_delay_slots,
        ),
    )
    .ok_or(JumpballError::SlotHashUnavailable)?;
    // Still in the sysvar: the feed's settings are capped so the whole fallback window fits.
    let (target_used, target_hash) =
        first_slot_hash_from(&data, draw.target_slot).ok_or(JumpballError::SlotHashUnavailable)?;
    drop(data);
    draw.result = forced_result(&target_hash, &fallback_hash, draw_acc.key);
    draw.status = DrawStatus::Forced;
    draw.fallback_slot = fallback_used;
    feed.forced = feed
        .forced
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    draw.write(&mut draw_acc.try_borrow_mut_data()?)?;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    // Everything needed to re-check the draw from the logs alone, like a reveal.
    sol_log_data(&[
        b"jumpball",
        b"forced",
        draw_acc.key.as_ref(),
        &draw.secret_index.to_le_bytes(),
        &target_used.to_le_bytes(),
        &target_hash,
        &fallback_used.to_le_bytes(),
        &fallback_hash,
        &draw.result,
    ]);
    msg!(
        "jumpball: draw {} forced (no reveal in time)",
        draw.secret_index
    );
    Ok(())
}

/// The requester closes a settled draw and its payer gets the rent back.
fn close_draw(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let requester = next_account_info(it)?;
    let draw_acc = next_account_info(it)?;
    let payer = next_account_info(it)?;
    let feed_acc = next_account_info(it)?;
    if !requester.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let feed = load_feed(program_id, feed_acc)?;
    let draw = load_draw(program_id, draw_acc, feed_acc.key)?;
    // Only once its secret is revealed (the internal review, 14 Oct 2026): reveals go in order and each
    // needs its draw's account, so closing a forced or failed draw before then would stop the chain for
    // good.
    if draw.secret_index > feed.revealed {
        return Err(JumpballError::NotRevealedYet.into());
    }
    if draw.requester != *requester.key {
        return Err(JumpballError::Unauthorized.into());
    }
    if draw.payer != *payer.key {
        return Err(JumpballError::WrongAccount.into());
    }
    if draw.status == DrawStatus::Pending {
        return Err(JumpballError::NotPending.into());
    }
    let lamports = draw_acc.lamports();
    **payer.try_borrow_mut_lamports()? = payer
        .lamports()
        .checked_add(lamports)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    **draw_acc.try_borrow_mut_lamports()? = 0;
    draw_acc.try_borrow_mut_data()?.fill(0);
    draw_acc.resize(0)?;
    draw_acc.assign(&system_program::ID);
    Ok(())
}

/// The commit authority (not the operator: the founder's hardware wallet, 14 Oct 2026) commits the next
/// chain ahead of time. It takes over at the first reveal past `chain_end`, so draws never wait for an
/// idle feed. It can be replaced only while no draw is assigned past `chain_end` (none uses its
/// secrets yet), so a wrong commit can be corrected (audit J-3, 15 Oct 2026); after that it's fixed.
fn commit_next(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let commit_authority = next_account_info(it)?;
    let feed_acc = next_account_info(it)?;
    if !commit_authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let mut feed = load_feed(program_id, feed_acc)?;
    if feed.commit_authority != *commit_authority.key {
        return Err(JumpballError::Unauthorized.into());
    }
    if feed.next_length > 0 && feed.assigned > feed.chain_end {
        return Err(JumpballError::NextChainPending.into());
    }
    let chain_length = u64_at(data, 32)?;
    if chain_length == 0 {
        return Err(JumpballError::BadSetting.into());
    }
    feed.next_head = arr32(data, 0)?;
    feed.next_length = chain_length;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    msg!(
        "jumpball: feed {} next chain committed ({} secrets after {}; replaceable until a draw is assigned to it)",
        feed.id,
        chain_length,
        feed.chain_end
    );
    Ok(())
}

/// Creates a program-owned account at a PDA, even if someone sent it lamports first (a plain
/// create would then fail): tops up rent, allocates and assigns instead.
fn create_pda<'a>(
    payer: &AccountInfo<'a>,
    target: &AccountInfo<'a>,
    system: &AccountInfo<'a>,
    owner: &Pubkey,
    space: usize,
    seeds: &[&[u8]],
) -> ProgramResult {
    if target.owner == owner {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let need = Rent::get()?.minimum_balance(space);
    let have = target.lamports();
    if have == 0 {
        return invoke_signed(
            &system_instruction::create_account(payer.key, target.key, need, space as u64, owner),
            &[payer.clone(), target.clone(), system.clone()],
            &[seeds],
        );
    }
    if have < need {
        invoke(
            &system_instruction::transfer(payer.key, target.key, need - have),
            &[payer.clone(), target.clone(), system.clone()],
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(target.key, space as u64),
        &[target.clone(), system.clone()],
        &[seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(target.key, owner),
        &[target.clone(), system.clone()],
        &[seeds],
    )
}
