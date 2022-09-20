use ibc::prelude::*;

use tendermint_proto::Protobuf;

use ibc_proto::ibc::lightclients::tendermint::v1::Misbehaviour as RawMisbehaviour;

use crate::{error::Error, header::Header};
use ibc::{core::ics24_host::identifier::ClientId, Height};

#[derive(Clone, Debug, PartialEq)]
pub struct Misbehaviour {
	pub client_id: ClientId,
	pub header1: Header,
	pub header2: Header,
}

impl ibc::core::ics02_client::misbehaviour::Misbehaviour for Misbehaviour {
	fn client_id(&self) -> &ClientId {
		&self.client_id
	}

	fn height(&self) -> Height {
		self.header1.height()
	}

	fn encode_to_vec(&self) -> Vec<u8> {
		self.encode_vec()
	}
}

impl Protobuf<RawMisbehaviour> for Misbehaviour {}

impl TryFrom<RawMisbehaviour> for Misbehaviour {
	type Error = Error;

	fn try_from(raw: RawMisbehaviour) -> Result<Self, Self::Error> {
		Ok(Self {
			client_id: Default::default(),
			header1: raw
				.header_1
				.ok_or_else(|| Error::invalid_raw_misbehaviour("missing header1".into()))?
				.try_into()?,
			header2: raw
				.header_2
				.ok_or_else(|| Error::invalid_raw_misbehaviour("missing header2".into()))?
				.try_into()?,
		})
	}
}

impl From<Misbehaviour> for RawMisbehaviour {
	fn from(value: Misbehaviour) -> Self {
		RawMisbehaviour {
			client_id: value.client_id.to_string(),
			header_1: Some(value.header1.into()),
			header_2: Some(value.header2.into()),
		}
	}
}

impl core::fmt::Display for Misbehaviour {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
		write!(
			f,
			"{:?} h1: {:?}-{:?} h2: {:?}-{:?}",
			self.client_id,
			self.header1.height(),
			self.header1.trusted_height,
			self.header2.height(),
			self.header2.trusted_height,
		)
	}
}
