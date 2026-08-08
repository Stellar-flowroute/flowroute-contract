use soroban_sdk::{symbol_short, Address, Env};

// Events are the auditable settlement record. The indexer in the app
// repository consumes these events, so topic names must stay stable; changing
// them later breaks the indexer.

/// One payout attempt per recipient.
///
/// Topics: (payout, payout_id, sender)
/// Data: (recipient, source_asset, dest_asset, amount_delivered, success)
pub fn payout(
    env: &Env,
    payout_id: u64,
    sender: &Address,
    recipient: &Address,
    source_asset: &Address,
    dest_asset: &Address,
    amount_delivered: i128,
    success: bool,
) {
    env.events().publish(
        (symbol_short!("payout"), payout_id, sender.clone()),
        (
            recipient.clone(),
            source_asset.clone(),
            dest_asset.clone(),
            amount_delivered,
            success,
        ),
    );
}

/// One summary event per batch run.
///
/// Topics: (batch, payout_id, sender)
/// Data: (recipient_count, success_count, total_source_amount)
pub fn batch(
    env: &Env,
    payout_id: u64,
    sender: &Address,
    recipient_count: u32,
    success_count: u32,
    total_source_amount: i128,
) {
    env.events().publish(
        (symbol_short!("batch"), payout_id, sender.clone()),
        (recipient_count, success_count, total_source_amount),
    );
}
