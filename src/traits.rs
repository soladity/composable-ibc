use crate::error::BeefyClientError;
use crate::primitives::BeefyNextAuthoritySet;
use codec::{Decode, Encode};
use sp_core::H256;
use sp_std::prelude::*;

#[derive(sp_std::fmt::Debug, Encode, Decode, Clone)]
pub struct MmrState {
    pub latest_beefy_height: u32,
    pub mmr_root_hash: H256,
}

#[derive(sp_std::fmt::Debug, Encode, Decode, Clone)]
pub struct AuthoritySet {
    pub current_authorities: BeefyNextAuthoritySet<H256>,
    pub next_authorities: BeefyNextAuthoritySet<H256>,
}

pub trait StorageRead {
    fn mmr_state(&self) -> Result<MmrState, BeefyClientError>;
    fn authority_set(&self) -> Result<AuthoritySet, BeefyClientError>;
}

pub trait StorageWrite {
    fn set_mmr_state(&mut self, mmr_state: MmrState) -> Result<(), BeefyClientError>;
    fn set_authority_set(&mut self, set: AuthoritySet) -> Result<(), BeefyClientError>;
}

pub trait HostFunctions {
    fn keccak_256(input: &[u8]) -> [u8; 32];
    fn secp256k1_ecdsa_recover_compressed(
        signature: &[u8; 65],
        value: &[u8; 32],
    ) -> Option<Vec<u8>>;
}
