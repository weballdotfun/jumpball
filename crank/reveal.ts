// Reference crank: reveals a feed's draws in order as soon as each target slot has passed, with no
// human in the loop (README "Known limitation"). Run it next to the operator key:
//
//   JUMPBALL_PROGRAM=<id> JUMPBALL_FEED_ID=1 JUMPBALL_SEED_FILE=seed.bin JUMPBALL_CHAIN=100000 \
//   JUMPBALL_KEYPAIR=operator.json RPC_URL=http://127.0.0.1:8899 npx tsx crank/reveal.ts
//
// The seed file holds the 32-byte chain seed; keep it as secret as the operator key.
import { readFileSync } from 'node:fs';
import bs58 from 'bs58';
import { Connection, Keypair, PublicKey, Transaction, sendAndConfirmTransaction } from '@solana/web3.js';
import { DrawStatus, feedAddress, hashChain, readDraw, readFeed, revealIx } from '../sdk/src/index.js';

const u64 = (n: bigint) => {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(n);
  return b;
};

/** The draw waiting for the feed's next secret, if one has been requested. */
export async function nextDraw(conn: Connection, program: PublicKey, feed: PublicKey, index: bigint) {
  const found = await conn.getProgramAccounts(program, {
    commitment: 'confirmed',
    filters: [{ dataSize: 232 }, { memcmp: { offset: 16, bytes: feed.toBase58() } }, { memcmp: { offset: 144, bytes: bs58.encode(u64(index)) } }],
  });
  const a = found[0];
  return a ? { address: a.pubkey, draw: readDraw(a.account.data) } : null;
}

/** One step: reveal the next draw if its target slot has passed. Returns what it did. */
export async function revealNext(
  conn: Connection,
  program: PublicKey,
  authority: Keypair,
  feedId: bigint,
  links: Buffer[],
): Promise<'none' | 'waiting' | { revealed: bigint; status: DrawStatus; signature: string }> {
  const feedKey = feedAddress(program, authority.publicKey, feedId);
  const info = await conn.getAccountInfo(feedKey, 'confirmed');
  if (!info) throw new Error(`feed ${feedKey.toBase58()} not found`);
  const feed = readFeed(info.data);
  if (feed.revealed === feed.assigned) return 'none';
  const index = feed.revealed + 1n;
  const next = await nextDraw(conn, program, feedKey, index);
  if (!next) return 'none'; // assigned but not visible yet: next tick
  const slot = await conn.getSlot('confirmed');
  if (BigInt(slot) <= next.draw.targetSlot) return 'waiting';
  const secret = links[Number(index)];
  if (!secret) throw new Error(`the chain has no secret ${index}: recommit a longer one`);
  const signature = await sendAndConfirmTransaction(
    conn,
    new Transaction().add(revealIx({ program, authority: authority.publicKey, feed: feedKey, draw: next.address, secret })),
    [authority],
    { commitment: 'confirmed' },
  );
  const after = readDraw((await conn.getAccountInfo(next.address, 'confirmed'))!.data);
  return { revealed: index, status: after.status, signature };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const conn = new Connection(process.env.RPC_URL ?? 'http://127.0.0.1:8899', 'confirmed');
  const program = new PublicKey(process.env.JUMPBALL_PROGRAM!);
  const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(process.env.JUMPBALL_KEYPAIR!, 'utf8'))));
  const feedId = BigInt(process.env.JUMPBALL_FEED_ID ?? '1');
  const links = hashChain(readFileSync(process.env.JUMPBALL_SEED_FILE!), Number(process.env.JUMPBALL_CHAIN ?? '100000'));
  console.log(`crank: feed ${feedAddress(program, authority.publicKey, feedId).toBase58()}, chain of ${links.length - 1}`);
  for (;;) {
    try {
      const r = await revealNext(conn, program, authority, feedId, links);
      if (typeof r === 'object') console.log(`revealed draw ${r.revealed}: ${DrawStatus[r.status]} ${r.signature.slice(0, 12)}…`);
      if (r !== 'none' && r !== 'waiting') continue; // more may be due at once
    } catch (err) {
      console.error(`crank: ${err instanceof Error ? err.message : String(err)}`);
    }
    await new Promise((res) => setTimeout(res, 400));
  }
}
