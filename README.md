# Jumpball

Provably fair, low-cost randomness for Solana programs. Built by [WeBall](https://weball.fun).

> Status: in development. Not deployed yet. Do not rely on it until a verified mainnet release is announced here.

## Why

On-chain games need a random result that nobody can steer: not the players, not the validator
producing the block, and not the game's operator. Oracle services solve this for a fee on every
request and add an outside dependency to every round. Jumpball is a small program that gets the
same guarantee from two ingredients that are cheap on Solana: a pre-committed secret, and a slot
hash nobody knows in advance.

## How it works

1. **Commit.** The operator generates a long hash chain off chain: `s_N`, `s_{N-1} = H(s_N)`, …,
   `s_0 = H(s_1)`, and publishes `s_0` on chain. From then on, every future secret is fixed: each
   revealed secret must hash to the previous one.
2. **Lock.** A consuming program (a game) locks a draw at slot `L`: from this point no more entries
   are accepted. The draw will use the hash of a slot after `L`, which nobody can know yet.
3. **Reveal.** The operator reveals the next secret `s_i`. Jumpball checks `H(s_i) == s_{i-1}` and
   computes the result as `H(s_i || slot_hash(L + k))`.
4. **Timeout.** If the reveal doesn't arrive within a set number of slots, the draw is marked
   failed, so the consuming program can refund.

Why nobody can steer it:

- **The operator** committed to its secrets before any draw and can't change them. It also can't
  know the slot hash when entries close.
- **A validator** can influence a slot hash only by skipping its slot, and it doesn't know the
  operator's secret, so it can't tell which slot hash would help.
- **Someone holding the secrets** still doesn't know the slot hash while entries are open.
- **Withholding a reveal** only fails the draw (refund). It can't pick an outcome.

## Checking a draw

Every fulfilled reveal logs its inputs and result (`Program data: jumpball reveal <draw> <index>
<secret> <slot> <slot hash> <result>`), so a draw can be re-checked from the transaction alone, even
after its account is closed. The SDK's `parseRevealLog` reads it and `checkReveal` confirms that the
secret hashes to the link committed before it and that the result is
`H("jumpball" ‖ secret ‖ slot hash ‖ draw)`.

## When a chain runs out

A feed knows how many secrets its committed chain holds (`chain_end`). Requests stop there
(`ChainExhausted`), so a draw is never assigned a secret that doesn't exist; the operator reveals
the waiting draws and commits a new chain, and requests resume. A new chain can't be committed while
draws are waiting, so the operator can't pick a chain after seeing a draw's slot hash.

## Fees

A feed may charge for each request, set by its authority with `set_fee` (instruction 6): an amount in
SOL (paid to a wallet) or in a token (paid to a token account of that mint, SPL Token or Token-2022),
with a share of a token fee burned from the payer (`burn_bps`). A request names the most it will pay
(`max_fee`, the 8 bytes after the nonce), so a fee raised after a request was built can't charge more
than it agreed to. New feeds are free. Every fee paid logs `Program data: jumpball fee <draw> <kind>
<amount> <burned>`.

## Known limitation (and how it's handled)

Whoever holds the secrets can compute a draw's result once its slot hash is known, before
revealing it, and could choose **not** to reveal. That can't pick an outcome: the draw simply fails
and the consuming program refunds. But a dishonest operator colluding with a player could use
failures as a free "undo" on rounds that player lost. Mitigations:

- Reveals are sent automatically by a crank as soon as the slot hash exists, with no human in the
  loop.
- Every failed draw is counted on chain in the feed (`failures`) and is public; a rising count is a
  red flag anyone can see.
- Consuming programs should cap what one player can put into a single draw.

## Program

| # | Instruction | What it does |
|---|---|---|
| 0 | `init_feed` | An operator commits a hash chain (`head = s_0`) and its length, with the slot delay and reveal window. |
| 1 | `request` | A consumer locks a draw at the current slot; it is assigned the feed's next secret. |
| 2 | `reveal` | The operator reveals that secret; the result is `H("jumpball" ‖ secret ‖ slot hash ‖ draw)`. |
| 3 | `expire` | Anyone fails a draw whose reveal window closed (the consumer refunds). |
| 4 | `close_draw` | The requester closes a settled draw and its payer gets the rent back. |
| 5 | `recommit` | The operator commits a new chain (and its length) once every assigned secret is revealed. |
| 6 | `set_fee` | The operator sets the per-request fee: SOL to a wallet, or a token with part burned. |

The target slot is `lock + delay_slots`; if that slot was skipped, the first produced slot after it
is used. The reveal window must fit in the 512 slots the SlotHashes sysvar keeps.

## Layout

```
programs/jumpball   the on-chain program (Rust, no framework)
sdk/                TypeScript client: addresses, instructions, account readers, hash chain
crank/              reference crank: reveals a feed's draws in order, automatically
tests/              end-to-end test on a local validator
```

## Build and test

```
cargo test                                  # unit tests
cargo-build-sbf --sbf-out-dir target/deploy # build the program
npm install && bash tests/run-localnet.sh   # end-to-end on its own local validator
```

## Verifying a deployment

Every release is built reproducibly. The deployed program can be checked against this source with
[solana-verify](https://github.com/Ellipsis-Labs/solana-verifiable-build), and its address and build
hash will be listed here.

## Security

Please report vulnerabilities privately to team@weball.fun rather than in public issues. A bug
bounty will be announced with the mainnet release.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
