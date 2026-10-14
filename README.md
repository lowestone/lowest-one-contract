# lowest.one game contract

The Solana program behind [lowest.one](https://www.lowest.one): pick the lowest number nobody
else picks, and you win the pot. Paid in $UNIQUE.

| | |
|---|---|
| Program (mainnet) | [`DGGCp8MjbdjtxENbgQVK8m2MKdckxtypQK12ssqcJ7be`](https://solscan.io/account/DGGCp8MjbdjtxENbgQVK8m2MKdckxtypQK12ssqcJ7be) |
| $UNIQUE mint | [`7JMvTtB11iw9HZjY44SNFCB1hDxqeTXCjjpUiLsjpump`](https://solscan.io/token/7JMvTtB11iw9HZjY44SNFCB1hDxqeTXCjjpUiLsjpump) (Token-2022, pump.fun) |
| Framework | Anchor 1.2.0 |

This repository holds the contract only, exactly as deployed. The website and the keeper
(the service that prices tickets, relays reveals and pays out) live elsewhere; everything they
do goes through the instructions below, and anyone can call the public ones.

## How a round works

The whole lifecycle is in the header of [`programs/unique/src/lib.rs`](programs/unique/src/lib.rs). In short:

1. **Open.** Players pay a ticket in $UNIQUE and submit a *sealed* pick: a sha256 commitment of
   their number(s), a salt and their wallet. Nobody can see the numbers, not even us. Picks can
   be changed until 60 seconds before the close. Since v6 one pick account can hold up to 10
   numbers (a bundle), each paying its own ticket.
2. **Reveal.** After the close, picks are opened (number + salt), checked against the commitment
   and counted per number. The site timelocks a copy of each pick to the close with
   [drand](https://drand.love), so picks reveal even if the player is offline.
3. **Claim.** Anyone can point at a revealed number that was picked exactly once; the round keeps
   the lowest ones.
4. **Settle.** Anyone can settle: the lowest unique numbers are paid by the round's tier (busier
   rounds pay more winners); unclaimed shares roll over into the next round, which opens in the
   same transaction.

Every ticket is split on-chain: prize pool, stakers, burn, and the marketing wallet (a referrer's
share comes out of the marketing share). Rule changes by the admin only apply to rounds that open
afterwards, and are bounded in code (e.g. the pool share can't go below 70%). The one exception
is the ticket price: the keeper keeps it near its dollar price, at most every 10 minutes and
within a set step, and a pick never pays more than the price the player saw.

## Build and test

```bash
anchor build                      # the program and its IDL
cargo build-sbf --arch v2 --sbf-out-dir target/deploy-test && cargo test   # LiteSVM tests
```

## Verify the deployed program

The program on mainnet was built with `anchor build` (Anchor 1.2.0, Solana 4.1, platform-tools
v1.57, SBPF v3). The same build runs reproducibly in Docker with
[solana-verify](https://github.com/Ellipsis-Labs/solana-verifiable-build):

```bash
solana-verify build --library-name unique \
  --base-image solanafoundation/solana-verifiable-build:4.1.2 \
  --arch v3 --cargo-build-sbf-args="--tools-version v1.57"
solana-verify get-executable-hash target/deploy/unique.so
solana-verify get-program-hash -u https://api.mainnet-beta.solana.com DGGCp8MjbdjtxENbgQVK8m2MKdckxtypQK12ssqcJ7be
```

Both hashes are `997d59adddabdfefaff20e2c2b73e9ef83b55be47e6f55e7cef2ca115cc901b5` for the
program deployed on 2026-10-14 (v6): the deployed program is exactly this code.
