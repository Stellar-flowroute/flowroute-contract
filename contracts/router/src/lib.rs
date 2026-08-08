#![no_std]

use soroban_sdk::{contract, contractimpl};

pub mod error;
pub mod storage;
pub mod types;

pub use error::Error;
pub use types::{PayoutResult, Recipient};

#[contract]
pub struct Router;

#[contractimpl]
impl Router {}
