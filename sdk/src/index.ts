// Jumpball TypeScript client: addresses, instructions, account readers and the hash chain.
// Mirrors programs/jumpball (instruction tags and account layouts).
import { createHash, randomBytes } from 'node:crypto';
import { PublicKey, SystemProgram, SYSVAR_SLOT_HASHES_PUBKEY, TransactionInstruction } from '@solana/web3.js';

export const FEED_SEED = Buffer.from('feed');
export const DRAW_SEED = Buffer.from('draw');

export enum DrawStatus {
  Pending = 0,
  Fulfilled = 1,
  Failed = 2,
  /** Settled by `force_settle` from slot hashes after a missed reveal. Settles like Fulfilled. */
  Forced = 3,
}

/** A settled draw (revealed or forced): the consumer pays out on its result. Failed means refund. */
export const isSettled = (s: DrawStatus) => s === DrawStatus.Fulfilled || s === DrawStatus.Forced;

export interface Feed {
  bump: number;
  authority: PublicKey;
  head: Buffer;
  id: bigint;
  assigned: bigint;
  revealed: bigint;
  failures: bigint;
  delaySlots: number;
  timeoutSlots: number;
  /** Per request: 0 = free. SOL goes to `feeAccount` (a wallet); a token fee to `feeAccount` (a token
   * account of the fee mint), with `burnBps` of it burned from the payer. */
  fee: { kind: 'sol' | 'token'; account: PublicKey; amount: bigint; burnBps: number };
  /** The last secret index the current chain covers (requests stop there unless a next chain is committed). */
  chainEnd: bigint;
  /** Slots after the reveal window before the fallback slot (0 = no fallback: a missed reveal fails). */
  fallbackDelaySlots: number;
  /** Draws settled by the fallback instead of a reveal (each one is a missed reveal). */
  forced: bigint;
  /** Who may commit the next chain (never the operator). */
  commitAuthority: PublicKey;
  /** The next chain, committed ahead (length 0n = none); it takes over past `chainEnd`. */
  next: { head: Buffer; length: bigint };
  /** The only requester served; PublicKey.default = anyone. */
  allowedRequester: PublicKey;
}

/** The accounts a fee needs, appended to a request: SOL → the fee wallet; token → the payer's token
 * account, the mint, the fee token account and the token program. */
export type FeeAccounts =
  | { kind: 'sol'; wallet: PublicKey }
  | { kind: 'token'; payerToken: PublicKey; mint: PublicKey; feeToken: PublicKey; tokenProgram: PublicKey };

export interface Draw {
  bump: number;
  status: DrawStatus;
  feed: PublicKey;
  requester: PublicKey;
  payer: PublicKey;
  nonce: Buffer;
  secretIndex: bigint;
  lockSlot: bigint;
  targetSlot: bigint;
  result: Buffer;
  /** For a forced draw, the produced slot whose hash settled it; 0n otherwise. */
  fallbackSlot: bigint;
}

const u64 = (n: bigint) => {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(n);
  return b;
};
const u32 = (n: number) => {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n);
  return b;
};
const sha256 = (b: Uint8Array) => createHash('sha256').update(b).digest();

export function feedAddress(program: PublicKey, authority: PublicKey, id: bigint): PublicKey {
  return PublicKey.findProgramAddressSync([FEED_SEED, authority.toBuffer(), u64(id)], program)[0];
}

export function drawAddress(program: PublicKey, feed: PublicKey, requester: PublicKey, nonce: Uint8Array): PublicKey {
  return PublicKey.findProgramAddressSync([DRAW_SEED, feed.toBuffer(), requester.toBuffer(), Buffer.from(nonce)], program)[0];
}

/** A u64 as the 32-byte nonce a consumer uses for, say, its round number. */
export function nonceFromU64(n: bigint): Buffer {
  return Buffer.concat([u64(n), Buffer.alloc(24)]);
}

const ix = (program: PublicKey, data: Buffer, keys: [PublicKey, boolean, boolean][]) =>
  new TransactionInstruction({
    programId: program,
    data,
    keys: keys.map(([pubkey, isSigner, isWritable]) => ({ pubkey, isSigner, isWritable })),
  });

export function initFeedIx(a: {
  program: PublicKey;
  payer: PublicKey;
  authority: PublicKey;
  id: bigint;
  head: Uint8Array;
  delaySlots: number;
  timeoutSlots: number;
  /** How many secrets the chain holds (`hashChain(seed, n)` → n). */
  chainLength: bigint;
  /** Slots after the reveal window before the fallback slot; 0 (the default) = no fallback, else at least 8. */
  fallbackDelaySlots?: number;
  /** Who may commit later chains (`commitNextIx`): not the authority (e.g. a hardware wallet). */
  commitAuthority: PublicKey;
  /** The only requester this feed serves; omitted = anyone. */
  allowedRequester?: PublicKey;
}): TransactionInstruction {
  const feed = feedAddress(a.program, a.authority, a.id);
  const data = Buffer.concat([
    Buffer.from([0]),
    u64(a.id),
    Buffer.from(a.head),
    u32(a.delaySlots),
    u32(a.timeoutSlots),
    u64(a.chainLength),
    u32(a.fallbackDelaySlots ?? 0),
    a.commitAuthority.toBuffer(),
    (a.allowedRequester ?? PublicKey.default).toBuffer(),
  ]);
  return ix(a.program, data, [
    [a.payer, true, true],
    [a.authority, true, false],
    [feed, false, true],
    [SystemProgram.programId, false, false],
  ]);
}

/** A request. On a feed with a fee, pass `maxFee` (the most the payer agrees to) and the fee accounts. */
export function requestIx(a: {
  program: PublicKey;
  payer: PublicKey;
  requester: PublicKey;
  feed: PublicKey;
  nonce: Uint8Array;
  maxFee?: bigint;
  fee?: FeeAccounts;
  /**
   * The fee kind this request expects: refused if the feed switched kind since. With `maxFee` it
   * defaults to `fee.kind`, so a paid request always binds the kind it read; one of the two is required.
   */
  feeKind?: 'sol' | 'token';
}): TransactionInstruction {
  const feeKind = a.feeKind ?? a.fee?.kind;
  if (a.maxFee !== undefined && !feeKind) throw new Error('requestIx: maxFee needs feeKind or fee (the fee kind the payer agreed to)');
  const draw = drawAddress(a.program, a.feed, a.requester, a.nonce);
  const keys: [PublicKey, boolean, boolean][] = [
    [a.payer, true, true],
    [a.requester, true, false],
    [a.feed, false, true],
    [draw, false, true],
    [SystemProgram.programId, false, false],
  ];
  if (a.fee?.kind === 'sol') keys.push([a.fee.wallet, false, true]);
  if (a.fee?.kind === 'token')
    keys.push([a.fee.payerToken, false, true], [a.fee.mint, false, true], [a.fee.feeToken, false, true], [a.fee.tokenProgram, false, false]);
  const data = [Buffer.from([1]), Buffer.from(a.nonce)];
  if (a.maxFee !== undefined) data.push(u64(a.maxFee));
  if (a.maxFee !== undefined) data.push(Buffer.from([feeKind === 'sol' ? 0 : 1]));
  return ix(a.program, Buffer.concat(data), keys);
}

/** The feed authority sets the fee (amount 0 = free; SOL fees can't burn). */
export function setFeeIx(a: {
  program: PublicKey;
  authority: PublicKey;
  feed: PublicKey;
  kind: 'sol' | 'token';
  amount: bigint;
  burnBps: number;
  account: PublicKey;
}): TransactionInstruction {
  const bps = Buffer.alloc(2);
  bps.writeUInt16LE(a.burnBps);
  return ix(a.program, Buffer.concat([Buffer.from([6, a.kind === 'sol' ? 0 : 1]), u64(a.amount), bps, a.account.toBuffer()]), [
    [a.authority, true, false],
    [a.feed, false, true],
  ]);
}

export function revealIx(a: { program: PublicKey; authority: PublicKey; feed: PublicKey; draw: PublicKey; secret: Uint8Array }): TransactionInstruction {
  return ix(a.program, Buffer.concat([Buffer.from([2]), Buffer.from(a.secret)]), [
    [a.authority, true, false],
    [a.feed, false, true],
    [a.draw, false, true],
    [SYSVAR_SLOT_HASHES_PUBKEY, false, false],
  ]);
}

export function expireIx(a: { program: PublicKey; feed: PublicKey; draw: PublicKey }): TransactionInstruction {
  return ix(a.program, Buffer.from([3]), [
    [a.feed, false, true],
    [a.draw, false, true],
  ]);
}

/** Anyone settles a withheld draw from slot hashes, once the fallback slot has passed. */
export function forceSettleIx(a: { program: PublicKey; caller: PublicKey; feed: PublicKey; draw: PublicKey }): TransactionInstruction {
  return ix(a.program, Buffer.from([7]), [
    [a.caller, true, false],
    [a.feed, false, true],
    [a.draw, false, true],
    [SYSVAR_SLOT_HASHES_PUBKEY, false, false],
  ]);
}

/** The slot whose hash (or the next produced one's) settles a withheld draw. */
export const fallbackSlot = (targetSlot: bigint, timeoutSlots: number, fallbackDelaySlots: number) =>
  targetSlot + BigInt(timeoutSlots) + BigInt(fallbackDelaySlots);

/** The requester closes a draw once its secret has been revealed; its payer gets the rent back. */
export function closeDrawIx(a: { program: PublicKey; requester: PublicKey; draw: PublicKey; payer: PublicKey; feed: PublicKey }): TransactionInstruction {
  return ix(a.program, Buffer.from([4]), [
    [a.requester, true, false],
    [a.draw, false, true],
    [a.payer, false, true],
    [a.feed, false, false],
  ]);
}

/** The commit authority commits the next chain ahead of time; it takes over past the current chain's end. */
export function commitNextIx(a: {
  program: PublicKey;
  commitAuthority: PublicKey;
  feed: PublicKey;
  head: Uint8Array;
  chainLength: bigint;
}): TransactionInstruction {
  return ix(a.program, Buffer.concat([Buffer.from([5]), Buffer.from(a.head), u64(a.chainLength)]), [
    [a.commitAuthority, true, false],
    [a.feed, false, true],
  ]);
}

export function readFeed(data: Buffer): Feed {
  if (data.length !== 304 || data.subarray(0, 8).toString() !== 'JBFEED03') throw new Error('not a Jumpball feed');
  return {
    bump: data[8]!,
    authority: new PublicKey(data.subarray(16, 48)),
    head: Buffer.from(data.subarray(48, 80)),
    id: data.readBigUInt64LE(80),
    assigned: data.readBigUInt64LE(88),
    revealed: data.readBigUInt64LE(96),
    failures: data.readBigUInt64LE(104),
    delaySlots: data.readUInt32LE(112),
    timeoutSlots: data.readUInt32LE(116),
    chainEnd: data.readBigUInt64LE(168),
    fallbackDelaySlots: data.readUInt32LE(176),
    forced: data.readBigUInt64LE(184),
    commitAuthority: new PublicKey(data.subarray(192, 224)),
    next: { head: Buffer.from(data.subarray(224, 256)), length: data.readBigUInt64LE(256) },
    allowedRequester: new PublicKey(data.subarray(264, 296)),
    fee: {
      kind: data[162] === 1 ? 'token' : 'sol',
      account: new PublicKey(data.subarray(120, 152)),
      amount: data.readBigUInt64LE(152),
      burnBps: data.readUInt16LE(160),
    },
  };
}

export function readDraw(data: Buffer): Draw {
  if (data.length !== 232 || data.subarray(0, 8).toString() !== 'JBDRAW01') throw new Error('not a Jumpball draw');
  return {
    bump: data[8]!,
    status: data[9]! as DrawStatus,
    feed: new PublicKey(data.subarray(16, 48)),
    requester: new PublicKey(data.subarray(48, 80)),
    payer: new PublicKey(data.subarray(80, 112)),
    nonce: Buffer.from(data.subarray(112, 144)),
    secretIndex: data.readBigUInt64LE(144),
    lockSlot: data.readBigUInt64LE(152),
    targetSlot: data.readBigUInt64LE(160),
    result: Buffer.from(data.subarray(168, 200)),
    fallbackSlot: data.readBigUInt64LE(200),
  };
}

/**
 * A hash chain of `length` secrets from a 32-byte seed: `links[0]` is the commitment published in
 * the feed, and `links[i]` is the secret revealed for the feed's i-th draw. Keep the seed secret:
 * anyone with it can compute every future secret (though not a result before its slot hash exists).
 */
export function hashChain(seed: Uint8Array, length: number): Buffer[] {
  const links: Buffer[] = [Buffer.from(seed)];
  for (let i = 0; i < length; i++) links.push(sha256(links[links.length - 1]!));
  return links.reverse();
}

export const newSeed = () => randomBytes(32);

/**
 * The `Program data:` lines a given program itself wrote, following the runtime's invoke/success
 * lines. Any program can log a look-alike line, so a verifier should pass Jumpball's program id.
 */
export function programDataLines(logs: readonly string[], program?: PublicKey): string[] {
  const id = program?.toBase58();
  const stack: string[] = [];
  const out: string[] = [];
  for (const line of logs) {
    const invoke = /^Program (\S+) invoke \[\d+\]$/.exec(line);
    if (invoke) {
      stack.push(invoke[1]!);
      continue;
    }
    if (/^Program \S+ (success|failed)/.test(line)) {
      stack.pop();
      continue;
    }
    if (line.startsWith('Program data: ') && (!id || stack[stack.length - 1] === id)) out.push(line);
  }
  return out;
}

/** A fulfilled draw as its reveal logged it ("Program data: jumpball reveal …"). */
export interface RevealLog {
  draw: PublicKey;
  index: bigint;
  secret: Buffer;
  slot: bigint;
  slotHash: Buffer;
  result: Buffer;
}

/**
 * Reads the reveal event from a transaction's log lines, if it has one. Pass `program` (Jumpball's id)
 * to count only lines Jumpball itself wrote; without it, a transaction from anyone can spoof one.
 */
export function parseRevealLog(logs: readonly string[], program?: PublicKey): RevealLog | null {
  for (const line of programDataLines(logs, program)) {
    const f = line.slice(14).split(' ').map((p) => Buffer.from(p, 'base64'));
    if (f[0]?.toString() !== 'jumpball' || f[1]?.toString() !== 'reveal' || f.length !== 8) continue;
    return { draw: new PublicKey(f[2]!), index: f[3]!.readBigUInt64LE(0), secret: f[4]!, slot: f[5]!.readBigUInt64LE(0), slotHash: f[6]!, result: f[7]! };
  }
  return null;
}

/**
 * Re-checks a draw from its reveal log: the secret hashes to the link committed before it (the
 * previous reveal's secret, or the feed's head when it was the first), and the result is
 * H("jumpball" ‖ secret ‖ slot hash ‖ draw). The slot hash itself is the chain's (SlotHashes).
 */
export function checkReveal(r: RevealLog, previousLink: Uint8Array): { chainOk: boolean; resultOk: boolean } {
  return {
    chainOk: sha256(r.secret).equals(Buffer.from(previousLink)),
    resultOk: createHash('sha256').update('jumpball').update(r.secret).update(r.slotHash).update(r.draw.toBuffer()).digest().equals(r.result),
  };
}

/** A forced draw as force_settle logged it ("Program data: jumpball forced …"). */
export interface ForcedLog {
  draw: PublicKey;
  index: bigint;
  /** The produced slot used for the target (the draw's target slot, or the next one if skipped). */
  targetSlot: bigint;
  targetHash: Buffer;
  /** The produced slot used for the fallback. */
  fallbackSlot: bigint;
  fallbackHash: Buffer;
  result: Buffer;
}

/**
 * Reads the forced-settle event from a transaction's log lines, if it has one. Pass `program`
 * (Jumpball's id) to count only lines Jumpball itself wrote.
 */
export function parseForcedLog(logs: readonly string[], program?: PublicKey): ForcedLog | null {
  for (const line of programDataLines(logs, program)) {
    const f = line.slice(14).split(' ').map((p) => Buffer.from(p, 'base64'));
    if (f[0]?.toString() !== 'jumpball' || f[1]?.toString() !== 'forced' || f.length !== 9) continue;
    return {
      draw: new PublicKey(f[2]!),
      index: f[3]!.readBigUInt64LE(0),
      targetSlot: f[4]!.readBigUInt64LE(0),
      targetHash: f[5]!,
      fallbackSlot: f[6]!.readBigUInt64LE(0),
      fallbackHash: f[7]!,
      result: f[8]!,
    };
  }
  return null;
}

/**
 * Re-checks a forced draw from its log: the result is H("jumpball:forced" ‖ target hash ‖ fallback
 * hash ‖ draw). With the feed's settings and the draw's target slot, also checks the fallback slot
 * used is at or after the one the feed fixes (the slot hashes themselves are the chain's).
 */
export function checkForced(
  f: ForcedLog,
  feed?: { timeoutSlots: number; fallbackDelaySlots: number; drawTargetSlot: bigint },
): { resultOk: boolean; slotsOk: boolean | null } {
  const resultOk = createHash('sha256')
    .update('jumpball:forced')
    .update(f.targetHash)
    .update(f.fallbackHash)
    .update(f.draw.toBuffer())
    .digest()
    .equals(f.result);
  const slotsOk = feed
    ? f.targetSlot >= feed.drawTargetSlot && f.fallbackSlot >= fallbackSlot(feed.drawTargetSlot, feed.timeoutSlots, feed.fallbackDelaySlots)
    : null;
  return { resultOk, slotsOk };
}
