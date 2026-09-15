#![no_std]

use soroban_sdk::{
    contract, contractimpl, panic_with_error, token::TokenClient, vec, Address, Env,
    MuxedAddress, Vec,
};

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
    /// Sets the admin and immutable swap venue, clears the paused flag, and
    /// resets the payout counter.
    /// Requires authorization from the supplied admin so an unrelated caller
    /// cannot claim an uninitialized deployment. Reverts with
    /// AlreadyInitialized if the contract was already initialized.
    pub fn initialize(env: Env, admin: Address, swap_router: Address) {
        if storage::read_admin(&env).is_some() {
            panic_with_error!(env, Error::AlreadyInitialized);
        }
        admin.require_auth();
        storage::write_admin(&env, &admin);
        storage::write_swap_router(&env, &swap_router);
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

    /// Executes one payout run.
    ///
    /// Guards in order: not initialized, paused, sender auth, batch not
    /// empty, amounts consistent. The full total_source_amount is pulled from
    /// the sender into this contract, then each recipient's allocation is
    /// swapped on the venue with the recipient's dest_min enforced as the
    /// output floor.
    ///
    /// Refund policy: a recipient whose swap reverts has its source amount
    /// refunded to the sender at the end of the batch, because the venue
    /// reverts atomically and the source never left this contract. If a venue
    /// incorrectly returns success below dest_min, its measured destination
    /// output is returned to the sender immediately; the recipient receives
    /// nothing and the payout is recorded as failed.
    ///
    /// One recipient failing never aborts the batch.
    pub fn execute_batch(
        env: Env,
        sender: Address,
        source_asset: Address,
        recipients: Vec<Recipient>,
        total_source_amount: i128,
    ) -> Vec<PayoutResult> {
        if storage::read_admin(&env).is_none() {
            panic_with_error!(env, Error::NotInitialized);
        }
        let swap_router = match storage::read_swap_router(&env) {
            Some(router) => router,
            None => panic_with_error!(env, Error::NotInitialized),
        };
        if storage::read_paused(&env) {
            panic_with_error!(env, Error::Paused);
        }
        sender.require_auth();

        if recipients.is_empty() {
            panic_with_error!(env, Error::EmptyBatch);
        }
        if total_source_amount <= 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
        let mut allocated: i128 = 0;
        for recipient in recipients.iter() {
            // Reject non-positive allocations; a negative amount_in could
            // otherwise hide inside a sum that still matches the total.
            if recipient.amount_in <= 0 {
                panic_with_error!(env, Error::InvalidAmount);
            }
            allocated = match allocated.checked_add(recipient.amount_in) {
                Some(sum) => sum,
                None => panic_with_error!(env, Error::InvalidAmount),
            };
        }
        if allocated != total_source_amount {
            panic_with_error!(env, Error::InvalidAmount);
        }

        storage::extend_instance_ttl(&env);

        // Pull the full batch amount from the sender into this contract.
        let contract_address = env.current_contract_address();
        let source_client = TokenClient::new(&env, &source_asset);
        source_client.transfer(
            &sender,
            &MuxedAddress::from(&contract_address),
            &total_source_amount,
        );

        // Assign this run an id and record it.
        let payout_id = match storage::read_payout_count(&env).checked_add(1) {
            Some(id) => id,
            None => panic_with_error!(env, Error::InvalidAmount),
        };
        storage::write_payout_count(&env, &payout_id);

        let mut results: Vec<PayoutResult> = Vec::new(&env);
        let mut success_count: u32 = 0;
        // Source amounts to refund to the sender for recipients whose swap
        // reverted. Such source never left this contract.
        let mut refund_amount: i128 = 0;

        for recipient in recipients.iter() {
            let dest_client = TokenClient::new(&env, &recipient.dest_asset);
            let balance_before = dest_client.balance(&contract_address);

            let path = vec![&env, source_asset.clone(), recipient.dest_asset.clone()];
            let swap_outcome = aggregator::swap(
                &env,
                &swap_router,
                recipient.amount_in,
                recipient.dest_min,
                path,
                contract_address.clone(),
            );

            match swap_outcome {
                Ok(_) => {
                    let received = match dest_client
                        .balance(&contract_address)
                        .checked_sub(balance_before)
                    {
                        Some(delta) => delta,
                        None => 0,
                    };
                    if received >= recipient.dest_min {
                        dest_client.transfer(
                            &contract_address,
                            &MuxedAddress::from(&recipient.address),
                            &received,
                        );
                        success_count += 1;
                        events::payout(
                            &env,
                            payout_id,
                            &sender,
                            &recipient.address,
                            &source_asset,
                            &recipient.dest_asset,
                            received,
                            true,
                        );
                        results.push_back(PayoutResult {
                            recipient: recipient.address.clone(),
                            success: true,
                            amount_delivered: received,
                        });
                    } else {
                        // A non-conforming venue may return Ok while sending
                        // less than its requested floor. Do not pay a partial
                        // amount to the recipient or retain it here: return
                        // exactly this swap's measured delta to the sender.
                        // The balance delta excludes any pre-existing balance
                        // of this destination asset in the contract.
                        if received > 0 {
                            dest_client.transfer(
                                &contract_address,
                                &MuxedAddress::from(&sender),
                                &received,
                            );
                        }
                        events::payout(
                            &env,
                            payout_id,
                            &sender,
                            &recipient.address,
                            &source_asset,
                            &recipient.dest_asset,
                            0,
                            false,
                        );
                        results.push_back(PayoutResult {
                            recipient: recipient.address.clone(),
                            success: false,
                            amount_delivered: 0,
                        });
                    }
                }
                Err(_) => {
                    // The venue call reverted atomically, so this recipient's
                    // source amount is still held here. Refund it at the end.
                    refund_amount = match refund_amount.checked_add(recipient.amount_in) {
                        Some(sum) => sum,
                        None => panic_with_error!(env, Error::InvalidAmount),
                    };
                    events::payout(
                        &env,
                        payout_id,
                        &sender,
                        &recipient.address,
                        &source_asset,
                        &recipient.dest_asset,
                        0,
                        false,
                    );
                    results.push_back(PayoutResult {
                        recipient: recipient.address.clone(),
                        success: false,
                        amount_delivered: 0,
                    });
                }
            }
        }

        if refund_amount > 0 {
            source_client.transfer(
                &contract_address,
                &MuxedAddress::from(&sender),
                &refund_amount,
            );
        }

        events::batch(
            &env,
            payout_id,
            &sender,
            recipients.len(),
            success_count,
            total_source_amount,
        );

        results
    }
}
