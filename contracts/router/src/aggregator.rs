//! Swap venue client.
//!
//! MVP decision: FlowRoute targets the Soroswap Router directly (one
//! liquidity source) rather than the Soroswap Aggregator (multi-protocol
//! split). The tradeoff is documented in the commit body. The verified
//! Aggregator interface is recorded below as a labeled follow-up.

use soroban_sdk::{contractclient, Address, Env, Vec};

use crate::error::Error;

/// Deployed Soroswap Router on Stellar testnet. Verified this session from
/// github.com/soroswap/core, file public/testnet.contracts.json, ids.router.
/// No admin function may change the venue in the MVP, so the address is a
/// constant. Change this constant when deploying against another network.
pub const SOROSWAP_ROUTER_TESTNET: &str =
    "CCJUD55AG6W5HAI5LRVNKAE5WDP5XGZBUDS5WNTIVDU7O264UZZE7BRD";

/// The router rejects a swap whose deadline has passed. Every swap gets a
/// fixed window starting from the current ledger timestamp.
pub const DEADLINE_BUFFER_SECONDS: u64 = 300;

// Client for the Soroswap Router contract. Signature verified this session
// against github.com/soroswap/core, contracts/router/src/lib.rs:
//
//   fn swap_exact_tokens_for_tokens(
//       e: Env,
//       amount_in: i128,
//       amount_out_min: i128,
//       path: Vec<Address>,
//       to: Address,
//       deadline: u64,
//   ) -> Result<Vec<i128>, CombinedRouterError>
//
// Fund flow: the router does not pull via allowance. It calls
// to.require_auth() and transfers the input tokens from `to` directly to the
// first pair contract. FlowRoute therefore passes itself as `to`, measures
// its own destination-token balance delta after the call, enforces the
// per-recipient floor on that delta, and forwards the received amount to the
// recipient.
//
// Aggregator follow-up, verified interface from github.com/soroswap/aggregator,
// contracts/aggregator/src/lib.rs and contracts/aggregator/src/models.rs:
//
//   fn swap_exact_tokens_for_tokens(
//       env: Env,
//       token_in: Address,
//       token_out: Address,
//       amount_in: i128,
//       amount_out_min: i128,
//       distribution: Vec<DexDistribution>,
//       to: Address,
//       deadline: u64,
//   ) -> Result<Vec<Vec<i128>>, AggregatorError>
//
//   DexDistribution { protocol_id: Protocol, path: Vec<Address>, parts: u32,
//   bytes: Option<Vec<BytesN<32>>> }, where Protocol is Soroswap, Phoenix,
//   Aqua or Comet.
//
//   The aggregator enforces amount_out_min on the total delivered to `to` and
//   reverts the whole call otherwise, so failures stay atomic and refundable,
//   exactly as with the router.
#[contractclient(name = "SoroswapRouterClient")]
pub trait RouterInterface {
    fn swap_exact_tokens_for_tokens(
        env: Env,
        amount_in: i128,
        amount_out_min: i128,
        path: Vec<Address>,
        to: Address,
        deadline: u64,
    ) -> Result<Vec<i128>, Error>;
}

pub fn venue_address(env: &Env) -> Address {
    Address::from_str(env, SOROSWAP_ROUTER_TESTNET)
}

/// Executes one recipient's swap on the venue. The venue enforces
/// amount_out_min internally and reverts atomically on any failure, so on an
/// error the recipient's source amount remains in this contract and can be
/// refunded to the sender.
pub fn swap(
    env: &Env,
    amount_in: i128,
    amount_out_min: i128,
    path: Vec<Address>,
    to: Address,
) -> Result<Vec<i128>, Error> {
    let client = SoroswapRouterClient::new(env, &venue_address(env));
    let deadline = env.ledger().timestamp() + DEADLINE_BUFFER_SECONDS;
    match client.try_swap_exact_tokens_for_tokens(
        &amount_in,
        &amount_out_min,
        &path,
        &to,
        &deadline,
    ) {
        Ok(Ok(amounts)) => Ok(amounts),
        _ => Err(Error::SwapFailed),
    }
}
