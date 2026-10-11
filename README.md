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

## Layout (planned)

```
programs/jumpball   the on-chain program (Rust)
sdk/                TypeScript client
crank/              reference crank: lock, reveal, timeout
tests/              localnet tests
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
