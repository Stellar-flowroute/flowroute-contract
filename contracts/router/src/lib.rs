#![no_std]

use soroban_sdk::{contract, contractimpl};

pub mod error;

pub use error::Error;

#[contract]
pub struct Router;

#[contractimpl]
impl Router {}
