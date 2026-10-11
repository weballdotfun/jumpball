//! Jumpball: provably fair randomness for Solana programs.
//!
//! An operator commits to a hash chain (a **feed**). A consuming program **requests** a draw, which
//! locks at that slot and is assigned the feed's next secret. Once a slot after the lock has a
//! hash, the operator **reveals** that secret: Jumpball checks it is the chain's next link and
//! mixes it with the slot hash for the result. A draw not revealed in time **fails**, so the
//! consumer can refund. See README.md for why nobody can steer a result.
//!
//! Instructions (first byte):
//! 0 `init_feed`  id u64 · head [32] · delay_slots u32 · timeout_slots u32
//!   accounts: payer (signer, writable) · authority (signer) · feed (writable) · system program
//! 1 `request`    nonce [32]
//!   accounts: payer (signer, writable) · requester (signer) · feed (writable) · draw (writable) · system program
//! 2 `reveal`     secret [32]
//!   accounts: authority (signer) · feed (writable) · draw (writable) · SlotHashes sysvar
//! 3 `expire`     (none)
//!   accounts: feed (writable) · draw (writable)
//! 4 `close_draw` (none)
//!   accounts: requester (signer) · draw (writable) · payer (writable, the draw's payer)
//! 5 `recommit`   head [32]
//!   accounts: authority (signer) · feed (writable)

pub mod logic;
pub mod state;

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    log::sol_log_data,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::{self, Sysvar},
};
use solana_system_interface::{instruction as system_instruction, program as system_program};

use crate::logic::{draw_result, first_slot_hash_from, is_next_link, timing, Timing};
use crate::state::{Draw, DrawStatus, Feed, DRAW_LEN, DRAW_SEED, FEED_LEN, FEED_SEED};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

/// Longest wait for a reveal a feed may set: the SlotHashes sysvar keeps the last 512 slots, so a
/// later reveal could no longer find its slot's hash.
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
    /// Draws are still waiting for their secrets: a new chain can't be committed yet.
    DrawsPending = 9,
    /// A setting is out of range.
    BadSetting = 10,
}

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
        5 => recommit(program_id, accounts, data),
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
    // At least one slot of delay (the lock slot's own hash is known to its leader), and a reveal
    // window that stays inside what SlotHashes remembers.
    if delay_slots == 0
        || timeout_slots == 0
        || delay_slots.saturating_add(timeout_slots) > MAX_TIMEOUT_SLOTS
    {
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
    let mut feed = load_feed(program_id, feed_acc)?;
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
    };
    draw.write(&mut draw_acc.try_borrow_mut_data()?)?;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    msg!(
        "jumpball: draw {} locked at slot {}, target {}",
        draw.secret_index,
        now,
        draw.target_slot
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
        // Too late: the draw fails (the consumer refunds), and the secret still advances the chain
        // so later draws can be revealed.
        (DrawStatus::Pending, Timing::TimedOut) => {
            draw.status = DrawStatus::Failed;
            feed.failures += 1;
        }
        // Already failed by `expire`: the reveal only advances the chain.
        (DrawStatus::Failed, _) => {}
        (DrawStatus::Fulfilled, _) => return Err(JumpballError::NotPending.into()),
    }
    feed.head = secret;
    feed.revealed += 1;
    draw.write(&mut draw_acc.try_borrow_mut_data()?)?;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    msg!("jumpball: draw {} {:?}", draw.secret_index, draw.status);
    Ok(())
}

/// Anyone may fail a draw whose reveal window has closed, so a consumer can refund without waiting
/// on the operator.
fn expire(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let feed_acc = next_account_info(it)?;
    let draw_acc = next_account_info(it)?;
    let mut feed = load_feed(program_id, feed_acc)?;
    let mut draw = load_draw(program_id, draw_acc, feed_acc.key)?;
    if draw.status != DrawStatus::Pending {
        return Err(JumpballError::NotPending.into());
    }
    if timing(Clock::get()?.slot, draw.target_slot, feed.timeout_slots) != Timing::TimedOut {
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

/// The requester closes a settled draw and its payer gets the rent back.
fn close_draw(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let requester = next_account_info(it)?;
    let draw_acc = next_account_info(it)?;
    let payer = next_account_info(it)?;
    if !requester.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if draw_acc.owner != program_id {
        return Err(JumpballError::WrongAccount.into());
    }
    let draw = Draw::read(&draw_acc.try_borrow_data()?)?;
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

/// A new chain once every assigned secret has been revealed (a chain is finite).
fn recommit(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
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
    if feed.revealed != feed.assigned {
        return Err(JumpballError::DrawsPending.into());
    }
    feed.head = arr32(data, 0)?;
    feed.write(&mut feed_acc.try_borrow_mut_data()?)?;
    msg!(
        "jumpball: feed {} recommitted at draw {}",
        feed.id,
        feed.revealed
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
