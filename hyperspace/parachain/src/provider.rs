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

use super::{error::Error, ParachainClient};
use crate::{
	config, finality_protocol::FinalityEvent, parachain, utils::MetadataIbcEventWrapper,
	FinalityProtocol, GrandpaClientState,
};
use beefy_prover::helpers::fetch_timestamp_extrinsic_with_proof;
use codec::Encode;
use finality_grandpa::BlockNumberOps;
use futures::Stream;
use grandpa_light_client_primitives::ParachainHeaderProofs;
use ibc::{
	applications::transfer::{Amount, PrefixedCoin, PrefixedDenom},
	core::{
		ics02_client::client_state::ClientType,
		ics23_commitment::commitment::CommitmentPrefix,
		ics24_host::identifier::{ChannelId, ClientId, ConnectionId, PortId},
	},
	events::IbcEvent,
	timestamp::Timestamp,
	Height,
};
use ibc_proto::{
	google::protobuf::Any,
	ibc::core::{
		channel::v1::{
			QueryChannelResponse, QueryChannelsResponse, QueryNextSequenceReceiveResponse,
			QueryPacketAcknowledgementResponse, QueryPacketCommitmentResponse,
			QueryPacketReceiptResponse,
		},
		client::v1::{
			IdentifiedClientState, QueryClientStateResponse, QueryConsensusStateResponse,
		},
		connection::v1::{IdentifiedConnection, QueryConnectionResponse},
	},
};
use ibc_rpc::{IbcApiClient, PacketInfo};
use ics11_beefy::client_state::ClientState as BeefyClientState;
use pallet_ibc::{
	light_clients::{AnyClientState, AnyConsensusState, HostFunctionsManager},
	HostConsensusProof,
};
use primitives::{Chain, IbcProvider, KeyProvider, UpdateType};
use sp_core::H256;
use sp_runtime::{
	traits::{IdentifyAccount, One, Verify},
	MultiSignature, MultiSigner,
};
use std::{collections::BTreeMap, fmt::Display, pin::Pin, str::FromStr, time::Duration};

use subxt::config::{
	extrinsic_params::BaseExtrinsicParamsBuilder, ExtrinsicParams, Header as HeaderT,
};

#[cfg(not(feature = "dali"))]
use subxt::config::polkadot::PlainTip as Tip;

#[cfg(feature = "dali")]
use subxt::config::substrate::AssetTip as Tip;
use tokio_stream::wrappers::ReceiverStream;

pub struct TransactionId<Hash> {
	pub ext_hash: Hash,
	pub block_hash: Hash,
}

#[async_trait::async_trait]
impl<T: config::Config + Send + Sync> IbcProvider for ParachainClient<T>
where
	u32: From<<<T as subxt::Config>::Header as HeaderT>::Number>,
	u32: From<<T as subxt::Config>::BlockNumber>,
	Self: KeyProvider,
	<<T as config::Config>::Signature as Verify>::Signer:
		From<MultiSigner> + IdentifyAccount<AccountId = T::AccountId>,
	MultiSigner: From<MultiSigner>,
	<T as subxt::Config>::Address: From<<T as subxt::Config>::AccountId>,
	<T as subxt::Config>::Signature: From<MultiSignature> + Send + Sync,
	T::BlockNumber: BlockNumberOps + From<u32> + Display + Ord + sp_runtime::traits::Zero + One,
	T::Hash: From<sp_core::H256> + From<[u8; 32]>,
	sp_core::H256: From<T::Hash>,
	BTreeMap<sp_core::H256, ParachainHeaderProofs>:
		From<BTreeMap<<T as subxt::Config>::Hash, ParachainHeaderProofs>>,
	<T::ExtrinsicParams as ExtrinsicParams<T::Index, T::Hash>>::OtherParams:
		From<BaseExtrinsicParamsBuilder<T, Tip>> + Send + Sync,
	<T as subxt::Config>::AccountId: Send + Sync,
	<T as subxt::Config>::Address: Send + Sync,
{
	type FinalityEvent = FinalityEvent;
	type TransactionId = TransactionId<T::Hash>;
	type Error = Error;

	async fn query_latest_ibc_events<C>(
		&mut self,
		finality_event: Self::FinalityEvent,
		counterparty: &C,
	) -> Result<(Vec<Any>, Vec<IbcEvent>, UpdateType), anyhow::Error>
	where
		C: Chain,
	{
		self.finality_protocol
			.clone()
			.query_latest_ibc_events(self, finality_event, counterparty)
			.await
	}

	async fn ibc_events(&self) -> Pin<Box<dyn Stream<Item = IbcEvent> + Send + 'static>> {
		use futures::StreamExt;
		use pallet_ibc::events::IbcEvent as RawIbcEvent;
		let (tx, rx) = tokio::sync::mpsc::channel(32);
		let event = self.para_client.events();
		let para_client = self.para_client.clone();
		tokio::spawn(async move {
			let stream = para_client
				.blocks()
				.subscribe_all()
				.await
				.expect("should susbcribe to blocks")
				.filter_map(|block| async {
					let block = block.ok()?;
					let events = event.at(Some(block.hash())).await.ok()?;
					let result = events
						.find::<parachain::api::ibc::events::Events>()
						.filter_map(|ev| {
							let ev = ev.ok()?.events;
							ev.into_iter()
								.filter_map(|ev| {
									Some(
										IbcEvent::try_from(RawIbcEvent::from(
											MetadataIbcEventWrapper(ev.ok()?),
										))
										.map_err(|e| subxt::Error::Other(e.to_string())),
									)
								})
								.collect::<Result<Vec<_>, _>>()
								.ok()
						})
						.flatten()
						.collect::<Vec<_>>();
					Some(result)
				});

			let mut stream = Box::pin(stream);

			while let Some(evs) = stream.next().await {
				let mut should_exit = false;
				for ev in evs {
					if let Err(_) = tx.send(ev).await {
						should_exit = true;
						break
					}
				}
				if should_exit {
					break
				}
			}
		});

		Box::pin(ReceiverStream::new(rx))
	}

	async fn query_client_consensus(
		&self,
		at: Height,
		client_id: ClientId,
		consensus_height: Height,
	) -> Result<QueryConsensusStateResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_client_consensus_state(
			&*self.para_ws_client,
			Some(at.revision_height as u32),
			client_id.to_string(),
			consensus_height.revision_height,
			consensus_height.revision_number,
			false,
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(res)
	}

	async fn query_client_state(
		&self,
		at: Height,
		client_id: ClientId,
	) -> Result<QueryClientStateResponse, Self::Error> {
		let response =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_client_state(
				&*self.para_ws_client,
				at.revision_height as u32,
				client_id.to_string(),
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(response)
	}

	async fn query_connection_end(
		&self,
		at: Height,
		connection_id: ConnectionId,
	) -> Result<QueryConnectionResponse, Self::Error> {
		let response = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_connection(
			&*self.para_ws_client,
			at.revision_height as u32,
			connection_id.to_string(),
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(response)
	}

	async fn query_channel_end(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
	) -> Result<QueryChannelResponse, Self::Error> {
		let response = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_channel(
			&*self.para_ws_client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(response)
	}

	async fn query_proof(&self, at: Height, keys: Vec<Vec<u8>>) -> Result<Vec<u8>, Self::Error> {
		let proof = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_proof(
			&*self.para_ws_client,
			at.revision_height as u32,
			keys,
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(proof.proof)
	}

	async fn query_packet_commitment(
		&self,
		at: Height,
		port_id: &PortId,
		channel_id: &ChannelId,
		seq: u64,
	) -> Result<QueryPacketCommitmentResponse, Self::Error> {
		let res =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_packet_commitment(
				&*self.para_ws_client,
				at.revision_height as u32,
				channel_id.to_string(),
				port_id.to_string(),
				seq,
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(res)
	}

	async fn query_packet_acknowledgement(
		&self,
		at: Height,
		port_id: &PortId,
		channel_id: &ChannelId,
		seq: u64,
	) -> Result<QueryPacketAcknowledgementResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_packet_acknowledgement(
			&*self.para_ws_client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
			seq,
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(res)
	}

	async fn query_next_sequence_recv(
		&self,
		at: Height,
		port_id: &PortId,
		channel_id: &ChannelId,
	) -> Result<QueryNextSequenceReceiveResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_next_seq_recv(
			&*self.para_ws_client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(res)
	}

	async fn query_packet_receipt(
		&self,
		at: Height,
		port_id: &PortId,
		channel_id: &ChannelId,
		seq: u64,
	) -> Result<QueryPacketReceiptResponse, Self::Error> {
		let res = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_packet_receipt(
			&*self.para_ws_client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
			seq,
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(res)
	}

	async fn latest_height_and_timestamp(&self) -> Result<(Height, Timestamp), Self::Error> {
		let finalized_header = self
			.para_client
			.rpc()
			.header(None)
			.await?
			.ok_or_else(|| Error::Custom("Latest height query returned None".to_string()))?;
		let latest_height: u64 = (finalized_header.number()).into();
		let height = Height::new(self.para_id.into(), latest_height.into());

		let subxt_block_number: subxt::rpc::types::BlockNumber = latest_height.into();
		let block_hash = self.para_client.rpc().block_hash(Some(subxt_block_number)).await.unwrap();
		let timestamp_addr = parachain::api::storage().timestamp().now();
		let unix_timestamp_millis = self
			.para_client
			.storage()
			.at(block_hash)
			.await
			.expect("Storage client")
			.fetch(&timestamp_addr)
			.await?
			.ok_or_else(|| Error::from("Timestamp should exist".to_string()))?;
		let timestamp_nanos = Duration::from_millis(unix_timestamp_millis).as_nanos() as u64;

		Ok((height, Timestamp::from_nanoseconds(timestamp_nanos)?))
	}

	async fn query_packet_commitments(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
	) -> Result<Vec<u64>, Self::Error> {
		let res =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_packet_commitments(
				&*self.para_ws_client,
				at.revision_height as u32,
				channel_id.to_string(),
				port_id.to_string(),
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(res.commitments.into_iter().map(|packet_state| packet_state.sequence).collect())
	}

	async fn query_packet_acknowledgements(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
	) -> Result<Vec<u64>, Self::Error> {
		let res = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_packet_acknowledgements(
			&*self.para_ws_client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
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
		let res =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_unreceived_packets(
				&*self.para_ws_client,
				at.revision_height as u32,
				channel_id.to_string(),
				port_id.to_string(),
				seqs,
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(res)
	}

	async fn query_unreceived_acknowledgements(
		&self,
		at: Height,
		channel_id: ChannelId,
		port_id: PortId,
		seqs: Vec<u64>,
	) -> Result<Vec<u64>, Self::Error> {
		let res = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_unreceived_acknowledgements(
			&*self.para_ws_client,
			at.revision_height as u32,
			channel_id.to_string(),
			port_id.to_string(),
			seqs,
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
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
		let response =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_connection_channels(
				&*self.para_ws_client,
				at.revision_height as u32,
				connection_id.to_string(),
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(response)
	}

	async fn query_send_packets(
		&self,
		channel_id: ChannelId,
		port_id: PortId,
		seqs: Vec<u64>,
	) -> Result<Vec<PacketInfo>, Self::Error> {
		let response =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_send_packets(
				&*self.para_ws_client,
				channel_id.to_string(),
				port_id.to_string(),
				seqs,
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(response)
	}

	async fn query_recv_packets(
		&self,
		channel_id: ChannelId,
		port_id: PortId,
		seqs: Vec<u64>,
	) -> Result<Vec<PacketInfo>, Self::Error> {
		let response =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_recv_packets(
				&*self.para_ws_client,
				channel_id.to_string(),
				port_id.to_string(),
				seqs,
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
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
		let response = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_client_update_time_and_height(
			&*self.para_ws_client,
			client_id.to_string(),
			client_height.revision_number,
			client_height.revision_height,
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
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
		let account = self.public_key.clone().into_account();
		let account = subxt::utils::AccountId32::from(<[u8; 32]>::from(account));
		let mut hex_string = hex::encode(account.0.to_vec());
		hex_string.insert_str(0, "0x");
		let coin: ibc_proto::cosmos::base::v1beta1::Coin =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_balance_with_address(
				&*self.para_ws_client,
				hex_string,
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		Ok(vec![PrefixedCoin {
			denom: PrefixedDenom::from_str(&coin.denom)?,
			amount: Amount::from_str(&coin.amount)?,
		}])
	}

	fn connection_prefix(&self) -> CommitmentPrefix {
		CommitmentPrefix::try_from(self.commitment_prefix.clone()).expect("Should not fail")
	}

	fn client_id(&self) -> ClientId {
		self.client_id()
	}

	fn connection_id(&self) -> ConnectionId {
		self.connection_id.as_ref().expect("Connection id should be defined").clone()
	}

	fn client_type(&self) -> ClientType {
		match self.finality_protocol {
			FinalityProtocol::Grandpa => GrandpaClientState::<HostFunctionsManager>::client_type(),
			FinalityProtocol::Beefy => BeefyClientState::<HostFunctionsManager>::client_type(),
		}
	}

	async fn query_timestamp_at(&self, block_number: u64) -> Result<u64, Self::Error> {
		let subxt_block_number: subxt::rpc::types::BlockNumber = block_number.into();
		let block_hash = self.para_client.rpc().block_hash(Some(subxt_block_number)).await.unwrap();
		let timestamp_addr = parachain::api::storage().timestamp().now();
		let unix_timestamp_millis = self
			.para_client
			.storage()
			.at(block_hash)
			.await
			.expect("Storage client")
			.fetch(&timestamp_addr)
			.await?
			.expect("Timestamp should exist");
		let timestamp_nanos = Duration::from_millis(unix_timestamp_millis).as_nanos() as u64;

		Ok(timestamp_nanos)
	}

	async fn query_clients(&self) -> Result<Vec<ClientId>, Self::Error> {
		let response: Vec<IdentifiedClientState> =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_clients(
				&*self.para_ws_client,
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		response
			.into_iter()
			.map(|client| {
				ClientId::from_str(&client.client_id)
					.map_err(|_| Error::Custom("Invalid client id ".to_string()))
			})
			.collect()
	}

	async fn query_channels(&self) -> Result<Vec<(ChannelId, PortId)>, Self::Error> {
		let response = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_channels(
			&*self.para_ws_client,
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
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

	async fn query_connection_using_client(
		&self,
		height: u32,
		client_id: String,
	) -> Result<Vec<IdentifiedConnection>, Self::Error> {
		let response = IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_connection_using_client(
			&*self.para_ws_client,
			height,
			client_id,
		)
		.await
		.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;

		Ok(response)
	}

	async fn is_update_required(
		&self,
		latest_height: u64,
		latest_client_height_on_counterparty: u64,
	) -> Result<bool, Self::Error> {
		let prover = self.grandpa_prover();
		let session_length = prover
			.session_length()
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;
		// We divide the session into some places and if the diff in block updates is greater than
		// this update is required
		let base =
			if cfg!(test) { (session_length / 2) as u64 } else { (session_length / 12) as u64 };
		let diff = latest_height - latest_client_height_on_counterparty;
		Ok(base >= diff)
	}

	async fn initialize_client_state(
		&self,
	) -> Result<(AnyClientState, AnyConsensusState), Self::Error> {
		match self.finality_protocol {
			FinalityProtocol::Grandpa => {
				let res = self.construct_grandpa_client_state().await?;
				Ok(res)
			},
			FinalityProtocol::Beefy => {
				let res = self.construct_beefy_client_state().await?;
				Ok(res)
			},
		}
	}

	async fn query_client_id_from_tx_hash(
		&self,
		tx_id: Self::TransactionId,
	) -> Result<ClientId, Self::Error> {
		// Query newly created client Id
		let TransactionId { ext_hash, block_hash } = tx_id;
		let identified_client_state =
			IbcApiClient::<u32, H256, <T as config::Config>::AssetId>::query_newly_created_client(
				&*self.para_ws_client,
				block_hash.into(),
				ext_hash.into(),
			)
			.await
			.map_err(|e| Error::from(format!("Rpc Error {:?}", e)))?;

		let client_id = ClientId::from_str(&identified_client_state.client_id)
			.expect("Should have a valid client id");
		Ok(client_id)
	}
}
