#![no_std]

use soroban_sdk::{contract, contractimpl, panic_with_error, Address, Env};

pub mod aggregator;
pub mod error;
pub mod events;
pub mod storage;
pub mod types;

#[cfg(test)]
mod test;

pub use error::Error;
pub use types::{PayoutResult, Recipient};

#[contract]
pub struct Router;

#[contractimpl]
impl Router {
    /// Sets the admin, clears the paused flag, and resets the payout counter.
    /// Takes no auth: the first caller sets the admin at deploy time. Reverts
    /// with AlreadyInitialized if the contract was already initialized.
    pub fn initialize(env: Env, admin: Address) {
        if storage::read_admin(&env).is_some() {
            panic_with_error!(env, Error::AlreadyInitialized);
        }
        storage::write_admin(&env, &admin);
        storage::write_paused(&env, &false);
        storage::write_payout_count(&env, &0);
        // Give the fresh entries the full TTL window so the contract does not
        // need an immediate follow-up call to stay alive.
        storage::extend_instance_ttl(&env);
    }

    /// Pauses or unpauses the batch executor. Admin auth required. Reverts
    /// with NotInitialized if the contract was not initialized yet.
    pub fn set_paused(env: Env, paused: bool) {
        let admin = match storage::read_admin(&env) {
            Some(admin) => admin,
            None => panic_with_error!(env, Error::NotInitialized),
        };
        admin.require_auth();
        storage::extend_instance_ttl(&env);
        storage::write_paused(&env, &paused);
    }

    /// Returns the number of payout runs executed so far. Read-only; returns
    /// 0 if the counter is unset.
    pub fn get_payout_count(env: Env) -> u64 {
        storage::read_payout_count(&env)
    }
}
