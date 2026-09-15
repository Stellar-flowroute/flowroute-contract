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

This API change requires a fresh FlowRoute Router deployment. The existing
testnet FlowRoute Router listed below was initialized with the prior interface
and is not changed by this release.

## Key Features

- **Batched payouts.** One transaction funds any number of recipients. The full source amount is pulled from the sender once and distributed in the same call.
- **Multi-currency delivery.** Each recipient names a destination asset, and the router converts the source asset through the Soroswap Router.
- **On-chain slippage floor.** Every recipient sets a minimum received amount (`dest_min`). FlowRoute measures the per-swap balance delta itself: an under-floor venue response is a failed payout, and any received destination tokens are returned to the sender rather than retained by the contract.
- **Auditable settlement.** Each payout run and every per-recipient result is emitted as an on-chain event, and a payout counter records how many runs have executed.
- **Failure isolation.** One recipient failing never aborts the batch. A swap that reverts is refunded to the sender at the end of the run.
- **Pause switch.** The admin can pause execution between runs.

## Architecture

The router is a single Soroban contract in `contracts/router`. Storage holds the admin address, an immutable external Soroswap Router address, a paused flag, and a payout counter. Swaps are delegated to that configured venue. The public surface is four functions:

- `initialize(admin, swap_router)` sets the admin and immutable Soroswap Router venue, clears the paused flag, and resets the payout counter. It requires authorization from the supplied admin.
- `set_paused(paused)` pauses or unpauses the batch executor. Requires admin auth.
- `get_payout_count()` returns the number of payout runs executed so far.
- `execute_batch(sender, source_asset, recipients, total_source_amount)` executes one payout run. It validates the batch, pulls the total amount from the sender, swaps each recipient's allocation on the venue with the recipient's `dest_min` enforced as the output floor, emits per-recipient and per-run events, refunds failed swaps to the sender, and never aborts on a single failure.

The application layer lives in the sibling repository `flowroute-app`.

## Existing contract addresses (testnet)

| Contract | Address |
| --- | --- |
| FlowRoute Router (prior interface) | `CBDWWJOW25KPUID432RZXFIPLHRYZY5KIXBT7FMC2L6LHFOITBMUX5LE` |
| External Soroswap Router | `CCJUD55AG6W5HAI5LRVNKAE5WDP5XGZBUDS5WNTIVDU7O264UZZE7BRD` |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow, commit conventions, and how to open a pull request.

## Maintainers

| Maintainer | Contact |
| --- | --- |
| Hollujay | [GitHub](https://github.com/Hollujay) |
