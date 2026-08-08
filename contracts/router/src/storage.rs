use soroban_sdk::{contracttype, Address, Env};

#[contracttype]
pub enum DataKey {
    Admin,
    Paused,
    PayoutCount,
}

// TTL policy
// ----------
// All contract state lives in instance storage. Entries are extended on every
// state-changing entrypoint via extend_instance_ttl, called after validation
// and before any state mutation.
//
// Threshold and extend-to are expressed in ledgers. Stellar produces a ledger
// roughly every 5 seconds, so 5000 ledgers is about 7 hours and 10000 ledgers
// about 14 hours. These are the same order of magnitude as the values the
// Soroban documentation recommends for instance entries.
//
// Consequence: if the contract is left idle for longer than the threshold
// window, its entries can expire and the contract must be re-initialized. A
// payout router is exercised on every payout run, so this is acceptable for
// the MVP. A background keeper that bumps TTL, or larger thresholds, can be
// added before mainnet.
pub const TTL_THRESHOLD: u32 = 5000;
pub const TTL_EXTEND_TO: u32 = 10000;

pub fn extend_instance_ttl(env: &Env) {
    env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

pub fn read_admin(env: &Env) -> Option<Address> {
    env.storage().instance().get(&DataKey::Admin)
}

pub fn write_admin(env: &Env, admin: &Address) {
    env.storage().instance().set(&DataKey::Admin, admin);
}

pub fn read_paused(env: &Env) -> bool {
    env.storage().instance().get(&DataKey::Paused).unwrap_or(false)
}

pub fn write_paused(env: &Env, paused: &bool) {
    env.storage().instance().set(&DataKey::Paused, paused);
}

pub fn read_payout_count(env: &Env) -> u64 {
    env.storage().instance().get(&DataKey::PayoutCount).unwrap_or(0)
}

pub fn write_payout_count(env: &Env, count: &u64) {
    env.storage().instance().set(&DataKey::PayoutCount, count);
}
