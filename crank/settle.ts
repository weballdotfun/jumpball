// Settler: anyone can run this for any feed with a fallback. For each pending draw whose reveal
// window has closed it waits for the fallback slot and calls force_settle (permissionless), so a
// withheld reveal still settles to a result nobody chose. If a draw's fallback window has also
// closed (nobody settled it in time), it expires the draw so the consumer can refund.
//
//   JUMPBALL_PROGRAM=<id> JUMPBALL_FEED=<feed address> JUMPBALL_KEYPAIR=any.json \
//   RPC_URL=http://127.0.0.1:8899 npx tsx crank/settle.ts
//
// The keypair only pays transaction fees; it needs no authority over the feed.
import { readFileSync } from 'node:fs';
import bs58 from 'bs58';
import { Connection, Keypair, PublicKey, Transaction, sendAndConfirmTransaction } from '@solana/web3.js';
import { DrawStatus, expireIx, fallbackSlot, forceSettleIx, readDraw, readFeed } from '../sdk/src/index.js';

/** One pass over the feed's pending draws. Returns what it did, one line per draw acted on. */
export async function settleDue(conn: Connection, program: PublicKey, feed: PublicKey, caller: Keypair): Promise<string[]> {
  const info = await conn.getAccountInfo(feed, 'confirmed');
  if (!info) throw new Error(`feed ${feed.toBase58()} not found`);
  const f = readFeed(info.data);
  if (f.fallbackDelaySlots === 0) return []; // no fallback: draws can only be expired, as before
  const pending = await conn.getProgramAccounts(program, {
    commitment: 'confirmed',
    filters: [{ dataSize: 232 }, { memcmp: { offset: 16, bytes: feed.toBase58() } }, { memcmp: { offset: 9, bytes: bs58.encode([DrawStatus.Pending]) } }],
  });
  const now = BigInt(await conn.getSlot('confirmed'));
  const done: string[] = [];
  for (const a of pending) {
    const d = readDraw(a.account.data);
    const fb = fallbackSlot(d.targetSlot, f.timeoutSlots, f.fallbackDelaySlots);
    if (now <= fb) continue; // the reveal may still come, or the fallback slot hasn't passed
    const closed = now > fb + BigInt(f.timeoutSlots);
    const ix = closed ? expireIx({ program, feed, draw: a.pubkey }) : forceSettleIx({ program, caller: caller.publicKey, feed, draw: a.pubkey });
    try {
      const sig = await sendAndConfirmTransaction(conn, new Transaction().add(ix), [caller], { commitment: 'confirmed' });
      done.push(`${closed ? 'expired' : 'forced'} draw ${d.secretIndex} ${sig.slice(0, 12)}…`);
    } catch (err) {
      // Someone else may have settled it first, or a skipped fallback slot has no successor yet.
      done.push(`draw ${d.secretIndex}: ${err instanceof Error ? err.message.split('\n')[0] : String(err)}`);
    }
  }
  return done;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const conn = new Connection(process.env.RPC_URL ?? 'http://127.0.0.1:8899', 'confirmed');
  const program = new PublicKey(process.env.JUMPBALL_PROGRAM!);
  const feed = new PublicKey(process.env.JUMPBALL_FEED!);
  const caller = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(process.env.JUMPBALL_KEYPAIR!, 'utf8'))));
  console.log(`settler: feed ${feed.toBase58()}`);
  for (;;) {
    try {
      for (const line of await settleDue(conn, program, feed, caller)) console.log(line);
    } catch (err) {
      console.error(`settler: ${err instanceof Error ? err.message : String(err)}`);
    }
    await new Promise((res) => setTimeout(res, 1_000));
  }
}
