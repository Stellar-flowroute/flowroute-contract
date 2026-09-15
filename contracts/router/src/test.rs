extern crate std;

use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events, MockAuth, MockAuthInvoke},
    token::{StellarAssetClient, TokenClient},
    vec,
    xdr::{self, ToXdr},
    Address, Bytes, Env, IntoVal, Symbol, Vec,
};

use crate::{storage, Error, PayoutResult, Recipient, Router, RouterClient, MAX_BATCH_RECIPIENTS};

fn setup_client(env: &Env) -> (Address, Address, RouterClient<'_>) {
    let contract_id = env.register(Router, ());
    let client = RouterClient::new(env, &contract_id);
    let admin = Address::generate(env);
    (contract_id, admin, client)
}

fn configured_swap_router(env: &Env, contract_id: &Address) -> Option<Address> {
    env.as_contract(contract_id, || storage::read_swap_router(env))
}

/// Derives a pair address the way the verified venue derives it
/// (`soroswap_library::pair_for`): the salt is the SHA-256 of the sorted token
/// pair's XDR and the address is that salt's deterministic deploy address from
/// the factory. The mocked venues resolve pairs with it and the tests deploy
/// the pair at the address it returns, so no pair address is hardcoded and the
/// resolve-then-authorize-then-pull chain is exercised end to end.
fn derive_pair_address(
    env: &Env,
    factory: &Address,
    token_a: &Address,
    token_b: &Address,
) -> Address {
    let (token_0, token_1) = if token_a < token_b {
        (token_a.clone(), token_b.clone())
    } else {
        (token_b.clone(), token_a.clone())
    };
    let mut salt_bytes = Bytes::new(env);
    salt_bytes.append(&token_0.to_xdr(env));
    salt_bytes.append(&token_1.to_xdr(env));
    let salt = env.crypto().sha256(&salt_bytes);
    env.deployer()
        .with_address(factory.clone(), salt)
        .deployed_address()
}

/// Minimal pair contract, deployed at the deterministic pair address. It is a
/// live contract that receives the input the router pulls, which is what the
/// pull needs: the token transfer the router performs is only authorized for
/// this address. Its `swap` mirrors the verified pair's `swap(amount_0_out,
/// amount_1_out, to)`: the pair delivers the output to `to` itself, keeps two
/// reserve entries the way the real pair's update does, and publishes the pair
/// events.
///
/// Only the authorization tests drive the pair, through `pair_invoking_router`.
/// The venue the calibration and the functional tests use delivers inline
/// instead: the test environment meters a fixed internal budget per contract
/// invocation, and the pair's two extra invocations per recipient would push
/// the largest batches those tests exercise over that test-only budget. The
/// pair's own work is therefore measured separately, and the calibration
/// (whose binding dimension is the event budget, which both models reproduce
/// exactly) states what it leaves out.
mod mock_pair {
    use soroban_sdk::{
        contract, contractimpl, contracttype, symbol_short, token::TokenClient, Address, Env,
        MuxedAddress,
    };

    use crate::error::Error;

    #[contracttype]
    pub struct PairSwapEvent {
        pub to: Address,
        pub amount_0_in: i128,
        pub amount_1_in: i128,
        pub amount_0_out: i128,
        pub amount_1_out: i128,
    }

    #[contracttype]
    pub struct PairSyncEvent {
        pub new_reserve_0: i128,
        pub new_reserve_1: i128,
    }

    #[contract]
    pub struct MockPair;

    #[contractimpl]
    impl MockPair {
        pub fn initialize(env: Env, token: Address) {
            env.storage()
                .instance()
                .set(&symbol_short!("token"), &token);
            env.storage().instance().set(&symbol_short!("res0"), &0i128);
            env.storage().instance().set(&symbol_short!("res1"), &0i128);
        }

        /// Mirrors `SoroswapPair::swap(amount_0_out, amount_1_out, to)`.
        pub fn swap(
            env: Env,
            amount_0_out: i128,
            amount_1_out: i128,
            to: Address,
        ) -> Result<(), Error> {
            let self_address = env.current_contract_address();
            let token: Address = env
                .storage()
                .instance()
                .get(&symbol_short!("token"))
                .unwrap();
            let reserve_0: i128 = env
                .storage()
                .instance()
                .get(&symbol_short!("res0"))
                .unwrap();
            let client = TokenClient::new(&env, &token);

            let amount_out = amount_0_out.checked_add(amount_1_out).unwrap_or(0);
            if amount_out > 0 {
                client.transfer(&self_address, &MuxedAddress::from(&to), &amount_out);
            }

            let reserve_1 = client.balance(&self_address);
            env.storage().instance().set(
                &symbol_short!("res0"),
                &reserve_0.saturating_add(amount_out),
            );
            env.storage()
                .instance()
                .set(&symbol_short!("res1"), &reserve_1);

            #[allow(deprecated)]
            env.events().publish(
                ("SoroswapPair", symbol_short!("swap")),
                PairSwapEvent {
                    to: to.clone(),
                    amount_0_in: 0,
                    amount_1_in: amount_out,
                    amount_0_out,
                    amount_1_out,
                },
            );
            #[allow(deprecated)]
            env.events().publish(
                ("SoroswapPair", symbol_short!("sync")),
                PairSyncEvent {
                    new_reserve_0: reserve_0,
                    new_reserve_1: reserve_1,
                },
            );

            Ok(())
        }
    }
}

/// Mock swap venue. Registered at the address supplied to initialize, so the
/// contract calls this mock exactly as it would call the real Soroswap Router.
/// It mirrors the verified router for a single hop: it resolves the pair the
/// way the router does internally, requires auth from `to`, transfers the
/// input from `to` into that pair, and lets the pair deliver the output to
/// `to` at a fixed 1:1 rate. A requested floor above the deliverable amount
/// reverts with SlippageExceeded, mirroring the real router's atomic revert.
mod mock_router {
    use soroban_sdk::{
        contract, contractimpl, panic_with_error, token::TokenClient, vec, Address, Env,
        MuxedAddress, Vec,
    };

    use super::derive_pair_address;
    use crate::error::Error;

    /// The pair for two tokens, derived the way `soroswap_library::pair_for`
    /// derives it with this venue as the factory base. Two identical tokens
    /// cannot form a pair, matching the library's SortIdenticalTokens error.
    fn pair_for(env: &Env, token_a: &Address, token_b: &Address) -> Result<Address, Error> {
        if token_a == token_b {
            return Err(Error::SwapFailed);
        }
        Ok(derive_pair_address(
            env,
            &env.current_contract_address(),
            token_a,
            token_b,
        ))
    }

    #[contract]
    pub struct MockRouter;

    #[contractimpl]
    impl MockRouter {
        pub fn router_pair_for(
            env: Env,
            token_a: Address,
            token_b: Address,
        ) -> Result<Address, Error> {
            pair_for(&env, &token_a, &token_b)
        }

        pub fn swap_exact_tokens_for_tokens(
            env: Env,
            amount_in: i128,
            amount_out_min: i128,
            path: Vec<Address>,
            to: Address,
            _deadline: u64,
        ) -> Result<Vec<i128>, Error> {
            let input = path.get(0).unwrap();
            let output = path.get(1).unwrap();
            let pair = pair_for(&env, &input, &output)?;
            let self_address = env.current_contract_address();

            to.require_auth();

            let amount_out = amount_in;
            if amount_out < amount_out_min {
                panic_with_error!(env, Error::SlippageExceeded);
            }

            // Mirror the real router fund flow: pull the input out of `to` and
            // into the first pair, then deliver the output to `to`.
            TokenClient::new(&env, &input).transfer(&to, &MuxedAddress::from(&pair), &amount_in);
            TokenClient::new(&env, &output).transfer(
                &self_address,
                &MuxedAddress::from(&to),
                &amount_out,
            );

            Ok(vec![&env, amount_in, amount_out])
        }
    }
}

/// Malicious venue that consumes the full source input, deliberately sends a
/// non-zero amount below amount_out_min, and still reports success.
mod under_delivering_router {
    use soroban_sdk::{
        contract, contractimpl, token::TokenClient, vec, Address, Env, MuxedAddress, Vec,
    };

    use super::derive_pair_address;
    use crate::error::Error;

    fn pair_for(env: &Env, token_a: &Address, token_b: &Address) -> Result<Address, Error> {
        if token_a == token_b {
            return Err(Error::SwapFailed);
        }
        Ok(derive_pair_address(
            env,
            &env.current_contract_address(),
            token_a,
            token_b,
        ))
    }

    #[contract]
    pub struct UnderDeliveringRouter;

    #[contractimpl]
    impl UnderDeliveringRouter {
        pub fn router_pair_for(
            env: Env,
            token_a: Address,
            token_b: Address,
        ) -> Result<Address, Error> {
            pair_for(&env, &token_a, &token_b)
        }

        pub fn swap_exact_tokens_for_tokens(
            env: Env,
            amount_in: i128,
            amount_out_min: i128,
            path: Vec<Address>,
            to: Address,
            _deadline: u64,
        ) -> Result<Vec<i128>, Error> {
            let input = path.get(0).unwrap();
            let output = path.get(1).unwrap();
            let pair = pair_for(&env, &input, &output)?;
            let self_address = env.current_contract_address();
            let amount_out = if amount_out_min <= amount_in {
                amount_in
            } else {
                amount_out_min - 1
            };

            to.require_auth();
            TokenClient::new(&env, &input).transfer(&to, &MuxedAddress::from(&pair), &amount_in);
            if amount_out > 0 {
                TokenClient::new(&env, &output).transfer(
                    &self_address,
                    &MuxedAddress::from(&to),
                    &amount_out,
                );
            }

            Ok(vec![&env, amount_in, amount_out])
        }
    }
}

/// Calibration venue. It mirrors `mock_router`'s fund flow and event output for
/// a single-hop swap, so a calibration run sees the footprint the deployed
/// venue produces. Shapes verified against github.com/soroswap/core:
///
///   router swap_exact_tokens_for_tokens -> topics ("SoroswapRouter", "swap")
///       SwapEvent { path: Vec<Address>, amounts: Vec<i128>, to: Address }
///   pair swap -> topics ("SoroswapPair", "swap")
///       SwapEvent { to, amount_0_in, amount_1_in, amount_0_out, amount_1_out }
///   pair update, called at the end of pair swap -> topics ("SoroswapPair",
///   "sync") SyncEvent { new_reserve_0, new_reserve_1 }
///
/// The pair's two events are published by the pair contract itself
/// (`mock_pair`), exactly as the deployed pair publishes them, so the measured
/// footprint includes the extra contract frame.
///
/// The deprecated `events().publish` entry point is used deliberately: the
/// macro-generated alternative cannot publish the 12-character component topic
/// names the venue uses.
mod soroswap_shaped_router {
    use soroban_sdk::{
        contract, contractimpl, contracttype, panic_with_error, symbol_short, token::TokenClient,
        vec, Address, Env, MuxedAddress, Vec,
    };

    use super::derive_pair_address;
    use crate::error::Error;

    #[contracttype]
    pub struct RouterSwapEvent {
        pub path: Vec<Address>,
        pub amounts: Vec<i128>,
        pub to: Address,
    }

    #[contracttype]
    pub struct PairSwapEvent {
        pub to: Address,
        pub amount_0_in: i128,
        pub amount_1_in: i128,
        pub amount_0_out: i128,
        pub amount_1_out: i128,
    }

    #[contracttype]
    pub struct PairSyncEvent {
        pub new_reserve_0: i128,
        pub new_reserve_1: i128,
    }

    fn pair_for(env: &Env, token_a: &Address, token_b: &Address) -> Result<Address, Error> {
        if token_a == token_b {
            return Err(Error::SwapFailed);
        }
        Ok(derive_pair_address(
            env,
            &env.current_contract_address(),
            token_a,
            token_b,
        ))
    }

    #[contract]
    pub struct SoroswapShapedRouter;

    #[contractimpl]
    impl SoroswapShapedRouter {
        pub fn router_pair_for(
            env: Env,
            token_a: Address,
            token_b: Address,
        ) -> Result<Address, Error> {
            pair_for(&env, &token_a, &token_b)
        }

        pub fn swap_exact_tokens_for_tokens(
            env: Env,
            amount_in: i128,
            amount_out_min: i128,
            path: Vec<Address>,
            to: Address,
            _deadline: u64,
        ) -> Result<Vec<i128>, Error> {
            let input = path.get(0).unwrap();
            let output = path.get(1).unwrap();
            let pair = pair_for(&env, &input, &output)?;
            let self_address = env.current_contract_address();

            to.require_auth();

            let amount_out = amount_in;
            if amount_out < amount_out_min {
                panic_with_error!(env, Error::SlippageExceeded);
            }

            TokenClient::new(&env, &input).transfer(&to, &MuxedAddress::from(&pair), &amount_in);
            TokenClient::new(&env, &output).transfer(
                &self_address,
                &MuxedAddress::from(&to),
                &amount_out,
            );

            #[allow(deprecated)]
            env.events().publish(
                ("SoroswapRouter", symbol_short!("swap")),
                RouterSwapEvent {
                    path,
                    amounts: vec![&env, amount_in, amount_out],
                    to: to.clone(),
                },
            );
            #[allow(deprecated)]
            env.events().publish(
                ("SoroswapPair", symbol_short!("swap")),
                PairSwapEvent {
                    to: to.clone(),
                    amount_0_in: amount_in,
                    amount_1_in: 0,
                    amount_0_out: 0,
                    amount_1_out: amount_out,
                },
            );
            #[allow(deprecated)]
            env.events().publish(
                ("SoroswapPair", symbol_short!("sync")),
                PairSyncEvent {
                    new_reserve_0: amount_in,
                    new_reserve_1: amount_out,
                },
            );

            Ok(vec![&env, amount_in, amount_out])
        }
    }
}

/// Venue that resolves the honest pair but pulls the input somewhere else, to
/// show that this contract authorizes only the pair the venue resolves.
mod misrouting_router {
    use soroban_sdk::{
        contract, contractimpl, panic_with_error, symbol_short, token::TokenClient, vec, Address,
        Env, MuxedAddress, Vec,
    };

    use super::derive_pair_address;
    use crate::error::Error;

    fn pair_for(env: &Env, token_a: &Address, token_b: &Address) -> Result<Address, Error> {
        if token_a == token_b {
            return Err(Error::SwapFailed);
        }
        Ok(derive_pair_address(
            env,
            &env.current_contract_address(),
            token_a,
            token_b,
        ))
    }

    #[contract]
    pub struct MisroutingRouter;

    #[contractimpl]
    impl MisroutingRouter {
        pub fn set_pull_target(env: Env, target: Address) {
            env.storage()
                .instance()
                .set(&symbol_short!("target"), &target);
        }

        pub fn router_pair_for(
            env: Env,
            token_a: Address,
            token_b: Address,
        ) -> Result<Address, Error> {
            pair_for(&env, &token_a, &token_b)
        }

        pub fn swap_exact_tokens_for_tokens(
            env: Env,
            amount_in: i128,
            amount_out_min: i128,
            path: Vec<Address>,
            to: Address,
            _deadline: u64,
        ) -> Result<Vec<i128>, Error> {
            let input = path.get(0).unwrap();
            let output = path.get(1).unwrap();
            let self_address = env.current_contract_address();
            let target: Address = env
                .storage()
                .instance()
                .get(&symbol_short!("target"))
                .unwrap();

            to.require_auth();

            let amount_out = amount_in;
            if amount_out < amount_out_min {
                panic_with_error!(env, Error::SlippageExceeded);
            }

            // Pulls into `target` instead of the pair it just resolved.
            TokenClient::new(&env, &input).transfer(&to, &MuxedAddress::from(&target), &amount_in);
            TokenClient::new(&env, &output).transfer(
                &self_address,
                &MuxedAddress::from(&to),
                &amount_out,
            );

            Ok(vec![&env, amount_in, amount_out])
        }
    }
}

/// Fully faithful venue, used by the authorization tests. It mirrors the
/// verified router for a single hop end to end: it resolves the pair, requires
/// auth from `to`, pulls the input from `to` into that pair, and then calls the
/// pair, which delivers the output to `to`. The pair call is what makes a
/// missing or undeployed pair fail that recipient instead of silently paying
/// them, exactly as the deployed venue behaves.
mod pair_invoking_router {
    use soroban_sdk::{
        contract, contractimpl, contracttype, panic_with_error, symbol_short, token::TokenClient,
        vec, Address, Env, MuxedAddress, Vec,
    };

    use super::{derive_pair_address, mock_pair};
    use crate::error::Error;

    #[contracttype]
    pub struct RouterSwapEvent {
        pub path: Vec<Address>,
        pub amounts: Vec<i128>,
        pub to: Address,
    }

    fn pair_for(env: &Env, token_a: &Address, token_b: &Address) -> Result<Address, Error> {
        if token_a == token_b {
            return Err(Error::SwapFailed);
        }
        Ok(derive_pair_address(
            env,
            &env.current_contract_address(),
            token_a,
            token_b,
        ))
    }

    #[contract]
    pub struct PairInvokingRouter;

    #[contractimpl]
    impl PairInvokingRouter {
        pub fn router_pair_for(
            env: Env,
            token_a: Address,
            token_b: Address,
        ) -> Result<Address, Error> {
            pair_for(&env, &token_a, &token_b)
        }

        pub fn swap_exact_tokens_for_tokens(
            env: Env,
            amount_in: i128,
            amount_out_min: i128,
            path: Vec<Address>,
            to: Address,
            _deadline: u64,
        ) -> Result<Vec<i128>, Error> {
            let input = path.get(0).unwrap();
            let pair = pair_for(&env, &input, &path.get(1).unwrap())?;

            to.require_auth();

            let amount_out = amount_in;
            if amount_out < amount_out_min {
                panic_with_error!(env, Error::SlippageExceeded);
            }

            TokenClient::new(&env, &input).transfer(&to, &MuxedAddress::from(&pair), &amount_in);
            mock_pair::MockPairClient::new(&env, &pair).swap(&0, &amount_out, &to);

            #[allow(deprecated)]
            env.events().publish(
                ("SoroswapRouter", symbol_short!("swap")),
                RouterSwapEvent {
                    path,
                    amounts: vec![&env, amount_in, amount_out],
                    to: to.clone(),
                },
            );

            Ok(vec![&env, amount_in, amount_out])
        }
    }
}

struct BatchSetup {
    env: Env,
    contract_id: Address,
    sender: Address,
    source: Address,
    dest: Address,
    swap_router: Address,
    /// The pair contract for `source`/`dest`, deployed at the address the
    /// venue resolves and funded with destination liquidity.
    pair: Address,
}

/// Deploys the mock pair at the address `venue` resolves for `source`/`dest`,
/// so the router's pull has a live pair contract to transfer the input into.
fn deploy_venue_pair(env: &Env, venue: &Address, source: &Address, dest: &Address) -> Address {
    let pair = derive_pair_address(env, venue, source, dest);
    let pair_id = env.register_at(&pair, mock_pair::MockPair, ());
    mock_pair::MockPairClient::new(env, &pair_id).initialize(dest);
    pair_id
}

/// Initializes the contract with a freshly registered venue, deploys and funds
/// the pair that venue resolves, and funds the sender with source tokens. All
/// auths are mocked here; tests that exercise the shipped authorization path
/// re-arm the environment with only the sender's invocation tree.
fn setup_with_venue(register_venue: impl FnOnce(&Env) -> Address) -> BatchSetup {
    let env = Env::default();
    // The venue (and the tokens it touches on this contract's behalf) calls
    // require_auth on this contract at a depth beyond the direct invoker, so
    // non-root authorizations must be allowed in recording mode.
    env.mock_all_auths_allowing_non_root_auth();

    let (contract_id, admin, _client) = setup_client(&env);
    let client = RouterClient::new(&env, &contract_id);
    let swap_router = register_venue(&env);
    client.initialize(&admin, &swap_router);

    let token_admin = Address::generate(&env);
    let source = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let dest = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    let pair = deploy_venue_pair(&env, &swap_router, &source, &dest);

    let sender = Address::generate(&env);
    StellarAssetClient::new(&env, &source).mint(&sender, &1_000_000);
    StellarAssetClient::new(&env, &dest).mint(&swap_router, &10_000_000);

    BatchSetup {
        env,
        contract_id,
        sender,
        source,
        dest,
        swap_router,
        pair,
    }
}

/// The default venue, mirroring the verified router for a single hop. It is
/// registered at a generated address rather than the former hardcoded testnet
/// address: a fallback to that retired configuration makes every
/// successful-payout test below fail because no venue exists there.
fn setup_batch() -> BatchSetup {
    setup_with_venue(|env| env.register(mock_router::MockRouter, ()))
}

fn setup_under_delivering_batch() -> BatchSetup {
    setup_with_venue(|env| env.register(under_delivering_router::UnderDeliveringRouter, ()))
}

/// Like `setup_batch`, but the configured venue is the Soroswap-shaped
/// calibration venue, so resource measurements include the venue's events.
fn setup_shaped_batch() -> BatchSetup {
    setup_with_venue(|env| env.register(soroswap_shaped_router::SoroswapShapedRouter, ()))
}

fn count_events(events: &soroban_sdk::testutils::ContractEvents, tag: Symbol) -> usize {
    let expected: xdr::ScVal = xdr::ScVal::from(tag);
    events
        .events()
        .iter()
        .filter(|event| {
            let xdr::ContractEventBody::V0(v0) = &event.body;
            v0.topics.len() >= 1 && v0.topics.get(0) == Some(&expected)
        })
        .count()
}

#[test]
fn initialize_sets_admin_and_swap_router() {
    let env = Env::default();
    let (contract_id, admin, client) = setup_client(&env);
    let swap_router = Address::generate(&env);
    client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "initialize",
                args: (&admin, &swap_router).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .initialize(&admin, &swap_router);

    // The configured venue remains available to later contract calls.
    assert_eq!(client.get_payout_count(), 0);
    assert_eq!(configured_swap_router(&env, &contract_id), Some(swap_router));
}

#[test]
fn initialize_requires_supplied_admin_auth() {
    let env = Env::default();
    let (contract_id, admin, client) = setup_client(&env);
    let other = Address::generate(&env);
    let swap_router = Address::generate(&env);

    assert!(client.try_initialize(&admin, &swap_router).is_err());
    assert_eq!(configured_swap_router(&env, &contract_id), None);

    // Authorization from an address other than the proposed admin cannot
    // initialize the contract.
    assert!(client
        .mock_auths(&[MockAuth {
            address: &other,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "initialize",
                args: (&admin, &swap_router).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .try_initialize(&admin, &swap_router)
        .is_err());
    assert_eq!(configured_swap_router(&env, &contract_id), None);

    client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "initialize",
                args: (&admin, &swap_router).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .initialize(&admin, &swap_router);
}

#[test]
fn router_cannot_be_replaced_after_initialization() {
    let env = Env::default();
    let (contract_id, admin, client) = setup_client(&env);
    let swap_router = Address::generate(&env);
    let replacement_router = Address::generate(&env);
    client.mock_all_auths().initialize(&admin, &swap_router);

    // Even the authorized admin cannot re-initialize to a replacement venue.
    assert!(client
        .mock_all_auths()
        .try_initialize(&admin, &replacement_router)
        .is_err());
    assert_eq!(configured_swap_router(&env, &contract_id), Some(swap_router));
}

#[test]
fn set_paused_requires_admin_auth() {
    let env = Env::default();
    let (_contract_id, admin, client) = setup_client(&env);
    let swap_router = Address::generate(&env);
    client.mock_all_auths().initialize(&admin, &swap_router);

    // No auth is mocked for the admin signature, so the call reverts.
    assert!(client.try_set_paused(&true).is_err());
}

#[test]
fn set_paused_admin_roundtrip() {
    let env = Env::default();
    env.mock_all_auths();
    let (_contract_id, admin, client) = setup_client(&env);
    client.initialize(&admin, &Address::generate(&env));

    client.set_paused(&true);
    client.set_paused(&false);
}

#[test]
fn set_paused_before_initialize_reverts() {
    let env = Env::default();
    env.mock_all_auths();
    let (_contract_id, _admin, client) = setup_client(&env);

    assert!(client.try_set_paused(&true).is_err());
}

#[test]
fn get_payout_count_starts_at_zero() {
    let env = Env::default();
    let (_contract_id, admin, client) = setup_client(&env);
    client
        .mock_all_auths()
        .initialize(&admin, &Address::generate(&env));

    assert_eq!(client.get_payout_count(), 0);
}

#[test]
fn execute_batch_before_initialize_reverts() {
    let env = Env::default();
    env.mock_all_auths();
    let (_contract_id, _admin, client) = setup_client(&env);

    let sender = Address::generate(&env);
    let source = Address::generate(&env);
    let recipients: Vec<Recipient> = vec![&env];
    assert!(client
        .try_execute_batch(&sender, &source, &recipients, &100)
        .is_err());
}

#[test]
fn execute_batch_reverts_when_paused() {
    let env = Env::default();
    env.mock_all_auths();
    let (_contract_id, admin, client) = setup_client(&env);
    client.initialize(&admin, &Address::generate(&env));
    client.set_paused(&true);

    let sender = Address::generate(&env);
    let source = Address::generate(&env);
    let recipients: Vec<Recipient> = vec![&env];
    assert!(client
        .try_execute_batch(&sender, &source, &recipients, &100)
        .is_err());
}

#[test]
fn execute_batch_reverts_on_empty_batch() {
    let env = Env::default();
    env.mock_all_auths();
    let (_contract_id, admin, client) = setup_client(&env);
    client.initialize(&admin, &Address::generate(&env));

    let sender = Address::generate(&env);
    let source = Address::generate(&env);
    let recipients: Vec<Recipient> = vec![&env];
    assert!(client
        .try_execute_batch(&sender, &source, &recipients, &100)
        .is_err());
}

#[test]
fn execute_batch_reverts_on_amount_mismatch() {
    let env = Env::default();
    env.mock_all_auths();
    let (_contract_id, admin, client) = setup_client(&env);
    client.initialize(&admin, &Address::generate(&env));

    let sender = Address::generate(&env);
    let source = Address::generate(&env);
    // Otherwise valid, so the rejections below come from the declared total
    // rather than from the per-recipient amount validation.
    let recipient = Recipient {
        address: Address::generate(&env),
        dest_asset: Address::generate(&env),
        dest_min: 100,
        amount_in: 100,
    };
    let recipients: Vec<Recipient> = vec![&env, recipient];

    // Allocated 100 but the declared total is 150.
    assert!(client
        .try_execute_batch(&sender, &source, &recipients, &150)
        .is_err());
    // Declared total of zero.
    assert!(client
        .try_execute_batch(&sender, &source, &recipients, &0)
        .is_err());
}

#[test]
fn execute_batch_above_dest_min_succeeds() {
    let setup = setup_batch();
    let env = setup.env;
    let client = RouterClient::new(&env, &setup.contract_id);

    // execute_batch reads the stored, explicitly initialized venue. setup_batch
    // intentionally has no mock at the old hardcoded testnet router address.
    assert_eq!(
        configured_swap_router(&env, &setup.contract_id),
        Some(setup.swap_router.clone())
    );

    let recipient_1 = Address::generate(&env);
    let recipient_2 = Address::generate(&env);
    let other_dest = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    StellarAssetClient::new(&env, &other_dest).mint(&setup.swap_router, &10_000_000);
    let recipients: Vec<Recipient> = vec![
        &env,
        Recipient {
            address: recipient_1.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 150,
            amount_in: 200,
        },
        Recipient {
            address: recipient_2.clone(),
            dest_asset: other_dest.clone(),
            dest_min: 50,
            amount_in: 100,
        },
    ];

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &300);

    // Capture the event stream right after the batch, before any further
    // invocations, so the assertions below see the batch events.
    let all_events = env.events().all();

    assert_eq!(results.len(), 2);
    assert_eq!(results.get(0).unwrap().success, true);
    assert_eq!(results.get(0).unwrap().amount_delivered, 200);
    assert_eq!(results.get(1).unwrap().success, true);
    assert_eq!(results.get(1).unwrap().amount_delivered, 100);

    // Recipients received their destination tokens.
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&recipient_1), 200);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    let other_dest_client = TokenClient::new(&env, &other_dest);
    assert_eq!(other_dest_client.balance(&recipient_2), 100);
    assert_eq!(other_dest_client.balance(&setup.contract_id), 0);

    // The sender was debited exactly the batch total; the contract holds no
    // leftover source.
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 999_700);
    assert_eq!(source_client.balance(&setup.contract_id), 0);

    // One payout event per recipient plus one batch summary.
    assert_eq!(count_events(&all_events, symbol_short!("payout")), 2);
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 1);

    // Lock the exact event shapes the indexer consumes. Payout topics are
    // (payout, payout_id, sender) and data is (recipient, source_asset,
    // dest_asset, amount_delivered, success); the batch event is (batch,
    // payout_id, sender) with data (recipient_count, success_count,
    // total_source_amount).
    let (payouts, batches) = spec_events(&all_events);
    assert_eq!(payouts.len(), 2);
    assert_eq!(batches.len(), 1);
    for payout in payouts.iter() {
        assert_eq!(payout.topics[0], xdr::ScVal::from(symbol_short!("payout")));
        assert_eq!(payout.topics[1], xdr::ScVal::U64(1));
        assert_eq!(payout.topics[2], xdr::ScVal::from(&setup.sender));
        assert_eq!(payout.data[1], xdr::ScVal::from(&setup.source));
        assert_eq!(payout.data[4], xdr::ScVal::Bool(true));
    }
    assert_eq!(payouts[0].data[0], xdr::ScVal::from(&recipient_1));
    assert_eq!(payouts[0].data[2], xdr::ScVal::from(&setup.dest));
    assert_eq!(payouts[0].data[3], scval_i128(200));
    assert_eq!(payouts[1].data[0], xdr::ScVal::from(&recipient_2));
    assert_eq!(payouts[1].data[2], xdr::ScVal::from(&other_dest));
    assert_eq!(payouts[1].data[3], scval_i128(100));
    let batch = &batches[0];
    assert_eq!(batch.topics[0], xdr::ScVal::from(symbol_short!("batch")));
    assert_eq!(batch.topics[1], xdr::ScVal::U64(1));
    assert_eq!(batch.topics[2], xdr::ScVal::from(&setup.sender));
    assert_eq!(batch.data[0], xdr::ScVal::U32(2));
    assert_eq!(batch.data[1], xdr::ScVal::U32(2));
    assert_eq!(batch.data[2], scval_i128(300));

    // The payout counter advanced.
    assert_eq!(client.get_payout_count(), 1);
    assert_eq!(
        configured_swap_router(&env, &setup.contract_id),
        Some(setup.swap_router.clone())
    );
}

#[test]
fn execute_batch_exact_dest_min_succeeds() {
    let setup = setup_batch();
    let env = setup.env;
    let client = RouterClient::new(&env, &setup.contract_id);
    let recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 100,
            amount_in: 100,
        },
    ];

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &100);

    assert_eq!(results.get(0).unwrap().success, true);
    assert_eq!(results.get(0).unwrap().amount_delivered, 100);
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&recipient), 100);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(TokenClient::new(&env, &setup.source).balance(&setup.contract_id), 0);
}

#[test]
fn under_delivering_success_reverts_atomically() {
    let setup = setup_under_delivering_batch();
    let env = setup.env;
    let client = RouterClient::new(&env, &setup.contract_id);
    let recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 200,
            amount_in: 100,
        },
    ];

    let results = client.try_execute_batch(&setup.sender, &setup.source, &recipients, &100);
    let all_events = env.events().all();

    // The malicious venue returned Ok after sending 199, but the invocation
    // reverts because that is below the requested floor of 200.
    assert!(results.is_err());
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&recipient), 0);
    assert_eq!(dest_client.balance(&setup.sender), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(TokenClient::new(&env, &setup.source).balance(&setup.sender), 1_000_000);
    assert_eq!(TokenClient::new(&env, &setup.source).balance(&setup.contract_id), 0);
    assert_eq!(dest_client.balance(&setup.swap_router), 10_000_000);

    let (payouts, _) = spec_events(&all_events);
    assert!(payouts.is_empty());
}

#[test]
fn under_delivery_rolls_back_earlier_successful_recipient() {
    let setup = setup_under_delivering_batch();
    let env = setup.env;
    let client = RouterClient::new(&env, &setup.contract_id);
    let first_recipient = Address::generate(&env);
    let second_recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: first_recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 50,
            amount_in: 100,
        },
        Recipient {
            address: second_recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 200,
            amount_in: 100,
        },
    ];

    // The first swap would succeed, but the second non-conforming response
    // aborts the outer invocation and rolls the first payout back as well.
    assert!(client
        .try_execute_batch(&setup.sender, &setup.source, &recipients, &200)
        .is_err());

    let source_client = TokenClient::new(&env, &setup.source);
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    assert_eq!(source_client.balance(&setup.pair), 0);
    assert_eq!(dest_client.balance(&first_recipient), 0);
    assert_eq!(dest_client.balance(&second_recipient), 0);
    assert_eq!(dest_client.balance(&setup.sender), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(dest_client.balance(&setup.swap_router), 10_000_000);
    assert!(spec_events(&env.events().all()).0.is_empty());
}

#[test]
fn preexisting_destination_balance_is_not_paid_to_recipient() {
    let setup = setup_batch();
    let env = setup.env;
    let client = RouterClient::new(&env, &setup.contract_id);
    let recipient = Address::generate(&env);
    StellarAssetClient::new(&env, &setup.dest).mint(&setup.contract_id, &500);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 100,
            amount_in: 100,
        },
    ];

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &100);

    assert_eq!(results.get(0).unwrap().success, true);
    assert_eq!(results.get(0).unwrap().amount_delivered, 100);
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&recipient), 100);
    assert_eq!(dest_client.balance(&setup.contract_id), 500);
}

/// Extracts the success flag (last data element) of every payout event.
fn payout_success_flags(events: &soroban_sdk::testutils::ContractEvents) -> std::vec::Vec<bool> {
    let tag: xdr::ScVal = xdr::ScVal::from(symbol_short!("payout"));
    let mut flags = std::vec::Vec::new();
    for event in events.events().iter() {
        let xdr::ContractEventBody::V0(v0) = &event.body;
        if v0.topics.len() >= 1 && v0.topics.get(0) == Some(&tag) {
            let data = match &v0.data {
                xdr::ScVal::Vec(Some(vec)) => vec,
                _ => continue,
            };
            match data.last() {
                Some(xdr::ScVal::Bool(flag)) => flags.push(*flag),
                _ => flags.push(false),
            }
        }
    }
    flags
}

/// A decoded event: topics and data payload as XDR values.
struct EventShape {
    topics: std::vec::Vec<xdr::ScVal>,
    data: std::vec::Vec<xdr::ScVal>,
}

/// Collects payout and batch events with their full topic/data layout, so
/// tests can lock the exact shapes the indexer consumes.
fn spec_events(
    events: &soroban_sdk::testutils::ContractEvents,
) -> (std::vec::Vec<EventShape>, std::vec::Vec<EventShape>) {
    let payout_tag: xdr::ScVal = xdr::ScVal::from(symbol_short!("payout"));
    let batch_tag: xdr::ScVal = xdr::ScVal::from(symbol_short!("batch"));
    let mut payouts = std::vec::Vec::new();
    let mut batches = std::vec::Vec::new();
    for event in events.events().iter() {
        let xdr::ContractEventBody::V0(v0) = &event.body;
        let topics = v0.topics.iter().cloned().collect();
        let data = match &v0.data {
            xdr::ScVal::Vec(Some(vec)) => vec.iter().cloned().collect(),
            _ => std::vec::Vec::new(),
        };
        let shape = EventShape { topics, data };
        if v0.topics.len() >= 1 && v0.topics.get(0) == Some(&payout_tag) {
            payouts.push(shape);
        } else if v0.topics.len() >= 1 && v0.topics.get(0) == Some(&batch_tag) {
            batches.push(shape);
        }
    }
    (payouts, batches)
}

fn scval_i128(value: i128) -> xdr::ScVal {
    xdr::ScVal::I128(xdr::Int128Parts {
        hi: (value >> 64) as i64,
        lo: value as u64,
    })
}

#[test]
fn execute_batch_failed_recipient_is_refunded() {
    let setup = setup_batch();
    let env = setup.env;
    let client = RouterClient::new(&env, &setup.contract_id);

    let failing_recipient = Address::generate(&env);
    let good_recipient = Address::generate(&env);
    let recipients: Vec<Recipient> = vec![
        &env,
        Recipient {
            address: failing_recipient.clone(),
            dest_asset: setup.dest.clone(),
            // Floor above the deliverable 100 at the 1:1 mock venue, so the
            // venue reverts with SlippageExceeded.
            dest_min: 200,
            amount_in: 100,
        },
        Recipient {
            address: good_recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 50,
            amount_in: 100,
        },
    ];

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &200);
    let all_events = env.events().all();

    // The failing recipient is marked failed with nothing delivered; the
    // other recipient succeeds, so one failure never aborts the batch.
    assert_eq!(results.len(), 2);
    assert_eq!(results.get(0).unwrap().success, false);
    assert_eq!(results.get(0).unwrap().amount_delivered, 0);
    assert_eq!(results.get(1).unwrap().success, true);
    assert_eq!(results.get(1).unwrap().amount_delivered, 100);

    // No destination tokens reached the failing recipient.
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&failing_recipient), 0);
    assert_eq!(dest_client.balance(&good_recipient), 100);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);

    // The failed recipient's source was refunded to the sender: 1_000_000
    // minted minus the 200 batch total plus the 100 refund.
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 999_900);
    assert_eq!(source_client.balance(&setup.contract_id), 0);

    // Events: one failed payout, one successful payout, one batch summary.
    assert_eq!(count_events(&all_events, symbol_short!("payout")), 2);
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 1);
    assert_eq!(payout_success_flags(&all_events), [false, true]);

    assert_eq!(client.get_payout_count(), 1);
}

#[test]
fn all_reverted_swaps_refund_all_source() {
    let setup = setup_batch();
    let env = setup.env;
    let client = RouterClient::new(&env, &setup.contract_id);
    let recipients = vec![
        &env,
        Recipient {
            address: Address::generate(&env),
            dest_asset: setup.dest.clone(),
            dest_min: 200,
            amount_in: 100,
        },
        Recipient {
            address: Address::generate(&env),
            dest_asset: setup.dest.clone(),
            dest_min: 200,
            amount_in: 100,
        },
    ];

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &200);

    assert_eq!(results.get(0).unwrap().success, false);
    assert_eq!(results.get(1).unwrap().success, false);
    assert_eq!(TokenClient::new(&env, &setup.source).balance(&setup.sender), 1_000_000);
    assert_eq!(TokenClient::new(&env, &setup.source).balance(&setup.contract_id), 0);
    assert_eq!(TokenClient::new(&env, &setup.dest).balance(&setup.contract_id), 0);
}

// Slippage floor validation
// -------------------------
// `dest_min` is the only delivery guarantee the contract makes. It is checked
// after each swap against the destination-token balance delta this contract
// measured, and it is also passed to the venue as amount_out_min, so it is the
// venue's own slippage protection too. A zero floor would therefore disable the
// only protection a recipient has: a swap could settle at any rate, deliver
// nothing, and still be recorded as a successful payout, contradicting the
// documented meaning of a `PayoutResult` with `amount_delivered == 0`. A zero
// floor is also not a minimum in the sense `Recipient.dest_min` documents, so
// `execute_batch` rejects any non-positive floor with `InvalidAmount`, the same
// error the contract already uses for its other non-positive monetary fields.

#[test]
fn execute_batch_rejects_zero_dest_min_before_funds_move() {
    let setup = setup_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 0,
            amount_in: 100,
        },
    ];

    let err = match client.try_execute_batch(&setup.sender, &setup.source, &recipients, &100) {
        Ok(_) => panic!("a zero slippage floor must be rejected"),
        Err(err) => err,
    };
    let all_events = env.events().all();

    let err = err.unwrap();
    assert!(err.is_type(xdr::ScErrorType::Contract));
    assert_eq!(err.get_code(), Error::InvalidAmount as u32);

    // Rejected before any funds move: the sender was not debited, the recipient
    // received nothing, no payout was recorded and no event was emitted.
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&recipient), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(client.get_payout_count(), 0);
    assert_eq!(count_events(&all_events, symbol_short!("payout")), 0);
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 0);
}

#[test]
fn execute_batch_rejects_negative_dest_min_before_funds_move() {
    let setup = setup_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: -1,
            amount_in: 100,
        },
    ];

    let err = match client.try_execute_batch(&setup.sender, &setup.source, &recipients, &100) {
        Ok(_) => panic!("a negative slippage floor must be rejected"),
        Err(err) => err,
    };
    let all_events = env.events().all();

    let err = err.unwrap();
    assert!(err.is_type(xdr::ScErrorType::Contract));
    assert_eq!(err.get_code(), Error::InvalidAmount as u32);

    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&recipient), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(client.get_payout_count(), 0);
    assert_eq!(count_events(&all_events, symbol_short!("payout")), 0);
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 0);
}

#[test]
fn execute_batch_rejects_zero_dest_min_in_later_recipient_before_funds_move() {
    let setup = setup_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let first_recipient = Address::generate(&env);
    let second_recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: first_recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 50,
            amount_in: 50,
        },
        Recipient {
            address: second_recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 0,
            amount_in: 50,
        },
    ];

    // One invalid recipient rejects the whole run, so the valid first
    // recipient is not paid either and nothing is pulled from the sender.
    let err = match client.try_execute_batch(&setup.sender, &setup.source, &recipients, &100) {
        Ok(_) => panic!("one zero slippage floor must reject the whole batch"),
        Err(err) => err,
    };
    let all_events = env.events().all();

    let err = err.unwrap();
    assert!(err.is_type(xdr::ScErrorType::Contract));
    assert_eq!(err.get_code(), Error::InvalidAmount as u32);

    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&first_recipient), 0);
    assert_eq!(dest_client.balance(&second_recipient), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(client.get_payout_count(), 0);
    assert_eq!(count_events(&all_events, symbol_short!("payout")), 0);
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 0);
}

#[test]
fn execute_batch_accepts_smallest_positive_dest_min() {
    let setup = setup_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 1,
            amount_in: 1,
        },
    ];

    // One unit is the smallest accepted floor, and it still has to be met.
    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &1);

    assert_eq!(results.get(0).unwrap().success, true);
    assert_eq!(results.get(0).unwrap().amount_delivered, 1);
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&recipient), 1);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(
        TokenClient::new(&env, &setup.source).balance(&setup.contract_id),
        0
    );
    assert_eq!(client.get_payout_count(), 1);
}

// Batch size limit
// ----------------
// `MAX_BATCH_RECIPIENTS` is derived from measured resource usage. Each
// recipient costs one FlowRoute `payout` event plus the events its token
// transfers and its venue swap emit, and a transaction may emit at most 16,384
// bytes of contract events. The boundary was located by running successful
// batches at increasing sizes with the network limits enforced, so the failing
// size is reported by the host rather than assumed:
//
//   * minimal venue (mirrors the router's fund flow only): 16 recipients fit
//     at 16,212 event bytes, 17 are rejected because "contract events size
//     bytes: 17200 > 16384".
//   * Soroswap-shaped venue (adds the router and pair events for one hop): 8
//     recipients fit at 14,836 event bytes, 9 are rejected because "contract
//     events size bytes: 16640 > 16384".
//
// CPU instructions, memory, ledger entries read and written, and bytes written
// stay far inside their limits at every size measured, so the event budget is
// the binding constraint. Those two sweeps ran against the pre-enforcement
// revision; once the limit is enforced, a batch above it is rejected by the
// contract, so the calibrated boundary is pinned by
// `batch_resource_envelope_reproduces_calibrated_ceiling`, which re-measures the
// per-recipient event cost at sizes the guard allows and re-derives the ceiling
// from those measurements.
//
// Authorizing the venue's source-token pull was re-measured against this same
// boundary. It adds one pair resolution per distinct destination asset per run
// and one authorization per recipient, and no events, so the re-measured
// envelope is unchanged in the dimension that binds (404 bytes fixed plus 1,804
// bytes per recipient, ceiling still 8). The ceiling for sizes above the
// enforced maximum is asserted arithmetically rather than observed, because a
// batch that large cannot be run here: the Soroban test environment meters a
// fixed internal budget per contract invocation (a guard on its own
// authorization instrumentation, on the order of tens of invocations per
// environment), and it is reached at nine recipients with this venue before the
// network event budget can reject the batch.

/// Stellar mainnet transaction-level limits. These are the values soroban-sdk
/// enforces by default in `Env::default()` through
/// `InvocationResourceLimits::mainnet()`. The limits type is not re-exported by
/// `soroban_sdk::testutils`, so the values are repeated here; they are also the
/// values the test environment enforces, so a drift in either direction makes
/// these tests fail instead of passing silently.
const MAINNET_INSTRUCTIONS: i64 = 400_000_000;
const MAINNET_MEM_BYTES: i64 = 41_943_040;
const MAINNET_DISK_READ_ENTRIES: u32 = 200;
const MAINNET_WRITE_ENTRIES: u32 = 200;
const MAINNET_LEDGER_ENTRIES: u32 = 400;
const MAINNET_DISK_READ_BYTES: u32 = 200_000;
const MAINNET_WRITE_BYTES: u32 = 132_096;
const MAINNET_CONTRACT_EVENTS_SIZE_BYTES: u32 = 16_384;

/// One invocation's metered resource usage, copied out of the SDK's
/// `InvocationResources` so these tests do not depend on that type being
/// nameable from this crate.
#[derive(Debug)]
struct Envelope {
    instructions: i64,
    mem_bytes: i64,
    disk_read_entries: u32,
    memory_read_entries: u32,
    write_entries: u32,
    disk_read_bytes: u32,
    write_bytes: u32,
    contract_events_size_bytes: u32,
}

impl Envelope {
    fn ledger_entries(&self) -> u32 {
        self.disk_read_entries + self.memory_read_entries + self.write_entries
    }
}

/// Every recipient asks for the 1:1 amount the mock venue can deliver, so the
/// batch is a realistic successful batch.
fn successful_batch(setup: &BatchSetup, count: u32) -> (Vec<Recipient>, i128) {
    let mut recipients: Vec<Recipient> = Vec::new(&setup.env);
    let mut total: i128 = 0;
    for _ in 0..count {
        recipients.push_back(Recipient {
            address: Address::generate(&setup.env),
            dest_asset: setup.dest.clone(),
            dest_min: 50,
            amount_in: 50,
        });
        total += 50;
    }
    (recipients, total)
}

/// Runs a batch with only the sender's invocation tree authorized, the way an
/// application submits it: the sender authorizes `execute_batch` and the nested
/// source transfer into this contract, and nothing else. The venue's pull of
/// this contract's tokens is authorized by the contract itself, so under this
/// enforced tree a missing or misdirected authorization fails here.
fn execute_batch_with_sender_auth(
    setup: &BatchSetup,
    recipients: &Vec<Recipient>,
    total_source_amount: i128,
) -> Vec<PayoutResult> {
    let env = setup.env.clone();
    let sub_invokes = [MockAuthInvoke {
        contract: &setup.source,
        fn_name: "transfer",
        args: (
            setup.sender.clone(),
            setup.contract_id.clone(),
            total_source_amount,
        )
            .into_val(&env),
        sub_invokes: &[],
    }];
    let invoke = MockAuthInvoke {
        contract: &setup.contract_id,
        fn_name: "execute_batch",
        args: (
            setup.sender.clone(),
            setup.source.clone(),
            recipients.clone(),
            total_source_amount,
        )
            .into_val(&env),
        sub_invokes: &sub_invokes,
    };
    env.mock_auths(&[MockAuth {
        address: &setup.sender,
        invoke: &invoke,
    }]);

    RouterClient::new(&env, &setup.contract_id).execute_batch(
        &setup.sender,
        &setup.source,
        recipients,
        &total_source_amount,
    )
}

/// Runs a `count`-recipient batch against the Soroswap-shaped venue and returns
/// the metered envelope of the invocation. `Env::default()` enforces the
/// mainnet limits, so a batch that does not fit panics here rather than
/// returning.
fn measure_shaped_envelope(count: u32) -> Envelope {
    let setup = setup_shaped_batch();
    let env = setup.env.clone();
    let (recipients, total) = successful_batch(&setup, count);

    let results = execute_batch_with_sender_auth(&setup, &recipients, total);
    assert_eq!(results.len(), count);
    for result in results.iter() {
        assert!(result.success);
    }

    let resources = env.cost_estimate().resources();
    Envelope {
        instructions: resources.instructions,
        mem_bytes: resources.mem_bytes,
        disk_read_entries: resources.disk_read_entries,
        memory_read_entries: resources.memory_read_entries,
        write_entries: resources.write_entries,
        disk_read_bytes: resources.disk_read_bytes,
        write_bytes: resources.write_bytes,
        contract_events_size_bytes: resources.contract_events_size_bytes,
    }
}

/// Calibration and regression guard for `MAX_BATCH_RECIPIENTS`.
///
/// Measures a successful batch at the enforced maximum against the
/// Soroswap-shaped venue, checks every metered dimension against the mainnet
/// limits, and re-derives the event-budget ceiling from the measured
/// per-recipient cost. If the payout event shape, the token transfers, the
/// venue's event output, or the venue authorization change the per-recipient
/// cost, this fails and the maximum has to be re-derived rather than left to
/// drift above the real boundary. It runs through the enforced sender
/// authorization tree, so it measures the shipped pull path and not one where
/// every authorization is mocked.
#[test]
fn batch_resource_envelope_reproduces_calibrated_ceiling() {
    let one = measure_shaped_envelope(1);
    let cap = measure_shaped_envelope(MAX_BATCH_RECIPIENTS);
    std::println!("recipients=1  {one:?}");
    std::println!("recipients={MAX_BATCH_RECIPIENTS}  {cap:?}");

    // Every dimension is inside the mainnet limit at the enforced maximum.
    assert!(cap.instructions < MAINNET_INSTRUCTIONS);
    assert!(cap.mem_bytes < MAINNET_MEM_BYTES);
    assert!(cap.disk_read_entries <= MAINNET_DISK_READ_ENTRIES);
    assert!(cap.ledger_entries() <= MAINNET_LEDGER_ENTRIES);
    assert!(cap.write_entries <= MAINNET_WRITE_ENTRIES);
    assert!(cap.disk_read_bytes <= MAINNET_DISK_READ_BYTES);
    assert!(cap.write_bytes <= MAINNET_WRITE_BYTES);
    assert!(cap.contract_events_size_bytes <= MAINNET_CONTRACT_EVENTS_SIZE_BYTES);

    // The event footprint is linear in the recipient count: the batch event
    // and the source transfer are fixed costs and every recipient adds the same
    // payout event plus the same token and venue events. Fit those two
    // constants from the two measurements and project the largest batch the
    // event budget allows.
    let one_events = i64::from(one.contract_events_size_bytes);
    let cap_events = i64::from(cap.contract_events_size_bytes);
    let per_recipient = (cap_events - one_events) / i64::from(MAX_BATCH_RECIPIENTS - 1);
    let fixed = one_events - per_recipient;
    assert_eq!(
        per_recipient, 1_804,
        "per-recipient event bytes changed; re-derive MAX_BATCH_RECIPIENTS"
    );
    assert_eq!(
        fixed, 404,
        "fixed per-batch event bytes changed; re-derive MAX_BATCH_RECIPIENTS"
    );
    let ceiling = (i64::from(MAINNET_CONTRACT_EVENTS_SIZE_BYTES) - fixed) / per_recipient;
    assert_eq!(
        ceiling, 8,
        "the event budget no longer fits 8 recipients; re-derive MAX_BATCH_RECIPIENTS"
    );
    assert!(
        i64::from(MAX_BATCH_RECIPIENTS) < ceiling,
        "MAX_BATCH_RECIPIENTS must stay below the measured event-budget ceiling"
    );

    // At the maximum, the event budget is the dimension under the most
    // pressure, and it still has headroom.
    assert!(
        cap.contract_events_size_bytes * 4 < MAINNET_CONTRACT_EVENTS_SIZE_BYTES * 3,
        "the enforced maximum should leave at least a quarter of the event budget unused"
    );
    assert!(cap.instructions * 20 < MAINNET_INSTRUCTIONS);
    assert!(cap.mem_bytes * 10 < MAINNET_MEM_BYTES);
}

#[test]
fn execute_batch_one_below_limit_succeeds() {
    let setup = setup_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let count = MAX_BATCH_RECIPIENTS - 1;
    let (recipients, total) = successful_batch(&setup, count);

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &total);

    assert_eq!(results.len(), count);
    let dest_client = TokenClient::new(&env, &setup.dest);
    for result in results.iter() {
        assert!(result.success);
        assert_eq!(result.amount_delivered, 50);
        assert_eq!(dest_client.balance(&result.recipient), 50);
    }
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000 - total);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(client.get_payout_count(), 1);
}

#[test]
fn execute_batch_at_limit_succeeds() {
    let setup = setup_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let (recipients, total) = successful_batch(&setup, MAX_BATCH_RECIPIENTS);

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &total);
    let all_events = env.events().all();

    assert_eq!(results.len(), MAX_BATCH_RECIPIENTS);
    let dest_client = TokenClient::new(&env, &setup.dest);
    for result in results.iter() {
        assert!(result.success);
        assert_eq!(result.amount_delivered, 50);
        assert_eq!(dest_client.balance(&result.recipient), 50);
    }
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000 - total);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(client.get_payout_count(), 1);
    assert_eq!(
        count_events(&all_events, symbol_short!("payout")),
        MAX_BATCH_RECIPIENTS as usize
    );
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 1);
}

#[test]
fn execute_batch_above_limit_is_rejected_before_funds_move() {
    let setup = setup_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let (recipients, total) = successful_batch(&setup, MAX_BATCH_RECIPIENTS + 1);

    let err = match client.try_execute_batch(&setup.sender, &setup.source, &recipients, &total) {
        Ok(_) => panic!("a batch above MAX_BATCH_RECIPIENTS must be rejected"),
        Err(err) => err,
    };
    let all_events = env.events().all();

    // The caller gets the named contract error, not a network resource failure.
    let err = err.unwrap();
    assert!(err.is_type(xdr::ScErrorType::Contract));
    assert_eq!(err.get_code(), Error::TooManyRecipients as u32);

    // And the rejection happens before any token movement: the sender was not
    // debited, this contract holds nothing, no payout was recorded, and no
    // event was emitted.
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    assert_eq!(TokenClient::new(&env, &setup.dest).balance(&setup.contract_id), 0);
    assert_eq!(client.get_payout_count(), 0);
    assert_eq!(count_events(&all_events, symbol_short!("payout")), 0);
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 0);
}

#[test]
fn execute_batch_mixed_results_at_limit_isolates_failure() {
    let setup = setup_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let (mut recipients, total) = successful_batch(&setup, MAX_BATCH_RECIPIENTS);

    // Make the second recipient's floor higher than the venue can deliver at
    // its 1:1 rate, so that swap reverts while the rest of the batch succeeds.
    let failing_address = Address::generate(&env);
    recipients.set(
        1,
        Recipient {
            address: failing_address.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 200,
            amount_in: 50,
        },
    );

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &total);
    let all_events = env.events().all();

    assert_eq!(results.len(), MAX_BATCH_RECIPIENTS);
    assert_eq!(results.get(0).unwrap().success, true);
    assert_eq!(results.get(0).unwrap().amount_delivered, 50);
    assert_eq!(results.get(1).unwrap().success, false);
    assert_eq!(results.get(1).unwrap().amount_delivered, 0);
    for index in 2..MAX_BATCH_RECIPIENTS {
        assert_eq!(results.get(index).unwrap().success, true);
    }

    // The failed recipient received nothing and its source came back to the
    // sender; the contract keeps no source or destination tokens.
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&failing_address), 0);
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000 - total + 50);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);

    assert_eq!(
        count_events(&all_events, symbol_short!("payout")),
        MAX_BATCH_RECIPIENTS as usize
    );
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 1);
    assert_eq!(client.get_payout_count(), 1);
}

// Venue authorization
// -------------------
// The venue moves the source tokens with its own `transfer` of the source
// token, from this contract into the pair it resolves for the recipient's asset
// pair. That invocation is a `require_auth` on this contract from a frame whose
// invoker is the venue, so it is authorized only because `execute_batch`
// declares it, for that exact pair and amount, before the venue runs. These
// tests run under an enforced authorization tree holding only the sender's
// invocation tree, so a missing or misdirected declaration fails here instead
// of silently refunding every recipient.

/// Like `setup_batch`, but the configured venue calls the pair, so the pair
/// delivers the output and needs the destination liquidity to deliver.
fn setup_pair_invoking_batch() -> BatchSetup {
    let setup = setup_with_venue(|env| env.register(pair_invoking_router::PairInvokingRouter, ()));
    StellarAssetClient::new(&setup.env, &setup.dest).mint(&setup.pair, &10_000_000);
    setup
}

#[test]
fn execute_batch_settles_a_router_shaped_payout_under_enforced_auth() {
    let setup = setup_pair_invoking_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 100,
            amount_in: 100,
        },
    ];

    let results = execute_batch_with_sender_auth(&setup, &recipients, 100);

    // The payout settles and the recipient receives the destination amount.
    let result = results.get(0).unwrap();
    assert!(result.success, "a router-shaped payout must settle");
    assert_eq!(result.amount_delivered, 100);

    let source_client = TokenClient::new(&env, &setup.source);
    let dest_client = TokenClient::new(&env, &setup.dest);
    assert_eq!(dest_client.balance(&recipient), 100);
    // The router's pull took the allocation out of this contract and into the
    // pair it resolved, which is where the deployed router puts it.
    assert_eq!(source_client.balance(&setup.pair), 100);
    // The sender paid exactly the batch total and this contract keeps nothing.
    assert_eq!(source_client.balance(&setup.sender), 1_000_000 - 100);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    assert_eq!(dest_client.balance(&setup.contract_id), 0);
    assert_eq!(client.get_payout_count(), 1);
}

#[test]
fn execute_batch_authorizes_only_the_pair_the_venue_resolves() {
    let setup = setup_pair_invoking_batch();
    let env = setup.env.clone();

    // The pair the venue reports is the pair that exists at that address, so
    // the address this contract authorizes is the one the pull targets.
    let resolved = pair_invoking_router::PairInvokingRouterClient::new(&env, &setup.swap_router)
        .router_pair_for(&setup.source, &setup.dest);
    assert_eq!(resolved, setup.pair);

    let recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 100,
            amount_in: 100,
        },
    ];
    assert!(
        execute_batch_with_sender_auth(&setup, &recipients, 100)
            .get(0)
            .unwrap()
            .success
    );
    assert_eq!(
        TokenClient::new(&env, &setup.source).balance(&setup.pair),
        100
    );

    // A venue that pulls into an address other than the pair it resolved is not
    // authorized for it: that recipient fails, its allocation is refunded, and
    // the unauthorized destination receives nothing.
    let misrouted = setup_with_venue(|env| {
        let venue = env.register(misrouting_router::MisroutingRouter, ());
        misrouting_router::MisroutingRouterClient::new(env, &venue).set_pull_target(&venue);
        venue
    });
    let env = misrouted.env.clone();
    let client = RouterClient::new(&env, &misrouted.contract_id);
    let recipient = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: recipient.clone(),
            dest_asset: misrouted.dest.clone(),
            dest_min: 100,
            amount_in: 100,
        },
    ];

    let results = execute_batch_with_sender_auth(&misrouted, &recipients, 100);

    assert!(!results.get(0).unwrap().success);
    assert_eq!(results.get(0).unwrap().amount_delivered, 0);
    let source_client = TokenClient::new(&env, &misrouted.source);
    assert_eq!(source_client.balance(&misrouted.sender), 1_000_000);
    assert_eq!(source_client.balance(&misrouted.contract_id), 0);
    // Neither the unauthorized destination nor the pair received anything.
    assert_eq!(source_client.balance(&misrouted.swap_router), 0);
    assert_eq!(source_client.balance(&misrouted.pair), 0);
    assert_eq!(
        TokenClient::new(&env, &misrouted.dest).balance(&recipient),
        0
    );
    assert_eq!(client.get_payout_count(), 1);
}

#[test]
fn execute_batch_isolates_pair_failures_per_recipient() {
    let setup = setup_pair_invoking_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);

    // A destination asset whose pair was never created: nothing is deployed at
    // the address the venue resolves, so that recipient's swap cannot settle.
    let unpaired_dest = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let paid = Address::generate(&env);
    let same_asset = Address::generate(&env);
    let unpaired = Address::generate(&env);
    let recipients = vec![
        &env,
        Recipient {
            address: paid.clone(),
            dest_asset: setup.dest.clone(),
            dest_min: 50,
            amount_in: 50,
        },
        // dest_asset == source_asset: no pair can be resolved for it at all.
        Recipient {
            address: same_asset.clone(),
            dest_asset: setup.source.clone(),
            dest_min: 50,
            amount_in: 50,
        },
        Recipient {
            address: unpaired.clone(),
            dest_asset: unpaired_dest.clone(),
            dest_min: 50,
            amount_in: 50,
        },
    ];

    let results = execute_batch_with_sender_auth(&setup, &recipients, 150);

    assert_eq!(results.len(), 3);
    assert!(results.get(0).unwrap().success);
    assert_eq!(results.get(0).unwrap().amount_delivered, 50);
    // The unresolvable pair and the missing pair both fail safely.
    assert!(!results.get(1).unwrap().success);
    assert_eq!(results.get(1).unwrap().amount_delivered, 0);
    assert!(!results.get(2).unwrap().success);
    assert_eq!(results.get(2).unwrap().amount_delivered, 0);

    // Only the settled recipient was paid and only its allocation left the
    // sender; nothing is stranded in this contract or in the pair.
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(TokenClient::new(&env, &setup.dest).balance(&paid), 50);
    assert_eq!(TokenClient::new(&env, &unpaired_dest).balance(&unpaired), 0);
    assert_eq!(source_client.balance(&setup.sender), 1_000_000 - 50);
    assert_eq!(source_client.balance(&setup.contract_id), 0);
    assert_eq!(source_client.balance(&setup.pair), 50);
    assert_eq!(client.get_payout_count(), 1);
}
