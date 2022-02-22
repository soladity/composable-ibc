use crate::error::BeefyClientError;
use crate::primitives::BeefyNextAuthoritySet;
use codec::{Decode, Encode};
use sp_core::H256;

#[derive(Debug, Encode, Decode)]
pub struct MmrState {
    pub latest_height: u32,
    pub mmr_root_hash: H256,
}

#[derive(Debug, Encode, Decode)]
pub struct AuthoritySet {
    pub current_authorities: BeefyNextAuthoritySet<H256>,
    pub next_authorities: BeefyNextAuthoritySet<H256>,
}

pub trait StorageRead {
    fn mmr_state() -> Result<MmrState, BeefyClientError>;
    fn authority_set() -> Result<AuthoritySet, BeefyClientError>;
}

pub trait StorageWrite {
    fn set_mmr_state(mmr_state: MmrState) -> Result<(), BeefyClientError>;
    fn set_authority_set(set: AuthoritySet) -> Result<(), BeefyClientError>;
}
