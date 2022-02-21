// Copyright (C) 2022 ComposableFi.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
#![cfg_attr(not(feature = "std"), no_std)]

pub mod error;
pub mod primitives;
#[cfg(test)]
mod tests;
pub mod traits;

use crate::error::BeefyClientError;
use crate::primitives::{
    BeefyNextAuthoritySet, KeccakHasher, MmrUpdateProof, HASH_LENGTH, MMR_ROOT_ID, SIGNATURE_LEN,
};
use crate::traits::{StorageRead, StorageWrite};
use codec::Encode;
use rs_merkle::MerkleProof;
use sp_core::{ByteArray, H256};
use sp_core_hashing::keccak_256;
use sp_io::crypto;
use sp_runtime::traits::Convert;

use sp_std::prelude::*;

pub trait BeefyLightClient {
    type Store: StorageRead + StorageWrite;

    /// This should verify the signed commitment signatures, and reconstruct the
    /// authority merkle root, confirming known authorities signed the [`crate::primitives::Commitment`]
    /// then using the mmr proofs, verify the latest mmr leaf,
    /// using the latest mmr leaf to rotate its view of the next authorities.
    fn ingest_mmr_root_with_proof(mmr_update: MmrUpdateProof) -> Result<(), BeefyClientError> {
        let current_authority_set = Self::Store::current_authority_set()?;
        let next_authority_set = Self::Store::next_authority_set()?;
        let signatures_len = mmr_update.signed_commitment.signatures.len();
        let validator_set_id = mmr_update.signed_commitment.commitment.validator_set_id;

        // If signature threshold is not satisfied, return
        if !validate_sigs_against_threshold(&current_authority_set, signatures_len)
            && !validate_sigs_against_threshold(&next_authority_set, signatures_len)
        {
            return Err(BeefyClientError::InvalidMmrUpdate);
        }

        if current_authority_set.id != validator_set_id && next_authority_set.id != validator_set_id
        {
            return Err(BeefyClientError::InvalidMmrUpdate);
        }

        let mmr_root_vec = mmr_update
            .signed_commitment
            .commitment
            .payload
            .get_raw(&MMR_ROOT_ID)
            .ok_or_else(|| BeefyClientError::InvalidMmrUpdate)?
            .clone();
        // Return if mmr_root_hash is invalid
        if mmr_root_vec.len() != HASH_LENGTH {
            return Err(BeefyClientError::InvalidRootHash);
        }
        let mut mmr_root_hash = [0u8; 32];
        mmr_root_hash.copy_from_slice(&mmr_root_vec);

        // Beefy validators sign the keccak_256 hash of the scale encoded commitment
        let encoded_commitment = mmr_update.signed_commitment.commitment.encode();
        let commitment_hash = keccak_256(&*encoded_commitment);

        let authority_addresses_and_indices = mmr_update
            .signed_commitment
            .signatures
            .into_iter()
            .enumerate()
            .filter_map(|item| {
                if let Some(sig) = item.1 {
                    let mut temp_sig = [0u8; SIGNATURE_LEN];
                    if sig.len() != SIGNATURE_LEN {
                        return None;
                    }
                    temp_sig.copy_from_slice(&sig);
                    Some((item.0, temp_sig))
                } else {
                    None
                }
            })
            .map(|(idx, sig)| {
                crypto::secp256k1_ecdsa_recover_compressed(&sig, &commitment_hash)
                    .map(|public_key_bytes| {
                        beefy_primitives::crypto::AuthorityId::from_slice(&public_key_bytes).ok()
                    })
                    .ok()
                    .flatten()
                    .map(|pub_key| (idx, beefy_mmr::BeefyEcdsaToEthereum::convert(pub_key)))
                    .ok_or_else(|| BeefyClientError::InvalidSignature)
            })
            .collect::<Result<Vec<_>, BeefyClientError>>()?;

        let mut authorities_changed = false;

        let authorities_merkle_proof = MerkleProof::<KeccakHasher>::new(
            mmr_update
                .authority_proof
                .into_iter()
                .map(|x| x.into())
                .collect(),
        );
        let authority_leaf_indices = authority_addresses_and_indices
            .iter()
            .cloned()
            .map(|x| x.0 as usize)
            .collect::<Vec<_>>();
        let authority_leaves = authority_addresses_and_indices
            .into_iter()
            .map(|x| keccak_256(&x.1).into())
            .collect::<Vec<_>>();

        // Verify mmr_update.authority_proof against store root hash
        if current_authority_set.id == validator_set_id {
            let root_hash = current_authority_set.root;
            if !authorities_merkle_proof.verify(
                root_hash.into(),
                &authority_leaf_indices,
                &authority_leaves,
                current_authority_set.len as usize,
            ) {
                return Err(BeefyClientError::InvalidAuthorityProof);
            }
        } else if next_authority_set.id == validator_set_id {
            let root_hash = next_authority_set.root;
            if !authorities_merkle_proof.verify(
                root_hash.into(),
                &authority_leaf_indices,
                &authority_leaves,
                next_authority_set.len as usize,
            ) {
                return Err(BeefyClientError::InvalidAuthorityProof);
            }
            authorities_changed = true;
        }

        let latest_beefy_height = Self::Store::latest_height()?;

        if mmr_update.signed_commitment.commitment.block_number <= latest_beefy_height {
            return Err(BeefyClientError::InvalidMmrUpdate);
        }

        // Move on to verify mmr_proof

        let proof = pallet_mmr_primitives::Proof {
            leaf_index: mmr_update.latest_mmr_leaf_with_index.index,
            // we treat this leaf as the latest leaf in the mmr
            leaf_count: mmr_update.latest_mmr_leaf_with_index.index + 1,
            items: mmr_update.mmr_proof.clone(),
        };

        let encodable_opaque_leaf = pallet_mmr_primitives::EncodableOpaqueLeaf(
            mmr_update.latest_mmr_leaf_with_index.leaf.encode(),
        );

        let node =
            pallet_mmr_primitives::DataOrHash::Data(encodable_opaque_leaf.into_opaque_leaf());
        match pallet_mmr::verify_leaf_proof::<sp_runtime::traits::Keccak256, _>(
            mmr_root_hash.into(),
            node,
            proof,
        ) {
            Err(_) => return Err(BeefyClientError::InvalidMmrProof),
            _ => {}
        }

        Self::Store::set_latest_height(mmr_update.signed_commitment.commitment.block_number)?;
        Self::Store::set_latest_mmr_root_hash(mmr_root_hash.into())?;

        if authorities_changed {
            Self::Store::set_current_authority_set(next_authority_set)?;
            Self::Store::set_next_authority_set(
                mmr_update
                    .latest_mmr_leaf_with_index
                    .leaf
                    .beefy_next_authority_set
                    .clone(),
            )?;
        }
        Ok(())
    }
}

fn authority_threshold(set: &BeefyNextAuthoritySet<H256>) -> u32 {
    ((2 * set.len) / 3) + 1
}

fn validate_sigs_against_threshold(set: &BeefyNextAuthoritySet<H256>, sigs_len: usize) -> bool {
    let threshold = authority_threshold(set);
    sigs_len >= threshold as usize
}
