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
4. **Timeout and fallback.** If the reveal doesn't arrive within the reveal window, a feed with a
   fallback (`fallback_delay_slots` > 0) settles the draw anyway: once a later **fallback slot**
   has passed, anyone calls `force_settle`, and the result is
   `H("jumpball:forced" || slot_hash(target) || slot_hash(fallback) || draw)`. The draw is marked
   **Forced**, which consumers treat like a revealed draw. Without a fallback, or if nobody settles
   it within the fallback window either, the draw is marked failed and the consuming program
   refunds.

What each party can and can't do:

- **The operator** committed to its secrets before any draw and can't change them. It also can't
  know the slot hash when entries close.
- **A validator** can influence a slot hash only by skipping its slot, and it doesn't know the
  operator's secret, so it can't tell which slot hash would help.
- **Someone holding the secrets** still doesn't know the slot hash while entries are open.
- **Withholding a reveal** can't pick an outcome either: on a feed with a fallback it re-rolls the
  draw once from slot hashes nobody knows yet (see "Known limitation"), and without one it fails the
  draw (refund).

## Checking a draw

Every fulfilled reveal logs its inputs and result (`Program data: jumpball reveal <draw> <index>
<secret> <slot> <slot hash> <result>`), so a draw can be re-checked from the transaction alone, even
after its account is closed. The SDK's `parseRevealLog` reads it and `checkReveal` confirms that the
secret hashes to the link committed before it and that the result is
`H("jumpball" ‖ secret ‖ slot hash ‖ draw)`.

A forced draw logs its inputs the same way (`Program data: jumpball forced <draw> <index> <target
slot> <target hash> <fallback slot> <fallback hash> <result>`, the slots being the produced slots
used). `parseForcedLog` reads it and `checkForced` confirms the result is
`H("jumpball:forced" ‖ target hash ‖ fallback hash ‖ draw)` and, given the feed's settings, that the
fallback slot is at or after `target + timeout_slots + fallback_delay_slots`. The slot hashes
themselves are the chain's, as for a reveal. The distinct domain tag means a forced result can never
equal a revealed one.

## When a chain runs out

A feed knows how many secrets its committed chain holds (`chain_end`). The feed's **commit authority**,
a key apart from the operator (ours is on a hardware wallet), commits the next chain ahead of time
with `commit_next`: requests may then run on into it, and the first reveal past `chain_end` switches
to it. A committed next chain can't be replaced (draws may already be assigned to it), and the
operator can't commit chains at all, so a stolen operator key can't commit a chain nobody can reveal.
Without a next chain, requests stop at `chain_end` (`ChainExhausted`); a draw is never assigned a
secret that doesn't exist.

## Closing draws

The requester closes a settled draw to get its rent back, but only once its secret has been revealed.
Reveals go in order and each needs its draw's account, so closing a forced or failed draw before its
reveal would stop the chain for good.

## Who may request

Anyone, unless the feed names an `allowed_requester` at creation: then only that key (for example a
game's own fixed PDA). A free feed for one consumer can't be flooded by others to delay its draws.

## Fees

A feed may charge for each request, set by its authority with `set_fee` (instruction 6): an amount in
SOL (paid to a wallet) or in a token (paid to a token account of that mint, SPL Token or Token-2022),
with a share of a token fee burned from the payer (`burn_bps`). A request names the most it will pay
(`max_fee`, the 8 bytes after the nonce), so a fee raised after a request was built can't charge more
than it agreed to. New feeds are free. Every fee paid logs `Program data: jumpball fee <draw> <kind>
<amount> <burned>`.

## Known limitation (and how it's handled)

Whoever holds the secrets can compute a draw's result once its slot hash is known, before revealing
it, and could choose **not** to reveal. **A missed reveal can re-roll a draw once, and every one is
publicly counted.**

- Withholding a reveal gives the operator a choice between the known revealed result R and an
  unknown fallback result F: one re-roll per draw. It cannot pick F. The fallback slot comes
  `fallback_delay_slots` after the reveal window closes, so its hash doesn't exist when the operator
  decides not to reveal. Every forced draw is public (`forced` in the feed). Consumers should keep
  the value decided by any single draw small (caps) and publish the forced count.
- `force_settle` is permissionless and one honest caller suffices: the result is fixed by the slot
  hashes, whoever calls it. Reaching a refund instead needs everyone to abstain for the whole
  fallback window, and those are counted in `failures`.
- A forced draw relies only on slot hashes, with no secret in it. The leader producing the fallback slot
  can vary its block to try many slot hashes and pick a result it likes before publishing: real control,
  not just skipping. That is why the fallback delay is at least 8 slots (two leader windows), so the
  fallback slot is never produced by the leader of the reveal window's last slot; it also means every
  forced draw is a weaker draw. A skipped fallback slot rolls on to the next produced slot, as target
  slots do. Treat a rising `forced` count as an incident.
- On a feed without a fallback (`fallback_delay_slots` = 0) a missed reveal fails the draw and the
  consuming program refunds; a dishonest operator colluding with a player could use those refunds
  as an "undo" on lost rounds. Prefer a fallback.

Reveals are sent automatically by a crank as soon as the slot hash exists, with no human in the
loop, and anyone can run the settler (`crank/settle.ts`).

### Trust assumptions, stated plainly

- **Operator and validator together:** an operator colluding with the leader of a draw's target slot
  can choose that draw's result (the leader knows the secret and can vary the slot hash). Each alone
  can't.
- **The seed:** whoever holds an operator's chain seed knows each draw's result as soon as its target
  slot exists. That is safe only if the consumer allows no action between locking a draw and settling
  it (Shot Clock allows none).
- **No key rotation:** a feed's operator and commit authority can't be changed; losing the operator key
  means moving to a new feed.
- **Fee mints:** a token fee works with plain SPL Token and Token-2022 mints. Token-2022 extensions such
  as transfer hooks, required memos, pausing, non-transferability or CPI guard make requests fail, and a
  transfer fee means the fee account receives less than the logged amount. Check a mint's extensions
  before setting it as a fee.

## Program

| # | Instruction | What it does |
|---|---|---|
| 0 | `init_feed` | An operator commits a hash chain (`head = s_0`) and its length, with the slot delay, reveal window, fallback delay (0 = none, else at least 8), commit authority and allowed requester (none = anyone). |
| 1 | `request` | A consumer locks a draw at the current slot; it is assigned the feed's next secret. It may name the most it will pay and the fee's kind. |
| 2 | `reveal` | The operator reveals that secret; the result is `H("jumpball" ‖ secret ‖ slot hash ‖ draw)`. |
| 3 | `expire` | Anyone fails a draw whose reveal window closed, and with a fallback, whose fallback window closed too (the consumer refunds). |
| 4 | `close_draw` | The requester closes a settled draw once its secret is revealed; its payer gets the rent back. |
| 5 | `commit_next` | The commit authority commits the next chain ahead of time; it takes over past the current one's end. |
| 6 | `set_fee` | The operator sets the per-request fee: SOL to a wallet, or a token with part burned. |
| 7 | `force_settle` | Anyone settles a withheld draw from slot hashes once the fallback slot has passed (status Forced). |

The target slot is `lock + delay_slots`; the fallback slot is `target + timeout_slots +
fallback_delay_slots`, and `force_settle` stays open for `timeout_slots` after it. If either slot
was skipped, the first produced slot after it is used. A late reveal never changes a forced or
failed draw (and on a feed with a fallback it doesn't fail a pending one); it only advances the
chain, which must keep moving in order. A feed's settings (`delay + timeout`, plus `fallback delay +
timeout` with a fallback) are capped at 400 slots, inside the 512 the SlotHashes sysvar keeps.

## Layout

```
programs/jumpball   the on-chain program (Rust, no framework)
sdk/                TypeScript client: addresses, instructions, account readers, hash chain
crank/              reference cranks: reveal.ts reveals a feed's draws in order (operator);
                    settle.ts force-settles withheld draws (anyone)
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
