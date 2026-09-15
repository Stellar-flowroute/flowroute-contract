//! Swap venue client.
//!
//! MVP decision: FlowRoute targets the Soroswap Router directly (one
//! liquidity source) rather than the Soroswap Aggregator (multi-protocol
//! split). The tradeoff is documented in the commit body. The verified
//! Aggregator interface is recorded below as a labeled follow-up.

use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contractclient, symbol_short, vec, Address, Env, IntoVal, Vec,
};

use crate::error::Error;

/// The router rejects a swap whose deadline has passed. Every swap gets a
/// fixed window starting from the current ledger timestamp.
pub const DEADLINE_BUFFER_SECONDS: u64 = 300;

// Client for the Soroswap Router contract. Signatures verified this session
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
//   fn router_pair_for(
//       e: Env,
//       token_a: Address,
//       token_b: Address,
//   ) -> Result<Address, CombinedRouterError>
//
// Fund flow: the router does not pull via allowance. It calls
// to.require_auth() and transfers the input tokens from `to` directly to the
// first pair contract. FlowRoute therefore passes itself as `to`, measures
// its own destination-token balance delta after the call, and only forwards
// the received amount to the recipient when it satisfies the per-recipient
// floor. A non-conforming successful response below that floor is rejected by
// the calling contract.
//
// Authorization: that pull is a `transfer` invocation of the source token whose
// invoker is the router, not this contract, so the source token's
// `from.require_auth()` is not covered by the rule that lets a contract move
// its own tokens. FlowRoute has to declare it, and the declaration names the
// exact invocation: the source token, `transfer`, from this contract, to the
// pair the router will transfer into, for the recipient's allocation. The
// destination comes from the venue's own `router_pair_for`, which is the
// resolution the router itself performs before its transfer
// (`soroswap_library::pair_for` over the router's configured factory:
// sha256 of the sorted token pair, deployed from the factory address). Reading
// it from the venue rather than re-deriving it keeps the authorized address and
// the address the router actually uses the same by construction. A pull that
// targets anything else is not authorized and fails closed: the swap reverts
// and the recipient is refunded like any other venue failure.
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

    /// The address of the pair the router transfers `token_a` into for the
    /// first hop of a `[token_a, token_b]` swap.
    fn router_pair_for(env: Env, token_a: Address, token_b: Address) -> Result<Address, Error>;
}

/// Resolves the pair the venue pulls `token_in` into for a single hop.
///
/// Any failure (an unreachable or misconfigured venue, or a pair whose two ends
/// are the same asset) is reported as `SwapFailed` so that the caller refunds
/// that recipient and never authorizes a destination the venue did not resolve.
pub fn resolve_pair(
    env: &Env,
    swap_router: &Address,
    token_in: &Address,
    token_out: &Address,
) -> Result<Address, Error> {
    let client = SoroswapRouterClient::new(env, swap_router);
    match client.try_router_pair_for(token_in, token_out) {
        Ok(Ok(pair)) => Ok(pair),
        _ => Err(Error::SwapFailed),
    }
}

/// Resolves the pair for `token_in`/`token_out`, reusing a resolution already
/// made in this batch for the same destination asset.
///
/// The pair is a function of the venue's factory and the two tokens, so a run
/// that pays several recipients in the same destination asset only has to ask
/// the venue once; the authorization itself is still issued per recipient,
/// because it names that recipient's allocation.
fn resolve_pair_cached(
    env: &Env,
    swap_router: &Address,
    token_in: &Address,
    token_out: &Address,
    resolved: &mut Vec<(Address, Address)>,
) -> Result<Address, Error> {
    for (dest_asset, pair) in resolved.iter() {
        if dest_asset == *token_out {
            return Ok(pair);
        }
    }
    let pair = resolve_pair(env, swap_router, token_in, token_out)?;
    resolved.push_back((token_out.clone(), pair.clone()));
    Ok(pair)
}

/// Declares the venue's pull of this contract's source tokens as an invocation
/// this contract authorizes.
///
/// The entry is the exact call the router makes. It is scoped to one
/// recipient's swap: the tracker it installs matches that invocation once, with
/// those arguments, so it cannot be replayed for a different amount, a
/// different destination, or a second time within the same batch.
fn authorize_source_pull(
    env: &Env,
    token_in: &Address,
    from: &Address,
    pair: &Address,
    amount_in: i128,
) {
    env.authorize_as_current_contract(vec![
        env,
        InvokerContractAuthEntry::Contract(SubContractInvocation {
            context: ContractContext {
                contract: token_in.clone(),
                fn_name: symbol_short!("transfer"),
                args: (from.clone(), pair.clone(), amount_in).into_val(env),
            },
            sub_invocations: vec![env],
        }),
    ]);
}

/// Executes one recipient's swap on the venue.
///
/// Before the venue runs, this authorizes the venue's source-token pull for
/// exactly `amount_in` into the resolved pair. The venue enforces
/// amount_out_min internally and reverts atomically on any failure, so on an
/// error the recipient's source amount remains in this contract and can be
/// refunded to the sender.
pub fn swap(
    env: &Env,
    swap_router: &Address,
    amount_in: i128,
    amount_out_min: i128,
    path: Vec<Address>,
    to: Address,
    resolved_pairs: &mut Vec<(Address, Address)>,
) -> Result<Vec<i128>, Error> {
    let token_in = path.get(0).ok_or(Error::SwapFailed)?;
    let token_out = path.get(1).ok_or(Error::SwapFailed)?;

    let pair = resolve_pair_cached(env, swap_router, &token_in, &token_out, resolved_pairs)?;
    authorize_source_pull(env, &token_in, &to, &pair, amount_in);

    let client = SoroswapRouterClient::new(env, swap_router);
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
