# FlowRoute Contract

FlowRoute is a batched, slippage-protected FX payout router on Stellar. A business
funds a single payout run, and the router contract swaps the source asset into each
recipient's chosen destination asset through the Soroswap Router, enforces a
per-recipient minimum-received floor, records an auditable event per payout, and
continues past any single recipient's failure.

This repository holds the contract layer. The application layer lives in the sibling
repository `flowroute-app`. Full documentation is a later phase.

See the git history and `contracts/router/src` for the current contract.
