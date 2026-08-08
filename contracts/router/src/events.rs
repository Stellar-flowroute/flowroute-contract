use soroban_sdk::{contractevent, Address, Env};

// Events are the auditable settlement record. The indexer in the app
// repository consumes these events, so topic names must stay stable; changing
// them later breaks the indexer.
//
// SDK note: in soroban-sdk 27 the env.events().publish API is deprecated in
// favor of the contractevent macro. The fixed topics below reproduce the
// exact shapes specified for this contract, and data_format vec keeps the
// data payload positional so the indexer sees the same layout.

/// One payout attempt per recipient.
///
/// Topics: (payout, payout_id, sender)
/// Data: (recipient, source_asset, dest_asset, amount_delivered, success)
#[contractevent(topics = ["payout"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutEvent {
    #[topic]
    pub payout_id: u64,
    #[topic]
    pub sender: Address,
    pub recipient: Address,
    pub source_asset: Address,
    pub dest_asset: Address,
    pub amount_delivered: i128,
    pub success: bool,
}

/// One summary event per batch run.
///
/// Topics: (batch, payout_id, sender)
/// Data: (recipient_count, success_count, total_source_amount)
#[contractevent(topics = ["batch"], data_format = "vec")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchEvent {
    #[topic]
    pub payout_id: u64,
    #[topic]
    pub sender: Address,
    pub recipient_count: u32,
    pub success_count: u32,
    pub total_source_amount: i128,
}

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
    PayoutEvent {
        payout_id,
        sender: sender.clone(),
        recipient: recipient.clone(),
        source_asset: source_asset.clone(),
        dest_asset: dest_asset.clone(),
        amount_delivered,
        success,
    }
    .publish(env);
}

pub fn batch(
    env: &Env,
    payout_id: u64,
    sender: &Address,
    recipient_count: u32,
    success_count: u32,
    total_source_amount: i128,
) {
    BatchEvent {
        payout_id,
        sender: sender.clone(),
        recipient_count,
        success_count,
        total_source_amount,
    }
    .publish(env);
}
