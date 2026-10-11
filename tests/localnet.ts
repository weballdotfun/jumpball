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
// 5. The reference crank reveals a new draw by itself, in order, once its slot passes.
const { revealNext } = await import('../crank/reveal.js');
const { parseRevealLog, checkReveal } = await import('../sdk/src/index.js');
const e = await request();
let r: Awaited<ReturnType<typeof revealNext>> = 'waiting';
for (let i = 0; i < 100 && typeof r !== 'object'; i++) {
  r = await revealNext(conn, PROGRAM_ID, operator, id, links);
  if (typeof r !== 'object') await new Promise((res) => setTimeout(res, 300));
}
assert.ok(typeof r === 'object' && r.revealed === 5n && r.status === 1, 'crank revealed draw 5');
assert.equal(await e.status(), 1);
console.log('crank revealed draw 5');
// The reveal's log alone re-checks the draw: secret 5 hashes to secret 4 and the result matches.
{
  const tx = await conn.getTransaction((r as { signature: string }).signature, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 });
  const log = parseRevealLog(tx!.meta!.logMessages!);
  assert.ok(log && log.index === 5n, 'the reveal logged draw 5');
  assert.deepEqual(checkReveal(log!, links[4]!), { chainOk: true, resultOk: true });
  assert.ok((await conn.getAccountInfo(e.draw, 'confirmed'))!.data.subarray(168, 200).equals(log!.result), 'the log matches the account');
  assert.deepEqual(checkReveal({ ...log!, result: Buffer.alloc(32) }, links[4]!).resultOk, false);
  console.log('reveal log re-checks the draw');
}
// 6. Fees (set by the feed authority; the requester names the most it will pay).
{
  const { requestIx, setFeeIx, readFeed } = await import('../sdk/src/index.js');
  const TOKEN = new PublicKey('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');
  const sendMany = (ixs: TransactionInstruction[], ...s: Keypair[]) => sendAndConfirmTransaction(conn, new Transaction().add(...ixs), s, { commitment: 'confirmed' });
  const tokenIx = (data: Buffer, keys: [PublicKey, boolean, boolean][]) =>
    new TransactionInstruction({ programId: TOKEN, data, keys: keys.map(([pubkey, isSigner, isWritable]) => ({ pubkey, isSigner, isWritable })) });
  const rent = async (n: number) => conn.getMinimumBalanceForRentExemption(n);
  const fee = (a: { kind: 'sol' | 'token'; amount: bigint; burnBps: number; account: PublicKey }, signer = operator) =>
    send(setFeeIx({ program: PROGRAM_ID, authority: signer.publicKey, feed, ...a }), signer);
  const req = (extra: Partial<Parameters<typeof requestIx>[0]> = {}) => {
    const nonce = randomBytes(32);
    return send(requestIx({ program: PROGRAM_ID, payer: game.publicKey, requester: game.publicKey, feed, nonce, ...extra }), game);
  };

  const wallet = Keypair.generate().publicKey;
  await custom(fee({ kind: 'sol', amount: 1_000_000n, burnBps: 0, account: wallet }, game), 8); // not the authority
  await custom(fee({ kind: 'sol', amount: 1_000_000n, burnBps: 100, account: wallet }), 10); // SOL can't burn
  await fee({ kind: 'sol', amount: 1_000_000n, burnBps: 0, account: wallet });
  assert.equal(readFeed((await conn.getAccountInfo(feed))!.data).fee.amount, 1_000_000n);
  await custom(req(), 11); // no maximum given
  await custom(req({ maxFee: 999_999n, fee: { kind: 'sol', wallet } }), 11); // agreed to less than the fee
  await custom(req({ maxFee: 1_000_000n, fee: { kind: 'sol', wallet: Keypair.generate().publicKey } }), 7); // wrong wallet
  await req({ maxFee: 1_000_000n, fee: { kind: 'sol', wallet } });
  assert.equal(await conn.getBalance(wallet, 'confirmed'), 1_000_000);
  console.log('SOL fee paid to the fee wallet');

  // A token fee, half burned and half to the fee account (the founder: 50% burned, 50% LP).
  const mint = Keypair.generate(), payerToken = Keypair.generate(), feeToken = Keypair.generate(), lp = Keypair.generate().publicKey;
  await sendMany([
    SystemProgram.createAccount({ fromPubkey: game.publicKey, newAccountPubkey: mint.publicKey, lamports: await rent(82), space: 82, programId: TOKEN }),
    tokenIx(Buffer.concat([Buffer.from([20, 6]), game.publicKey.toBuffer(), Buffer.from([0])]), [[mint.publicKey, false, true]]),
    SystemProgram.createAccount({ fromPubkey: game.publicKey, newAccountPubkey: payerToken.publicKey, lamports: await rent(165), space: 165, programId: TOKEN }),
    tokenIx(Buffer.concat([Buffer.from([18]), game.publicKey.toBuffer()]), [[payerToken.publicKey, false, true], [mint.publicKey, false, false]]),
    SystemProgram.createAccount({ fromPubkey: game.publicKey, newAccountPubkey: feeToken.publicKey, lamports: await rent(165), space: 165, programId: TOKEN }),
    tokenIx(Buffer.concat([Buffer.from([18]), lp.toBuffer()]), [[feeToken.publicKey, false, true], [mint.publicKey, false, false]]),
    tokenIx(Buffer.concat([Buffer.from([7]), u64(10_000_000n)]), [[mint.publicKey, false, true], [payerToken.publicKey, false, true], [game.publicKey, true, false]]),
  ], game, mint, payerToken, feeToken);
  const amountOf = async (k: PublicKey) => (await conn.getAccountInfo(k, 'confirmed'))!.data.readBigUInt64LE(64);
  const supply = async () => (await conn.getAccountInfo(mint.publicKey, 'confirmed'))!.data.readBigUInt64LE(36);
  await custom(fee({ kind: 'token', amount: 1_000_000n, burnBps: 10_001, account: feeToken.publicKey }), 10);
  await fee({ kind: 'token', amount: 1_000_001n, burnBps: 5_000, account: feeToken.publicKey });
  const tokenFee = (feeTok = feeToken.publicKey) => ({ kind: 'token' as const, payerToken: payerToken.publicKey, mint: mint.publicKey, feeToken: feeTok, tokenProgram: TOKEN });
  await custom(req({ maxFee: 1_000_001n, fee: tokenFee(payerToken.publicKey) }), 7); // wrong fee account
  await req({ maxFee: 1_000_001n, fee: tokenFee() });
  assert.equal(await amountOf(payerToken.publicKey), 10_000_000n - 1_000_001n, 'the payer paid the fee');
  assert.equal(await amountOf(feeToken.publicKey), 500_001n, 'half (rounded up) to the fee account');
  assert.equal(await supply(), 10_000_000n - 500_000n, 'half burned (rounded down)');
  console.log('token fee: half burned, half to the fee account');

  await fee({ kind: 'sol', amount: 0n, burnBps: 0, account: PublicKey.default });
  await req();
  console.log('fee set back to 0: requests are free again');
}
console.log('ALL PASSED');
