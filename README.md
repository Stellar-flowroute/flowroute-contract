![FlowRoute Contract](banner.svg)

# flowroute-contract

flowroute-contract is the Soroban smart contract layer for FlowRoute, a payout tool for businesses that need to send money to many people at once, where each recipient may want a different currency. This contract swaps the source asset into each recipient's chosen destination asset through the Soroswap Router, enforces a per-recipient minimum-received floor on-chain, emits an auditable event per payout, and continues past any single recipient's failure without aborting the batch. The application layer that calls this contract lives in the sibling repo, flowroute-app.

**Documentation:** https://hollujay-labs.gitbook.io/flowroute/

[![CI](https://img.shields.io/github/actions/workflow/status/Stellar-flowroute/flowroute-contract/ci.yml?branch=main&label=CI)](https://github.com/Stellar-flowroute/flowroute-contract/actions/workflows/ci.yml)
[![Network](https://img.shields.io/badge/network-testnet-1c7ed6)](https://developers.stellar.org/docs/learn/fundamentals/networks)

## Quick Start

Requirements: the Stellar CLI (`stellar`) and a Rust nightly toolchain with the `wasm32v1-none` target. The toolchain is pinned in `rust-toolchain.toml`.

Build the contract wasm:

```bash
stellar contract build
```

Run the test suite:

```bash
cargo test
```

Deploy to testnet. `test-deployer` is a locally configured identity that signs and pays for the transactions:

```bash
stellar contract deploy \
  --wasm target/wasm32v1-none/release/flowroute_router.wasm \
  --source test-deployer \
  --network testnet \
  --alias flowroute-router
```

Initialize the newly deployed FlowRoute Router with the admin address and the
external Soroswap Router address for testnet (use the contract id printed by
deploy, or the `flowroute-router` alias):

```bash
stellar contract invoke \
  --id flowroute-router \
  --source test-deployer \
  --network testnet \
  -- initialize \
  --admin <ADMIN_ADDRESS> \
  --swap_router CCJUD55AG6W5HAI5LRVNKAE5WDP5XGZBUDS5WNTIVDU7O264UZZE7BRD
```

The initializer must authorize as the supplied admin: `test-deployer` must be
that address or one of its signers. The external Soroswap Router address is a
testnet-specific deployment dependency; supply the appropriate verified venue
address when deploying to another network.

This API change requires a fresh FlowRoute Router deployment, and the current
release has one: the deployment recorded in [Live testnet
verification](#live-testnet-verification) below was built from this revision and
initialized with the Soroswap Router above. The earlier testnet FlowRoute Router
listed further down was initialized with the prior interface and is not changed
by this release.

## Key Features

- **Batched payouts.** One transaction funds up to `MAX_BATCH_RECIPIENTS` recipients (see [Batch size limit](#batch-size-limit)). The full source amount is pulled from the sender once and distributed in the same call.
- **Multi-currency delivery.** Each recipient names a destination asset, and the router converts the source asset through the Soroswap Router.
- **On-chain slippage floor.** Every recipient must set a positive minimum received amount (`dest_min`); `execute_batch` rejects a zero or negative floor with `Error::InvalidAmount` before pulling the sender's total, because a zero floor would leave the recipient with no protection at all. FlowRoute measures the per-swap balance delta itself: an under-floor venue response returns `VenueUnderDelivered` and aborts the batch with an atomic rollback, so no partial payout or destination output is retained by the contract.
- **Auditable settlement.** Each payout run and every per-recipient result is emitted as an on-chain event, and a payout counter records how many runs have executed.
- **Failure isolation.** One recipient failing never aborts the batch. A swap that reverts is refunded to the sender at the end of the run.
- **Authorized venue pull.** The Soroswap Router moves the source tokens itself, from FlowRoute into the pair for the recipient's asset pair. FlowRoute resolves that pair from the venue and authorizes exactly that transfer, for that recipient's allocation, before the venue runs, so a real payout settles instead of silently refunding.
- **Pause switch.** The admin can pause execution between runs.

## Architecture

The router is a single Soroban contract in `contracts/router`. Storage holds the admin address, an immutable external Soroswap Router address, a paused flag, and a payout counter. Swaps are delegated to that configured venue. The public surface is four functions:

- `initialize(admin, swap_router)` sets the admin and immutable Soroswap Router venue, clears the paused flag, and resets the payout counter. It requires authorization from the supplied admin.
- `set_paused(paused)` pauses or unpauses the batch executor. Requires admin auth.
- `get_payout_count()` returns the number of payout runs executed so far.
- `execute_batch(sender, source_asset, recipients, total_source_amount)` executes one payout run. It validates the batch before moving any funds, rejecting a batch larger than `MAX_BATCH_RECIPIENTS` and any recipient whose allocation or `dest_min` is not positive, then pulls the total amount from the sender, swaps each recipient's allocation on the venue with the recipient's `dest_min` enforced as the output floor, emits per-recipient and per-run events, refunds failed swaps to the sender, and never aborts on a single failure.

### Venue fund flow and authorization

The configured Soroswap Router requires auth from `to` and then transfers the input tokens out of `to` and into the first pair of the swap path. FlowRoute therefore passes itself as `to` and has to authorize that transfer, because its invoker is the router rather than FlowRoute itself, so the usual rule that lets a contract move its own tokens does not apply.

For each recipient, `execute_batch`:

1. asks the venue for the pair it will transfer into (`router_pair_for`, the same resolution the router performs internally before its transfer: a salt over the sorted token pair, deployed from the venue's factory),
2. declares that exact transfer as an authorized invocation of this contract, naming the source token, `transfer`, this contract as `from`, the resolved pair as `to`, and the recipient's `amount_in`, and
3. calls the venue, which pulls the allocation into the pair, gets the output delivered to this contract, and is checked here against the recipient's `dest_min` using this contract's own balance delta.

A resolution is reused within a run when several recipients share a destination asset. Authorization is never blanket: the declared invocation is matched once, for that amount and that pair, so a venue that pulls into any other address is not authorized and fails closed, and the recipient is refunded like any other venue failure. Pairs are never hardcoded; a pair that cannot be resolved (a destination asset equal to the source asset, or an unreachable venue) fails that recipient and the rest of the batch continues.

The application layer lives in the sibling repository `flowroute-app`.

## Batch size limit

`execute_batch` accepts at most `MAX_BATCH_RECIPIENTS` recipients per call (6 as of this release). The limit is checked with the other batch guards, before the total is pulled from the sender, so an oversized batch reverts with `Error::TooManyRecipients` (code 10) and the sender's funds are never touched. Longer payout runs must be split into several calls.

The number is measured, not picked: one recipient costs the FlowRoute `payout` event plus the events that recipient's token transfers and swap emit, and a Stellar transaction may emit at most 16,384 bytes of contract events (`tx_max_contract_events_size_bytes`). That event budget, not CPU or memory, is what bounds the batch.

Calibration, run with the network transaction limits enforced (the default in the Soroban test environment, so an oversized batch fails during the test rather than silently passing). Each row uses a venue that reproduces the verified Soroswap router and pair event output for a single-hop swap:

| Recipients | Contract events (bytes) | Share of the event budget | Result |
| --- | --- | --- | --- |
| 1 | 2,208 | 13% | ok |
| 6 (`MAX_BATCH_RECIPIENTS`) | 11,228 | 69% | ok |
| 7 | 13,032 | 80% | ok |
| 8 | 14,836 | 91% | ok |
| 9 | 16,640 | 102% | rejected: `contract events size bytes: 16640 > 16384` |
| 10 | 18,444 | 113% | rejected |
| 12 | 22,052 | 135% | rejected |
| 16 | 29,268 | 179% | rejected |

Every other resource stays far inside its own limit. Re-measured through the shipped authorization path (the sender's invocation tree enforced, no blanket auth mocking), the envelope at the enforced maximum is 5,197,431 CPU instructions (1.3% of 400,000,000), 740,870 bytes of memory (1.8% of 41,943,040), 33 ledger entries read or written (8% of 400) and 2,864 bytes written to the ledger (2.2% of 132,096). Nothing else comes close even at much larger sizes: repeated against a venue that mirrors the router's fund flow but publishes none of the router's or pair's events, the event budget is exhausted at 17 recipients (`17200 > 16384`), and the next constraint behind it, the 400-entry footprint cap, is not reached until roughly 94 recipients. The authorization work did not move this: it adds one pair resolution per distinct destination asset per run and one 64-byte-argument authorization per recipient, no events, so the ceiling the event budget sets is unchanged.

The event footprint is exactly linear in the recipient count, 404 bytes of fixed cost (the batch event plus the single source transfer) plus 1,804 bytes per recipient, so the ceiling can be re-derived from smaller measurements. `batch_resource_envelope_reproduces_calibrated_ceiling` in `contracts/router/src/test.rs` re-measures the envelope at 1 and 6 recipients, checks every dimension against the network limits, and fails if the derived ceiling is no longer 8, so the maximum cannot drift above a changed event footprint unnoticed. The rows above the enforced maximum come from a sweep run before the limit was added; the committed test re-derives the ceiling from measurements the guard still allows.

One measurement caveat, so the envelope above is not read as more than it is. The calibration venue pulls the allocation into the pair and then delivers the output itself, instead of invoking the pair, because the Soroban test environment meters a fixed internal budget per contract invocation (a test-only guard on its authorization instrumentation) that the largest sweeps would otherwise exceed before the event budget could be observed. Measured at small sizes, invoking the pair adds about 105,000 CPU instructions and up to two ledger entries per recipient, so a fully pair-driven run at the maximum would be roughly 5.6M instructions (1.4%) and about 40 entries (10%) - still far inside every limit, and with byte-for-byte identical events, which is the dimension that bounds the batch.

Re-run the calibration:

```bash
cargo test --lib batch_resource_envelope -- --nocapture
```

## Live testnet verification

The current release was deployed to Stellar Testnet and exercised end to end
against the live Soroswap Router and its pools. No mock venue is involved in any
of the evidence below. Verified 2026-09-15.

| Item | Value |
| --- | --- |
| Network | Testnet (`Test SDF Network ; September 2015`) |
| Source revision | `2933107` |
| WASM sha256 | `0c2251cd29b10ab20966b4189a4c1a800e758c0edaf733bce48d26bff843fb7a` (30,530 bytes, byte-identical after a clean rebuild, and the hash the network recorded for the deployed instance) |
| WASM upload tx | [`967cb00d153bae61f67cfe369de92960535dcfa7d832d45f1b52242d4a254909`](https://stellar.expert/explorer/testnet/tx/967cb00d153bae61f67cfe369de92960535dcfa7d832d45f1b52242d4a254909) |
| Deploy tx | [`9a384afc053c76f816c40a3de63b75f836ea634349c55e2b548cd63a9f2702b4`](https://stellar.expert/explorer/testnet/tx/9a384afc053c76f816c40a3de63b75f836ea634349c55e2b548cd63a9f2702b4) |
| Contract | [`CBB3UVMGMFVWLF6ZVMQYRQDWOXZUWNW4SD6SERG3RMLFXMWLZNOZ767U`](https://stellar.expert/explorer/testnet/contract/CBB3UVMGMFVWLF6ZVMQYRQDWOXZUWNW4SD6SERG3RMLFXMWLZNOZ767U) |
| Initialize tx | [`07a09da09e5d015ad3f2d02cb19eb1b9a85a4e583e7c19ad04b9bc782b156a19`](https://stellar.expert/explorer/testnet/tx/07a09da09e5d015ad3f2d02cb19eb1b9a85a4e583e7c19ad04b9bc782b156a19) (ledger 4,691,023) |
| Source asset | XLM, the native asset contract `CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC` |
| Destination assets | USDC `CB3TLW74NBIOT3BUWOZ3TUM6RFDF6A4GVIRUQRQZABG5KPOUL4JJOV2F`, XTAR `CCZGLAUBDKJSQK72QOZHVU7CUWKW45OZWYWCLL27AEK74U2OIBK6LXF2` |
| Venue | Soroswap Router `CCJUD55AG6W5HAI5LRVNKAE5WDP5XGZBUDS5WNTIVDU7O264UZZE7BRD`, factory `CDP3HMUH6SMS3S7NPGNDJLULCOXXEPSHY4JKUKMBNQMATHDHWXRRJTBY` |

Every row below is a real signed transaction, submitted to Testnet and then
confirmed independently through Soroban RPC `getTransaction` and Horizon rather
than trusted from CLI output.

| Test | Expected | Actual | Tx hash | Result |
| --- | --- | --- | --- | --- |
| Single-recipient payout, XLM→USDC, `amount_in` 10 XLM, `dest_min` 0.7 USDC | recipient receives ≥ `dest_min`, no residue | delivered 7,617,467 (0.7617 USDC); contract holds 0 XLM and 0 USDC; counter 1 | [`f2bade9f…`](https://stellar.expert/explorer/testnet/tx/f2bade9f6228b5f52ef4211632ff3a97f5ae054c9013e428fbd87b6d4750e8b2) (ledger 4,691,030) | ok |
| Maximum batch, 6 recipients (5× XLM→USDC, 1× XLM→XTAR), 10 XLM each, total 60 XLM | all six receive ≥ their floor, no residue, counter increments once | all six delivered; each amount equalled the on-chain quote exactly (7,610,530 / 7,603,602 / 7,596,684 / 7,589,775 / 7,582,876 USDC, 91,658,230 XTAR); contract holds 0 of XLM, USDC and XTAR; counter 2 | [`f2114a2e…`](https://stellar.expert/explorer/testnet/tx/f2114a2e08fe2e2869921a589a9cc7dc0cd681cd2c71b1cc1ef048f578afbbe3) (ledger 4,691,040) | ok |
| 7 recipients | rejected before any transfer | `TooManyRecipients`; sender, contract and counter unchanged; no event | RPC simulation | ok |
| `dest_min = 0` | rejected before any transfer | `InvalidAmount`; sender, contract and counter unchanged; no event | RPC simulation | ok |
| batch while paused | rejected before any transfer | `Paused`; sender, contract and counter unchanged; no event | RPC simulation | ok |
| second `initialize` | rejected | `AlreadyInitialized` | RPC simulation | ok |

The rejection cases are labelled by the error the contract returned. Each failed
before any transfer, and after each attempt the sender's balance, this
contract's balances for all three assets, the payout counter and the event log
were re-read and were unchanged. The unpause and pause calls around the paused
case were themselves real transactions.

On-chain settlement detail. The events of both payout runs show the venue's pull
exactly as the authorization fix intends: XLM moves sender → FlowRoute, then
FlowRoute → the pair the venue resolves (`CDVAIOYHCD4RUSLQNVFI7RIZBFT2JZMJWM4RTOLQZQXL4QAVXU5RFKDB`
for USDC, `CDH4NEG6TAII2AXGJY52WSMMOGCPMFIQBBH245ATW2TIZ7MBYM23YOAR` for XTAR),
then the destination token moves pair → FlowRoute → recipient, followed by the
`SoroswapPair` and `SoroswapRouter` events and this contract's own `payout` and
`batch` events. Because the batch pays two different destination assets, it also
shows the per-destination pair resolution working: two resolutions, six
per-recipient authorizations. RPC's event log for the contract records 7
`payout` events and 2 `batch` events — one batch per run, one payout per
recipient — and none from the rejected calls.

Resources measured on that real transaction, not in the test environment:

| Quantity | 6 recipients | Network limit |
| --- | --- | --- |
| CPU instructions | 29,851,229 | 400,000,000 |
| Ledger footprint keys | 20 | 400 |
| Bytes written | 3,636 | 132,096 |
| Contract events | 10,384 B across 46 events | 16,384 B |

For comparison the same reconstruction gives 2,244 B across 11 events for the
single-recipient run. The event dimension is the one that bounds the batch, and
the live figure confirms the calibration's margin: the maximum batch uses about
63% of the event budget. The live instruction count is far above the
test-environment calibration of about 5.2M because the live run also executes
the real pair contract, which the calibration venue does not.

Two limits on this evidence. The four rejected cases were verified by RPC
simulation against live ledger state: that executes the deployed contract and
returns the contract error but commits nothing, so they have no failed on-chain
transaction of their own. And the ceiling itself is only exercised here at 6
recipients — the committed calibration test, which pushes past the ceiling, runs
in the Soroban test environment rather than on the network.

## Existing contract addresses (testnet)

| Contract | Address |
| --- | --- |
| FlowRoute Router (this release, verified live) | `CBB3UVMGMFVWLF6ZVMQYRQDWOXZUWNW4SD6SERG3RMLFXMWLZNOZ767U` |
| FlowRoute Router (prior interface) | `CBDWWJOW25KPUID432RZXFIPLHRYZY5KIXBT7FMC2L6LHFOITBMUX5LE` |
| External Soroswap Router | `CCJUD55AG6W5HAI5LRVNKAE5WDP5XGZBUDS5WNTIVDU7O264UZZE7BRD` |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow, commit conventions, and how to open a pull request.

## Maintainers

| Maintainer | Contact |
| --- | --- |
| Hollujay | [GitHub](https://github.com/Hollujay) |
