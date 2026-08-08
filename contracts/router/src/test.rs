extern crate std;

use soroban_sdk::{
    symbol_short, testutils::{Address as _, Events}, token::{StellarAssetClient, TokenClient},
    vec, Address, Env, Symbol, Vec, xdr,
};

use crate::{aggregator, Recipient, Router, RouterClient};

fn setup_client(env: &Env) -> (Address, Address, RouterClient<'_>) {
    let contract_id = env.register(Router, ());
    let client = RouterClient::new(env, &contract_id);
    let admin = Address::generate(env);
    (contract_id, admin, client)
}

/// Mock swap venue. Registered at the venue address constant that the
/// contract uses, so the contract calls this mock exactly as it would call
/// the real Soroswap Router. It requires auth from `to`, pulls the source
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

struct BatchSetup {
    env: Env,
    contract_id: Address,
    sender: Address,
    source: Address,
    dest: Address,
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
    client.initialize(&admin);

    let token_admin = Address::generate(&env);
    let source = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let dest = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    let venue = Address::from_str(&env, aggregator::SOROSWAP_ROUTER_TESTNET);
    env.register_at(&venue, mock_router::MockRouter, ());

    let sender = Address::generate(&env);
    StellarAssetClient::new(&env, &source).mint(&sender, &1_000_000);
    StellarAssetClient::new(&env, &dest).mint(&venue, &10_000_000);

    BatchSetup {
        env,
        contract_id,
        sender,
        source,
        dest,
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
fn initialize_sets_admin() {
    let env = Env::default();
    let (_contract_id, admin, client) = setup_client(&env);
    client.initialize(&admin);
}

#[test]
fn initialize_second_call_reverts() {
    let env = Env::default();
    let (_contract_id, admin, client) = setup_client(&env);
    client.initialize(&admin);

    let other = Address::generate(&env);
    assert!(client.try_initialize(&other).is_err());
}

#[test]
fn set_paused_requires_admin_auth() {
    let env = Env::default();
    let (_contract_id, admin, client) = setup_client(&env);
    client.initialize(&admin);

    // No auth is mocked for the admin signature, so the call reverts.
    assert!(client.try_set_paused(&true).is_err());
}

#[test]
fn set_paused_admin_roundtrip() {
    let env = Env::default();
    env.mock_all_auths();
    let (_contract_id, admin, client) = setup_client(&env);
    client.initialize(&admin);

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
    client.initialize(&admin);

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
    client.initialize(&admin);
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
    client.initialize(&admin);

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
    client.initialize(&admin);

    let sender = Address::generate(&env);
    let source = Address::generate(&env);
    let recipient = Recipient {
        address: Address::generate(&env),
        dest_asset: Address::generate(&env),
        dest_min: 0,
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
fn execute_batch_happy_path() {
    let setup = setup_batch();
    let env = setup.env;
    let client = RouterClient::new(&env, &setup.contract_id);

    let recipient_1 = Address::generate(&env);
    let recipient_2 = Address::generate(&env);
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
            dest_asset: setup.dest.clone(),
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
    assert_eq!(dest_client.balance(&recipient_2), 100);

    // The sender was debited exactly the batch total; the contract holds no
    // leftover source.
    let source_client = TokenClient::new(&env, &setup.source);
    assert_eq!(source_client.balance(&setup.sender), 999_700);
    assert_eq!(source_client.balance(&setup.contract_id), 0);

    // One payout event per recipient plus one batch summary.
    assert_eq!(count_events(&all_events, symbol_short!("payout")), 2);
    assert_eq!(count_events(&all_events, symbol_short!("batch")), 1);

    // The payout counter advanced.
    assert_eq!(client.get_payout_count(), 1);
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

