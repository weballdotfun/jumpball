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
}

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
}

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
}): TransactionInstruction {
  const feed = feedAddress(a.program, a.authority, a.id);
  return ix(a.program, Buffer.concat([Buffer.from([0]), u64(a.id), Buffer.from(a.head), u32(a.delaySlots), u32(a.timeoutSlots)]), [
    [a.payer, true, true],
    [a.authority, true, false],
    [feed, false, true],
    [SystemProgram.programId, false, false],
  ]);
}

export function requestIx(a: { program: PublicKey; payer: PublicKey; requester: PublicKey; feed: PublicKey; nonce: Uint8Array }): TransactionInstruction {
  const draw = drawAddress(a.program, a.feed, a.requester, a.nonce);
  return ix(a.program, Buffer.concat([Buffer.from([1]), Buffer.from(a.nonce)]), [
    [a.payer, true, true],
    [a.requester, true, false],
    [a.feed, false, true],
    [draw, false, true],
    [SystemProgram.programId, false, false],
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

export function closeDrawIx(a: { program: PublicKey; requester: PublicKey; draw: PublicKey; payer: PublicKey }): TransactionInstruction {
  return ix(a.program, Buffer.from([4]), [
    [a.requester, true, false],
    [a.draw, false, true],
    [a.payer, false, true],
  ]);
}

export function recommitIx(a: { program: PublicKey; authority: PublicKey; feed: PublicKey; head: Uint8Array }): TransactionInstruction {
  return ix(a.program, Buffer.concat([Buffer.from([5]), Buffer.from(a.head)]), [
    [a.authority, true, false],
    [a.feed, false, true],
  ]);
}

export function readFeed(data: Buffer): Feed {
  if (data.length !== 184 || data.subarray(0, 8).toString() !== 'JBFEED01') throw new Error('not a Jumpball feed');
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

/** A fulfilled draw as its reveal logged it ("Program data: jumpball reveal …"). */
export interface RevealLog {
  draw: PublicKey;
  index: bigint;
  secret: Buffer;
  slot: bigint;
  slotHash: Buffer;
  result: Buffer;
}

/** Reads the reveal event from a transaction's log lines, if it has one. */
export function parseRevealLog(logs: readonly string[]): RevealLog | null {
  for (const line of logs) {
    if (!line.startsWith('Program data: ')) continue;
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
