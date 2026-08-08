use soroban_sdk::{contracttype, Address};

/// One recipient of a payout run. All monetary values are i128 integer base
/// units; there are no floats anywhere in the contract.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recipient {
    /// Who receives the destination asset.
    pub address: Address,
    /// Token contract of the asset they receive.
    pub dest_asset: Address,
    /// Minimum units of dest_asset they must receive. This is the per
    /// recipient slippage floor, passed through as amount_out_min to the
    /// swap venue.
    pub dest_min: i128,
    /// Units of source asset allocated to this recipient.
    pub amount_in: i128,
}

/// Outcome of one recipient's payout attempt.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutResult {
    pub recipient: Address,
    pub success: bool,
    /// 0 if failed.
    pub amount_delivered: i128,
}
