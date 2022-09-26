use codec::Encode;
use ibc::{
	core::{
		ics23_commitment::commitment::CommitmentPrefix,
		ics24_host::identifier::{ChannelId, ClientId, ConnectionId, PortId},
	},
	events::IbcEvent,
	Height,
};
use std::{
	collections::{BTreeSet, HashMap},
	fmt::Display,
};

use ibc_proto::ibc::core::{
	channel::v1::{
		QueryChannelResponse, QueryNextSequenceReceiveResponse, QueryPacketAcknowledgementResponse,
		QueryPacketCommitmentResponse, QueryPacketReceiptResponse,
	},
	client::v1::{QueryClientStateResponse, QueryConsensusStateResponse},
	connection::v1::QueryConnectionResponse,
};
use sp_runtime::{
	traits::{Header as HeaderT, IdentifyAccount, Verify},
	MultiSignature, MultiSigner,
};
use subxt::Config;

use super::{error::Error, ParachainClient};
use ibc::core::ics02_client::client_state::ClientState as _;
use ibc_rpc::{BlockNumberOrHash, IbcApiClient, PacketInfo};
use primitives::{
	find_maximum_height_for_timeout_proofs, Chain, IbcProvider, KeyProvider, UpdateType,
};
use sp_core::H256;

use crate::{parachain, parachain::api::runtime_types::primitives::currency::CurrencyId};
use ibc::{
	applications::transfer::{Amount, PrefixedCoin, PrefixedDenom},
	core::ics02_client::{client_state::ClientType, msgs::update_client::MsgUpdateAnyClient},
	timestamp::Timestamp,
	tx_msg::Msg,
};
use ibc_proto::{
	google::protobuf::Any,
	ibc::core::{channel::v1::QueryChannelsResponse, client::v1::IdentifiedClientState},
};

#[cfg(feature = "beefy")]
use beefy_light_client_primitives::{ClientState as BeefyPrimitivesClientState, NodesUtils};
use beefy_prover::helpers::fetch_timestamp_extrinsic_with_proof;
use grandpa_light_client_primitives::{FinalityProof, ParachainHeaderProofs};
#[cfg(feature = "beefy")]
use ics11_beefy::{
	client_message::{BeefyHeader, ClientMessage as BeefyClientMessage, ParachainHeadersWithProof},
	client_state::ClientState as BeefyClientState,
};
use pallet_ibc::{
	light_clients::{AnyClientMessage, AnyClientState, HostFunctionsManager},
	HostConsensusProof,
};
use primitives::mock::LocalClientTypes;
use sp_runtime::traits::One;
use std::{collections::BTreeMap, str::FromStr, time::Duration};
use tendermint_proto::Protobuf;

#[cfg(feature = "beefy")]
/// Finality event for parachains
pub type FinalityEvent =
	beefy_primitives::SignedCommitment<u32, beefy_primitives::crypto::Signature>;

#[cfg(not(feature = "beefy"))]
pub type FinalityEvent =
	grandpa_light_client::justification::GrandpaJustification<polkadot_core_primitives::Header>;

#[async_trait::async_trait]
impl<T: Config + Send + Sync> IbcProvider for ParachainClient<T>
where
	u32: From<<<T as Config>::Header as HeaderT>::Number>,
	u32: From<<T as Config>::BlockNumber>,
	Self: KeyProvider,
	<T::Signature as Verify>::Signer: From<MultiSigner> + IdentifyAccount<AccountId = T::AccountId>,
	MultiSigner: From<MultiSigner>,
	<T as subxt::Config>::Address: From<<T as subxt::Config>::AccountId>,
	T::Signature: From<MultiSignature>,
	T::BlockNumber: From<u32> + Display + Ord + sp_runtime::traits::Zero + One,
	T::Hash: From<sp_core::H256>,
	FinalityProof<sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>>:
		From<FinalityProof<T::Header>>,
	BTreeMap<sp_core::H256, ParachainHeaderProofs>:
		From<BTreeMap<<T as subxt::Config>::Hash, ParachainHeaderProofs>>,
{
	type FinalityEvent = FinalityEvent;
	type Error = Error;

	#[cfg(not(feature = "beefy"))]
	async fn query_latest_ibc_events<C>(
		&mut self,
		justification: Self::FinalityEvent,
		counterparty: &C,
	) -> Result<(Any, Vec<IbcEvent>, UpdateType), anyhow::Error>
	where
		C: Chain,
	{
		use grandpa_light_client::justification::find_scheduled_change;
		use grandpa_light_client_primitives::ParachainHeadersWithFinalityProof;
		use ics10_grandpa::client_message::{ClientMessage, Header as GrandpaHeader};

		let client_id = self.client_id();
		let latest_height = counterparty.latest_height_and_timestamp().await?.0;
		let response = counterparty.query_client_state(latest_height, client_id).await?;
		let client_state = response.client_state.ok_or_else(|| {
			Error::Custom("Received an empty client state from counterparty".to_string())
		})?;
		let client_state = AnyClientState::try_from(client_state)
			.map_err(|_| Error::Custom("Failed to decode client state".to_string()))?;
		let grandpa_client_state = match &client_state {
			AnyClientState::Grandpa(client_state) => client_state,
			c => Err(Error::ClientStateRehydration(format!(
				"Expected AnyClientState::Grandpa found: {:?}",
				c
			)))?,
		};

		// fetch the new parachain headers that have been finalized
		let headers = self
			.query_finalized_parachain_headers_between(
				justification.commit.target_hash.into(),
				grandpa_client_state.latest_relay_hash.into(),
			)
			.await?;

		log::info!(
			"Fetching events from {} for blocks {}..{}",
			self.name(),
			headers[0].number(),
			headers.last().unwrap().number()
		);

		let finalized_blocks =
			headers.iter().map(|header| u32::from(*header.number())).collect::<Vec<_>>();

		let finalized_block_numbers = finalized_blocks
			.iter()
			.filter_map(|block_number| {
				if (client_state.latest_height().revision_height as u32) < *block_number {
					Some(*block_number)
				} else {
					None
				}
			})
			.map(|h| BlockNumberOrHash::Number(h))
			.collect::<Vec<_>>();

		// 1. we should query the sink chain for any outgoing packets to the source chain and return
		// the maximum height at which we can construct non-existence proofs for all these packets
		// on the source chain
		let max_height_for_timeouts =
			find_maximum_height_for_timeout_proofs(counterparty, self).await;
		let timeout_update_required = if let Some(max_height) = max_height_for_timeouts {
			let max_height = max_height as u32;
			finalized_blocks.contains(&max_height)
		} else {
			false
		};

		let latest_finalized_block = finalized_blocks.into_iter().max().unwrap_or_default();

		let is_update_required = self.is_update_required(
			latest_finalized_block.into(),
			client_state.latest_height().revision_height,
		);

		let target = self
			.relay_client
			.rpc()
			.header(Some(justification.commit.target_hash.into()))
			.await?
			.ok_or_else(|| {
				Error::from(
					"Could not find relay chain header for justification target".to_string(),
				)
			})?;

		let authority_set_changed_scheduled = find_scheduled_change(&target).is_some();
		// if validator set has changed this is a mandatory update
		let update_type = match authority_set_changed_scheduled ||
			timeout_update_required ||
			is_update_required
		{
			true => UpdateType::Mandatory,
			false => UpdateType::Optional,
		};

		// block_number => events
		let events: HashMap<String, Vec<IbcEvent>> = IbcApiClient::<u32, H256>::query_events(
			&*self.para_client.rpc().client,
			finalized_block_numbers,
		)
		.await?;

		// header number is serialized to string
		let mut headers_with_events = events
			.iter()
			.filter_map(|(num, events)| {
				if events.is_empty() {
					None
				} else {
					str::parse::<u32>(&*num).ok().map(T::BlockNumber::from)
				}
			})
			.collect::<BTreeSet<_>>();

		let events: Vec<IbcEvent> = events.into_values().flatten().collect();

		if timeout_update_required {
			let max_height_for_timeouts = max_height_for_timeouts.unwrap();
			if max_height_for_timeouts > client_state.latest_height().revision_height {
				let max_timeout_height = T::BlockNumber::from(max_height_for_timeouts as u32);
				headers_with_events.insert(max_timeout_height);
			}
		}

		if is_update_required {
			headers_with_events.insert(T::BlockNumber::from(latest_finalized_block));
		}

		let ParachainHeadersWithFinalityProof { finality_proof, parachain_headers } = self
			.query_grandpa_finalized_parachain_headers_with_proof(
				justification.commit.target_hash.into(),
				grandpa_client_state.latest_relay_hash.into(),
				headers_with_events.into_iter().collect(),
			)
			.await?;
		let grandpa_header = GrandpaHeader {
			finality_proof: finality_proof.into(),
			parachain_headers: parachain_headers.into(),
		};

		for event in events.iter() {
			if self.sender.send(event.clone()).is_err() {
				log::trace!("Failed to push {event:?} to stream, no active receiver found");
				break
			}
		}

		let update_header = {
			let msg = MsgUpdateAnyClient::<LocalClientTypes> {
				client_id: self.client_id(),
				client_message: AnyClientMessage::Grandpa(ClientMessage::Header(grandpa_header)),
				signer: counterparty.account_id(),
			};
			let value = msg.encode_vec();
			Any { value, type_url: msg.type_url() }
		};

		Ok((update_header, events, update_type))
	}

	#[cfg(feature = "beefy")]
	async fn query_latest_ibc_events<C>(
		&mut self,
		signed_commitment: Self::FinalityEvent,
		counterparty: &C,
	) -> Result<(Any, Vec<IbcEvent>, UpdateType), anyhow::Error>
	where
		C: Chain,
	{
		let client_id = self.client_id();
		let latest_height = counterparty.latest_height_and_timestamp().await?.0;
		let response = counterparty.query_client_state(latest_height, client_id).await?;
		let client_state = response.client_state.ok_or_else(|| {
			Error::Custom("Received an empty client state from counterparty".to_string())
		})?;
		let client_state = AnyClientState::try_from(client_state)
			.map_err(|_| Error::Custom("Failed to decode client state".to_string()))?;
		let beefy_client_state = match &client_state {
			AnyClientState::Beefy(client_state) => BeefyPrimitivesClientState {
				latest_beefy_height: client_state.latest_beefy_height,
				mmr_root_hash: client_state.mmr_root_hash,
				current_authorities: client_state.authority.clone(),
				next_authorities: client_state.next_authority_set.clone(),
				beefy_activation_block: client_state.beefy_activation_block,
			},
			c => Err(Error::ClientStateRehydration(format!(
				"Expected AnyClientState::Beefy found: {:?}",
				c
			)))?,
		};

		if signed_commitment.commitment.validator_set_id < beefy_client_state.current_authorities.id
		{
			log::info!(
				"Commitment: {:#?}\nClientState: {:#?}",
				signed_commitment.commitment,
				beefy_client_state
			);
			// If validator set id of signed commitment is less than current validator set id we
			// have Then commitment is outdated and we skip it.
			log::warn!(
				"Skipping outdated commitment \n Received signed commitmment with validator_set_id: {:?}\n Current authority set id: {:?}\n Next authority set id: {:?}\n",
				signed_commitment.commitment.validator_set_id, beefy_client_state.current_authorities.id, beefy_client_state.next_authorities.id
			);
			Err(Error::HeaderConstruction("Received an outdated beefy commitment".to_string()))?
		}

		// fetch the new parachain headers that have been finalized
		let headers = self
			.query_finalized_parachain_headers_at(
				signed_commitment.commitment.block_number,
				&beefy_client_state,
			)
			.await?;

		log::info!(
			"Fetching events from {} for blocks {}..{}",
			self.name(),
			headers[0].number(),
			headers.last().unwrap().number()
		);

		// Get finalized parachain block numbers, but only those higher than the latest para
		// height recorded in the on-chain client state, because in some cases a parachain
		// block that was already finalized in a former beefy block might still be part of
		// the parachain headers in a later beefy block, discovered this from previous logs
		let finalized_blocks =
			headers.iter().map(|header| u32::from(*header.number())).collect::<Vec<_>>();

		let finalized_block_numbers = finalized_blocks
			.iter()
			.filter_map(|block_number| {
				if (client_state.latest_height().revision_height as u32) < *block_number {
					Some(*block_number)
				} else {
					None
				}
			})
			.map(|h| BlockNumberOrHash::Number(h))
			.collect::<Vec<_>>();

		// 1. we should query the sink chain for any outgoing packets to the source chain and return
		// the maximum height at which we can construct non-existence proofs for all these packets
		// on the source chain
		let max_height_for_timeouts =
			find_maximum_height_for_timeout_proofs(counterparty, self).await;
		let timeout_update_required = if let Some(max_height) = max_height_for_timeouts {
			let max_height = max_height as u32;
			finalized_blocks.contains(&max_height)
		} else {
			false
		};

		let latest_finalized_block = finalized_blocks.into_iter().max().unwrap_or_default();

		let authority_set_changed =
			signed_commitment.commitment.validator_set_id == beefy_client_state.next_authorities.id;

		let is_update_required = self.is_update_required(
			latest_finalized_block.into(),
			client_state.latest_height().revision_height,
		);

		// if validator set has changed this is a mandatory update
		let update_type =
			match authority_set_changed || timeout_update_required || is_update_required {
				true => UpdateType::Mandatory,
				false => UpdateType::Optional,
			};

		// block_number => events
		let events: HashMap<String, Vec<IbcEvent>> = IbcApiClient::<u32, H256>::query_events(
			&*self.para_client.rpc().client,
			finalized_block_numbers,
		)
		.await?;

		// header number is serialized to string
		let mut headers_with_events = events
			.iter()
			.filter_map(|(num, events)| {
				if events.is_empty() {
					None
				} else {
					str::parse::<u32>(&*num).ok().map(T::BlockNumber::from)
				}
			})
			.collect::<BTreeSet<_>>();

		let events: Vec<IbcEvent> = events.into_values().flatten().collect();

		if timeout_update_required {
			let max_height_for_timeouts = max_height_for_timeouts.unwrap();
			if max_height_for_timeouts > client_state.latest_height().revision_height {
				let max_timeout_height = T::BlockNumber::from(max_height_for_timeouts as u32);
				headers_with_events.insert(max_timeout_height);
			}
		}

		if is_update_required {
			headers_with_events.insert(T::BlockNumber::from(latest_finalized_block));
		}

		// only query proofs for headers that actually have events or are mandatory
		let headers_with_proof = if !headers_with_events.is_empty() {
			let (headers, batch_proof) = self
				.query_beefy_finalized_parachain_headers_with_proof(
					signed_commitment.commitment.block_number,
					&beefy_client_state,
					headers_with_events.into_iter().collect(),
				)
				.await?;
			let mmr_size = NodesUtils::new(batch_proof.leaf_count).size();

			Some(ParachainHeadersWithProof {
				headers,
				mmr_size,
				mmr_proofs: batch_proof.items.into_iter().map(|item| item.encode()).collect(),
			})
		} else {
			None
		};

		let mmr_update =
			self.fetch_mmr_update_proof_for(signed_commitment, &beefy_client_state).await?;
		let beefy_header = BeefyHeader { headers_with_proof, mmr_update_proof: Some(mmr_update) };

		for event in events.iter() {
			if self.sender.send(event.clone()).is_err() {
				log::trace!("Failed to push {event:?} to stream, no active receiver found");
				break
			}
		}

		let update_header = {
			let msg = MsgUpdateAnyClient::<LocalClientTypes> {
				client_id: self.client_id(),
				client_message: AnyClientMessage::Beefy(BeefyClientMessage::Header(beefy_header)),
				signer: counterparty.account_id(),
			};
			let value = msg.encode_vec();
			Any { value, type_url: msg.type_url() }
		};

		Ok((update_header, events, update_type))
	}

	async fn query_client_consensus(
		&self,
		at: Height,
		client_id: ClientId,
		consensus_height: Height,
	) -> Result<QueryConsensusStateResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_client_consensus_state(
			&*self.para_client.rpc().client,
			Some(at.revision_height as u32),
			client_id.to_string(),
			consensus_height.revision_height,
			consensus_height.revision_number,
			false,
		)
		.await?;
		Ok(res)
	}

	async fn query_client_state(
		&self,
		at: Height,
		client_id: ClientId,
	) -> Result<QueryClientStateResponse, Self::Error> {
		let response = IbcApiClient::<u32, H256>::query_client_state(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			client_id.to_string(),
		)
		.await?;
		Ok(response)
	}

	async fn query_connection_end(
		&self,
		at: Height,
		connection_id: ConnectionId,
	) -> Result<QueryConnectionResponse, Self::Error> {
		let response = IbcApiClient::<u32, H256>::query_connection(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			connection_id.to_string(),
		)
		.await?;
		Ok(response)
	}

	async fn query_channel_end(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
	) -> Result<QueryChannelResponse, Self::Error> {
		let response = IbcApiClient::<u32, H256>::query_channel(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
		)
		.await?;
		Ok(response)
	}

	async fn query_proof(&self, at: Height, keys: Vec<Vec<u8>>) -> Result<Vec<u8>, Self::Error> {
		let proof = IbcApiClient::<u32, H256>::query_proof(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			keys,
		)
		.await?;

		Ok(proof.proof)
	}

	async fn query_packet_commitment(
		&self,
		at: Height,
		port_id: &PortId,
		channel_id: &ChannelId,
		seq: u64,
	) -> Result<QueryPacketCommitmentResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_packet_commitment(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
			seq,
		)
		.await?;
		Ok(res)
	}

	async fn query_packet_acknowledgement(
		&self,
		at: Height,
		port_id: &PortId,
		channel_id: &ChannelId,
		seq: u64,
	) -> Result<QueryPacketAcknowledgementResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_packet_acknowledgement(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
			seq,
		)
		.await?;
		Ok(res)
	}

	async fn query_next_sequence_recv(
		&self,
		at: Height,
		port_id: &PortId,
		channel_id: &ChannelId,
	) -> Result<QueryNextSequenceReceiveResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_next_seq_recv(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
		)
		.await?;
		Ok(res)
	}

	async fn query_packet_receipt(
		&self,
		at: Height,
		port_id: &PortId,
		channel_id: &ChannelId,
		seq: u64,
	) -> Result<QueryPacketReceiptResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_packet_receipt(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
			seq,
		)
		.await?;
		Ok(res)
	}

	async fn latest_height_and_timestamp(&self) -> Result<(Height, Timestamp), Self::Error> {
		let finalized_header = self
			.para_client
			.rpc()
			.header(None)
			.await?
			.ok_or_else(|| Error::Custom("Latest height query returned None".to_string()))?;
		let latest_height = *finalized_header.number();
		let height = Height::new(self.para_id.into(), latest_height.into());

		let api = self
			.para_client
			.clone()
			.to_runtime_api::<parachain::api::RuntimeApi<T, subxt::PolkadotExtrinsicParams<_>>>();
		let block_hash = finalized_header.hash();
		let unix_timestamp_millis = api.storage().timestamp().now(Some(block_hash)).await?;
		let timestamp_nanos = Duration::from_millis(unix_timestamp_millis).as_nanos() as u64;

		Ok((height, Timestamp::from_nanoseconds(timestamp_nanos)?))
	}

	async fn query_packet_commitments(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
	) -> Result<Vec<u64>, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_packet_commitments(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
		)
		.await?;
		Ok(res.commitments.into_iter().map(|packet_state| packet_state.sequence).collect())
	}

	async fn query_packet_acknowledgements(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
	) -> Result<Vec<u64>, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_packet_acknowledgements(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
		)
		.await?;
		Ok(res
			.acknowledgements
			.into_iter()
			.map(|packet_state| packet_state.sequence)
			.collect())
	}

	async fn query_unreceived_packets(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
		seqs: Vec<u64>,
	) -> Result<Vec<u64>, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_unreceived_packets(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
			seqs,
		)
		.await?;
		Ok(res)
	}

	async fn query_unreceived_acknowledgements(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
		seqs: Vec<u64>,
	) -> Result<Vec<u64>, Self::Error> {
		let res = IbcApiClient::<u32, H256>::query_unreceived_acknowledgements(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
			seqs,
		)
		.await?;
		Ok(res)
	}

	fn channel_whitelist(&self) -> Vec<(ChannelId, PortId)> {
		self.channel_whitelist.clone()
	}

	async fn query_connection_channels(
		&self,
		at: Height,
		connection_id: &ConnectionId,
	) -> Result<QueryChannelsResponse, Self::Error> {
		let response = IbcApiClient::<u32, H256>::query_connection_channels(
			&*self.para_client.rpc().client,
			at.revision_height as u32,
			connection_id.to_string(),
		)
		.await?;
		Ok(response)
	}

	async fn query_send_packets(
		&self,
		channel_id: ChannelId,
		port_id: PortId,
		seqs: Vec<u64>,
	) -> Result<Vec<PacketInfo>, Self::Error> {
		let response = IbcApiClient::<u32, H256>::query_send_packets(
			&*self.para_client.rpc().client,
			channel_id.to_string(),
			port_id.to_string(),
			seqs,
		)
		.await?;
		Ok(response)
	}

	async fn query_recv_packets(
		&self,
		channel_id: ChannelId,
		port_id: PortId,
		seqs: Vec<u64>,
	) -> Result<Vec<PacketInfo>, Self::Error> {
		let response = IbcApiClient::<u32, H256>::query_recv_packets(
			&*self.para_client.rpc().client,
			channel_id.to_string(),
			port_id.to_string(),
			seqs,
		)
		.await?;
		Ok(response)
	}

	fn expected_block_time(&self) -> Duration {
		// Parachains have an expected block time of 12 seconds
		Duration::from_secs(12)
	}

	async fn query_client_update_time_and_height(
		&self,
		client_id: ClientId,
		client_height: Height,
	) -> Result<(Height, Timestamp), Self::Error> {
		let response = IbcApiClient::<u32, H256>::query_client_update_time_and_height(
			&*self.para_client.rpc().client,
			client_id.to_string(),
			client_height.revision_number,
			client_height.revision_height,
		)
		.await?;
		Ok((
			response.height.into(),
			Timestamp::from_nanoseconds(response.timestamp)
				.map_err(|_| Error::Custom("Received invalid timestamp".to_string()))?,
		))
	}

	async fn query_host_consensus_state_proof(
		&self,
		height: Height,
	) -> Result<Option<Vec<u8>>, Self::Error> {
		let hash = self.para_client.rpc().block_hash(Some(height.revision_height.into())).await?;
		let header = self
			.para_client
			.rpc()
			.header(hash)
			.await?
			.ok_or_else(|| Error::Custom("Latest height query returned None".to_string()))?;
		let extrinsic_with_proof =
			fetch_timestamp_extrinsic_with_proof(&self.para_client, Some(header.hash()))
				.await
				.map_err(Error::BeefyProver)?;

		let host_consensus_proof = HostConsensusProof {
			header: header.encode(),
			extrinsic: extrinsic_with_proof.ext,
			extrinsic_proof: extrinsic_with_proof.proof,
		};
		Ok(Some(host_consensus_proof.encode()))
	}

	async fn query_ibc_balance(&self) -> Result<Vec<PrefixedCoin>, Self::Error> {
		let api = self
			.para_client
			.clone()
			.to_runtime_api::<parachain::api::RuntimeApi<T, subxt::PolkadotExtrinsicParams<_>>>();

		let account = self.public_key.clone().into_account();
		let balance = api.storage().tokens().accounts(&account, &CurrencyId(1), None).await?;

		Ok(vec![PrefixedCoin {
			denom: PrefixedDenom::from_str("PICA")?,
			amount: Amount::from_str(&format!("{}", balance.free))?,
		}])
	}

	fn connection_prefix(&self) -> CommitmentPrefix {
		CommitmentPrefix::try_from(self.commitment_prefix.clone()).expect("Should not fail")
	}

	fn client_id(&self) -> ClientId {
		self.client_id()
	}

	#[cfg(not(feature = "beefy"))]
	fn client_type(&self) -> ClientType {
		use ics10_grandpa::client_state::ClientState as GrandpaClientState;
		GrandpaClientState::<HostFunctionsManager>::client_type()
	}

	#[cfg(feature = "beefy")]
	fn client_type(&self) -> ClientType {
		BeefyClientState::<HostFunctionsManager>::client_type()
	}

	async fn query_timestamp_at(&self, block_number: u64) -> Result<u64, Self::Error> {
		let api = self
			.para_client
			.clone()
			.to_runtime_api::<parachain::api::RuntimeApi<T, subxt::PolkadotExtrinsicParams<_>>>();
		let block_hash = self.para_client.rpc().block_hash(Some(block_number.into())).await?;
		let unix_timestamp_millis = api.storage().timestamp().now(block_hash).await?;
		let timestamp_nanos = Duration::from_millis(unix_timestamp_millis).as_nanos() as u64;
		Ok(timestamp_nanos)
	}

	async fn query_clients(&self) -> Result<Vec<ClientId>, Self::Error> {
		let response: Vec<IdentifiedClientState> =
			IbcApiClient::<u32, H256>::query_clients(&*self.para_client.rpc().client).await?;
		response
			.into_iter()
			.map(|client| {
				ClientId::from_str(&client.client_id)
					.map_err(|_| Error::Custom("Invalid client id ".to_string()))
			})
			.collect()
	}

	async fn query_channels(&self) -> Result<Vec<(ChannelId, PortId)>, Self::Error> {
		let response =
			IbcApiClient::<u32, H256>::query_channels(&*self.para_client.rpc().client).await?;
		response
			.channels
			.into_iter()
			.map(|identified_chan| {
				Ok((
					ChannelId::from_str(&identified_chan.channel_id)
						.expect("Failed to convert invalid string to channel id"),
					PortId::from_str(&identified_chan.port_id)
						.expect("Failed to convert invalid string to port id"),
				))
			})
			.collect::<Result<Vec<_>, _>>()
	}

	fn is_update_required(
		&self,
		latest_height: u64,
		latest_client_height_on_counterparty: u64,
	) -> bool {
		let refresh_period: u64 = if cfg!(feature = "testing") { 15 } else { 50 };
		latest_height - latest_client_height_on_counterparty >= refresh_period
	}
}
