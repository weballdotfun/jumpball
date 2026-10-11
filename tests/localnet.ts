// End-to-end on a local validator with the built program loaded at PROGRAM_ID (see run-localnet.sh).
import { createHash, randomBytes } from 'node:crypto';
import assert from 'node:assert/strict';
import {
  Connection, Keypair, PublicKey, SystemProgram, SYSVAR_SLOT_HASHES_PUBKEY, Transaction, TransactionInstruction, sendAndConfirmTransaction,
} from '@solana/web3.js';

const PROGRAM_ID = new PublicKey(process.env.PROGRAM_ID!);
const conn = new Connection(process.env.RPC_URL ?? 'http://127.0.0.1:18899', 'confirmed');
const sha = (b: Uint8Array) => createHash('sha256').update(b).digest();

/** A hash chain: links[0] is the commitment (s_0); links[i] is revealed for the i-th draw. */
function chain(n: number): Buffer[] {
  const links = [randomBytes(32)];
  for (let i = 0; i < n; i++) links.push(sha(links[links.length - 1]!));
  return links.reverse();
}
const u64 = (n: bigint) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(n); return b; };
const u32 = (n: number) => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
const ix = (data: Buffer, keys: [PublicKey, boolean, boolean][]) =>
  new TransactionInstruction({ programId: PROGRAM_ID, data, keys: keys.map(([pubkey, isSigner, isWritable]) => ({ pubkey, isSigner, isWritable })) });
const send = (i: TransactionInstruction, ...s: Keypair[]) => sendAndConfirmTransaction(conn, new Transaction().add(i), s, { commitment: 'confirmed' });
async function custom(p: Promise<unknown>, code: number) {
  await assert.rejects(p, (e: Error) => new RegExp(`custom program error: 0x${code.toString(16)}\\b|"Custom":${code}\\b`).test(String(e) + JSON.stringify((e as { logs?: unknown }).logs ?? '')));
}
async function waitSlot(target: number) { while ((await conn.getSlot('confirmed')) <= target) await new Promise((r) => setTimeout(r, 200)); }

const operator = Keypair.generate();
const game = Keypair.generate(); // stands in for a consuming program's signer
await conn.confirmTransaction(await conn.requestAirdrop(operator.publicKey, 10e9), 'confirmed');
await conn.confirmTransaction(await conn.requestAirdrop(game.publicKey, 10e9), 'confirmed');

const links = chain(10);
const id = 1n;
const [feed] = PublicKey.findProgramAddressSync([Buffer.from('feed'), operator.publicKey.toBuffer(), u64(id)], PROGRAM_ID);
const DELAY = 2, TIMEOUT = 20;
await send(ix(Buffer.concat([Buffer.from([0]), u64(id), links[0]!, u32(DELAY), u32(TIMEOUT)]),
  [[operator.publicKey, true, true], [operator.publicKey, true, false], [feed, false, true], [SystemProgram.programId, false, false]]), operator);

const drawKey = (nonce: Buffer) => PublicKey.findProgramAddressSync([Buffer.from('draw'), feed.toBuffer(), game.publicKey.toBuffer(), nonce], PROGRAM_ID)[0];
async function request() {
  const nonce = randomBytes(32);
  const draw = drawKey(nonce);
  await send(ix(Buffer.concat([Buffer.from([1]), nonce]),
    [[game.publicKey, true, true], [game.publicKey, true, false], [feed, false, true], [draw, false, true], [SystemProgram.programId, false, false]]), game);
  const d = (await conn.getAccountInfo(draw))!.data;
  return { draw, target: Number(d.readBigUInt64LE(160)), status: () => conn.getAccountInfo(draw).then((a) => a!.data[9]) };
}
const reveal = (draw: PublicKey, secret: Buffer) => send(ix(Buffer.concat([Buffer.from([2]), secret]),
  [[operator.publicKey, true, false], [feed, false, true], [draw, false, true], [SYSVAR_SLOT_HASHES_PUBKEY, false, false]]), operator);

// 1. Request, too early to reveal, then a wrong secret and a skipped link are refused, then the right one.
const a = await request();
await custom(reveal(a.draw, links[1]!), 3); // TooEarly
await waitSlot(a.target);
await custom(reveal(a.draw, links[2]!), 1); // NotNextLink (skips a link)
await custom(reveal(a.draw, randomBytes(32)), 1);
await reveal(a.draw, links[1]!);
const da = (await conn.getAccountInfo(a.draw))!.data;
assert.equal(da[9], 1, 'fulfilled');
assert.notDeepEqual(da.subarray(168, 200), Buffer.alloc(32), 'a result is written');
console.log('draw 1 fulfilled, result', da.subarray(168, 176).toString('hex'), '…');

// 2. Draws are revealed in order: the third can't be revealed before the second.
const b = await request();
const c = await request();
await waitSlot(c.target);
await custom(reveal(c.draw, links[2]!), 2); // OutOfOrder
await reveal(b.draw, links[2]!);
await reveal(c.draw, links[3]!);
assert.equal(await b.status(), 1);
assert.equal(await c.status(), 1);
console.log('draws 2 and 3 fulfilled in order');

// 3. A draw nobody reveals in time can be expired by anyone; the late secret still advances the chain.
const d = await request();
await waitSlot(d.target + TIMEOUT);
const stranger = Keypair.generate();
await conn.confirmTransaction(await conn.requestAirdrop(stranger.publicKey, 1e9), 'confirmed');
await send(ix(Buffer.from([3]), [[feed, false, true], [d.draw, false, true]]), stranger);
assert.equal(await d.status(), 2, 'failed');
await reveal(d.draw, links[4]!);
const f = (await conn.getAccountInfo(feed))!.data;
assert.equal(f.readBigUInt64LE(96), 4n, 'four secrets revealed');
assert.equal(f.readBigUInt64LE(104), 1n, 'one failure, public');
console.log('draw 4 expired (refund), chain advanced; failures = 1');

// 4. The requester closes a settled draw and gets its rent back.
const before = await conn.getBalance(game.publicKey);
await send(ix(Buffer.from([4]), [[game.publicKey, true, false], [a.draw, false, true], [game.publicKey, false, true]]), game);
assert.equal(await conn.getAccountInfo(a.draw), null);
assert.ok((await conn.getBalance(game.publicKey)) > before - 10_000, 'rent returned (less the fee)');
console.log('draw 1 closed, rent back');
console.log('ALL PASSED');
