use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    NotAdmin = 3,
    Paused = 4,
    SlippageExceeded = 5,
    EmptyBatch = 6,
    InvalidAmount = 7,
    SwapFailed = 8,
}
