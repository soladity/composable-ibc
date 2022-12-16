// Copyright 2022 ComposableFi
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{format, Config};
use frame_support::storage::{child, child::ChildInfo};
use ibc::{
	core::ics24_host::{identifier::ClientId, path::ClientConsensusStatePath},
	Height,
};
use ibc_primitives::apply_prefix;
use sp_std::{marker::PhantomData, prelude::*};

/// client_id, height => consensus_state
/// trie key path: "clients/{client_id}/consensusStates/{height}"
/// todo: only store up to 250 (height => consensus_state) per client_id
pub struct ConsensusStates<T>(PhantomData<T>);

impl<T: Config> ConsensusStates<T> {
	pub fn get(client_id: ClientId, height: Height) -> Option<Vec<u8>> {
		let consensus_path = ClientConsensusStatePath {
			client_id,
			epoch: height.revision_number,
			height: height.revision_height,
		};
		let path = format!("{}", consensus_path);
		let key = apply_prefix(T::PALLET_PREFIX, vec![path]);
		child::get(&ChildInfo::new_default(T::PALLET_PREFIX), &key)
	}

	pub fn insert(client_id: ClientId, height: Height, consensus_state: Vec<u8>) {
		let consensus_path = ClientConsensusStatePath {
			client_id,
			epoch: height.revision_number,
			height: height.revision_height,
		};
		let path = format!("{}", consensus_path);
		let key = apply_prefix(T::PALLET_PREFIX, vec![path]);
		child::put(&ChildInfo::new_default(T::PALLET_PREFIX), &key, &consensus_state)
	}
}
