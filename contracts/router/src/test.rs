use soroban_sdk::{testutils::Address as _, Address, Env};

use crate::{Router, RouterClient};

fn setup_client(env: &Env) -> (Address, RouterClient) {
    let contract_id = env.register(Router, ());
    let client = RouterClient::new(env, &contract_id);
    let admin = Address::generate(env);
    (admin, client)
}

#[test]
fn initialize_sets_admin() {
    let env = Env::default();
    let (admin, client) = setup_client(&env);
    client.initialize(&admin);
}

#[test]
fn initialize_second_call_reverts() {
    let env = Env::default();
    let (admin, client) = setup_client(&env);
    client.initialize(&admin);

    let other = Address::generate(&env);
    assert!(client.try_initialize(&other).is_err());
}

#[test]
fn set_paused_requires_admin_auth() {
    let env = Env::default();
    let (admin, client) = setup_client(&env);
    client.initialize(&admin);

    // No auth is mocked for the admin signature, so the call reverts.
    assert!(client.try_set_paused(&true).is_err());
}

#[test]
fn set_paused_admin_roundtrip() {
    let env = Env::default();
    env.mock_all_auths();
    let (admin, client) = setup_client(&env);
    client.initialize(&admin);

    client.set_paused(&true);
    client.set_paused(&false);
}

#[test]
fn set_paused_before_initialize_reverts() {
    let env = Env::default();
    env.mock_all_auths();
    let (_admin, client) = setup_client(&env);

    assert!(client.try_set_paused(&true).is_err());
}

#[test]
fn get_payout_count_starts_at_zero() {
    let env = Env::default();
    let (admin, client) = setup_client(&env);
    client.initialize(&admin);

    assert_eq!(client.get_payout_count(), 0);
}
