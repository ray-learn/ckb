mod block_verifier;
mod error;
mod header_verifier;
mod transaction_verifier;

#[cfg(test)]
mod tests;

pub use crate::block_verifier::{BlockVerifier, HeaderResolverWrapper, TransactionsVerifier};
pub use crate::error::{Error, TransactionError};
pub use crate::header_verifier::{HeaderResolver, HeaderVerifier};
pub use crate::transaction_verifier::{PoolTransactionVerifier, TransactionVerifier};

pub const ALLOWED_FUTURE_BLOCKTIME: u64 = 15 * 1000; // 15 Second

pub trait Verifier {
    type Target;
    fn verify(&self, target: &Self::Target) -> Result<(), Error>;
}
