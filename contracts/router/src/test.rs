use soroban_sdk::{testutils::Address as _, Address, Env};

use crate::{Router, RouterClient};

#[test]
fn initialize_sets_admin() {
    let env = Env::default();
    let contract_id = env.register(Router, ());
    let client = RouterClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    client.initialize(&admin);
}

#[test]
fn initialize_second_call_reverts() {
    let env = Env::default();
    let contract_id = env.register(Router, ());
    let client = RouterClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    client.initialize(&admin);

    let other = Address::generate(&env);
    assert!(client.try_initialize(&other).is_err());
}
