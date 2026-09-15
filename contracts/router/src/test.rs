extern crate std;

use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events, MockAuth, MockAuthInvoke},
    token::{StellarAssetClient, TokenClient},
    vec, xdr, Address, Env, IntoVal, Symbol, Vec,
};

use crate::{storage, Error, Recipient, Router, RouterClient, MAX_BATCH_RECIPIENTS};

fn setup_client(env: &Env) -> (Address, Address, RouterClient<'_>) {
    let contract_id = env.register(Router, ());
    let client = RouterClient::new(env, &contract_id);
    let admin = Address::generate(env);
    (contract_id, admin, client)
}

fn configured_swap_router(env: &Env, contract_id: &Address) -> Option<Address> {
    env.as_contract(contract_id, || storage::read_swap_router(env))
}

/// Mock swap venue. Registered at the address supplied to initialize, so the
/// contract calls this mock exactly as it would call the real Soroswap Router.
/// It requires auth from `to`, pulls the source
/// tokens from `to`, and delivers the destination tokens back to `to` at a
/// fixed 1:1 rate. A requested floor above the deliverable amount reverts
/// with SlippageExceeded, mirroring the real router's atomic revert.
mod mock_router {
    use soroban_sdk::{
        contract, contractimpl, panic_with_error, token::TokenClient, vec, Address, Env,
        MuxedAddress, Vec,
    };

    use crate::error::Error;

    #[contract]
    pub struct MockRouter;

    #[contractimpl]
    impl MockRouter {
        pub fn swap_exact_tokens_for_tokens(
            env: Env,
            amount_in: i128,
            amount_out_min: i128,
            path: Vec<Address>,
            to: Address,
            _deadline: u64,
        ) -> Result<Vec<i128>, Error> {
            let token_in = TokenClient::new(&env, &path.get(0).unwrap());
            let token_out = TokenClient::new(&env, &path.get(1).unwrap());
            let self_address = env.current_contract_address();

            to.require_auth();

            let amount_out = amount_in;
            if amount_out < amount_out_min {
                panic_with_error!(env, Error::SlippageExceeded);
            }

            // Mirror the real router fund flow: pull the input from `to`,
            // then deliver the output to `to`.
            token_in.transfer(&to, &MuxedAddress::from(&self_address), &amount_in);
            token_out.transfer(&self_address, &MuxedAddress::from(&to), &amount_out);

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

    use crate::error::Error;

    #[contract]
    pub struct UnderDeliveringRouter;

    #[contractimpl]
    impl UnderDeliveringRouter {
        pub fn swap_exact_tokens_for_tokens(
            env: Env,
            amount_in: i128,
            amount_out_min: i128,
            path: Vec<Address>,
            to: Address,
            _deadline: u64,
        ) -> Result<Vec<i128>, Error> {
            let token_in = TokenClient::new(&env, &path.get(0).unwrap());
            let token_out = TokenClient::new(&env, &path.get(1).unwrap());
            let self_address = env.current_contract_address();
            let amount_out = if amount_out_min <= amount_in {
                amount_in
            } else {
                amount_out_min - 1
            };

            to.require_auth();
            token_in.transfer(&to, &MuxedAddress::from(&self_address), &amount_in);
            if amount_out > 0 {
                token_out.transfer(&self_address, &MuxedAddress::from(&to), &amount_out);
            }

            Ok(vec![&env, amount_in, amount_out])
        }
    }
}

/// Calibration venue. It mirrors `mock_router`'s fund flow and, on top of
/// that, publishes the events the verified Soroswap venue emits for the same
/// single-hop swap, so a calibration run sees the event footprint the deployed
/// venue produces. Shapes verified against github.com/soroswap/core:
///
///   router swap_exact_tokens_for_tokens -> topics ("SoroswapRouter", "swap")
///       SwapEvent { path: Vec<Address>, amounts: Vec<i128>, to: Address }
///   pair swap -> topics ("SoroswapPair", "swap")
///       SwapEvent { to, amount_0_in, amount_1_in, amount_0_out, amount_1_out }
///   pair update, called at the end of pair swap -> topics ("SoroswapPair",
///   "sync") SyncEvent { new_reserve_0, new_reserve_1 }
///
/// The deprecated `events().publish` entry point is used deliberately: the
/// macro-generated alternative cannot publish the 12-character component topic
/// names the venue uses.
mod soroswap_shaped_router {
    use soroban_sdk::{
        contract, contractimpl, contracttype, panic_with_error, symbol_short, token::TokenClient,
        vec, Address, Env, MuxedAddress, Vec,
    };

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

    #[contract]
    pub struct SoroswapShapedRouter;

    #[contractimpl]
    impl SoroswapShapedRouter {
        pub fn swap_exact_tokens_for_tokens(
            env: Env,
            amount_in: i128,
            amount_out_min: i128,
            path: Vec<Address>,
            to: Address,
            _deadline: u64,
        ) -> Result<Vec<i128>, Error> {
            let token_in = TokenClient::new(&env, &path.get(0).unwrap());
            let token_out = TokenClient::new(&env, &path.get(1).unwrap());
            let self_address = env.current_contract_address();

            to.require_auth();

            let amount_out = amount_in;
            if amount_out < amount_out_min {
                panic_with_error!(env, Error::SlippageExceeded);
            }

            token_in.transfer(&to, &MuxedAddress::from(&self_address), &amount_in);
            token_out.transfer(&self_address, &MuxedAddress::from(&to), &amount_out);

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

struct BatchSetup {
    env: Env,
    contract_id: Address,
    sender: Address,
    source: Address,
    dest: Address,
    swap_router: Address,
}

/// Initializes the contract, registers the mock venue, and funds the sender
/// with source tokens and the venue with destination tokens. All auths are
/// mocked.
fn setup_batch() -> BatchSetup {
    let env = Env::default();
    // The venue (and the tokens it touches on this contract's behalf) calls
    // require_auth on this contract at a depth beyond the direct invoker, so
    // non-root authorizations must be allowed in recording mode.
    env.mock_all_auths_allowing_non_root_auth();

    let (contract_id, admin, _client) = setup_client(&env);
    let client = RouterClient::new(&env, &contract_id);
    // Register at a generated address rather than the former hardcoded
    // testnet address. A fallback to that retired configuration makes every
    // successful-payout test below fail because no venue exists there.
    let swap_router = env.register(mock_router::MockRouter, ());
    client.initialize(&admin, &swap_router);

    let token_admin = Address::generate(&env);
    let source = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let dest = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

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
    }
}

fn setup_under_delivering_batch() -> BatchSetup {
    let env = Env::default();
    env.mock_all_auths_allowing_non_root_auth();

    let (contract_id, admin, _client) = setup_client(&env);
    let client = RouterClient::new(&env, &contract_id);
    let swap_router = env.register(under_delivering_router::UnderDeliveringRouter, ());
    client.initialize(&admin, &swap_router);

    let token_admin = Address::generate(&env);
    let source = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let dest = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
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
    }
}

/// Like `setup_batch`, but the configured venue is the Soroswap-shaped
/// calibration venue, so resource measurements include the venue's events.
fn setup_shaped_batch() -> BatchSetup {
    let env = Env::default();
    env.mock_all_auths_allowing_non_root_auth();

    let (contract_id, admin, _client) = setup_client(&env);
    let client = RouterClient::new(&env, &contract_id);
    let swap_router = env.register(soroswap_shaped_router::SoroswapShapedRouter, ());
    client.initialize(&admin, &swap_router);

    let token_admin = Address::generate(&env);
    let source = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let dest = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

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
    }
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
    assert_eq!(
        configured_swap_router(&env, &contract_id),
        Some(swap_router)
    );
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
    assert_eq!(source_client.balance(&setup.swap_router), 0);
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

/// Runs a `count`-recipient batch against the Soroswap-shaped venue and returns
/// the metered envelope of the invocation. `Env::default()` enforces the
/// mainnet limits, so a batch that does not fit panics here rather than
/// returning.
fn measure_shaped_envelope(count: u32) -> Envelope {
    let setup = setup_shaped_batch();
    let env = setup.env.clone();
    let client = RouterClient::new(&env, &setup.contract_id);
    let (recipients, total) = successful_batch(&setup, count);

    let results = client.execute_batch(&setup.sender, &setup.source, &recipients, &total);
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
/// per-recipient cost. If the payout event shape, the token transfers, or the
/// venue's event output change, this fails and the maximum has to be re-derived
/// rather than left to drift above the real boundary.
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
