// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

//! The block builder runtime api.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use codec::{Decode, Encode};
use core::time::Duration;
use sp_inherents::{CheckInherentsResult, InherentData};
use sp_runtime::{traits::Block as BlockT, ApplyExtrinsicResult};
#[cfg(feature = "std")]
mod client_side;

#[cfg(feature = "std")]
pub use client_side::*;

#[derive(Encode, Decode, scale_info::TypeInfo, Debug)]
pub struct BlockRate {
	/// Time between individual blocks.
	pub block_time: BlockTime,
	/// Maximum time to spend building per block.
	pub block_building_time: Duration,
}

#[derive(Encode, Decode, scale_info::TypeInfo, Debug)]
pub enum BlockTime {
	/// Blocks are expected every X.
	Regularly {
		/// Time between blocks.
		every: Duration,
	},
	/// Blocks are coming at unexpected times.
	Irregular,
}

impl BlockTime {
	pub fn as_regular(&self) -> Option<Duration> {
		match self {
			Self::Regularly { every } => Some(*every),
			Self::Irregular => None,
		}
	}
}

sp_api::decl_runtime_apis! {
	/// The `BlockBuilder` api trait that provides the required functionality for building a block.
	#[api_version(6)]
	pub trait BlockBuilder {
		/// Apply the given extrinsic.
		///
		/// Returns an inclusion outcome which specifies if this extrinsic is included in
		/// this block or not.
		fn apply_extrinsic(extrinsic: <Block as BlockT>::Extrinsic) -> ApplyExtrinsicResult;

		#[changed_in(6)]
		fn apply_extrinsic(
			extrinsic: <Block as BlockT>::Extrinsic,
		) -> sp_runtime::legacy::byte_sized_error::ApplyExtrinsicResult;

		/// Finish the current block.
		#[renamed("finalise_block", 3)]
		fn finalize_block() -> <Block as BlockT>::Header;

		/// Generate inherent extrinsics. The inherent data will vary from chain to chain.
		fn inherent_extrinsics(
			inherent: InherentData,
		) -> alloc::vec::Vec<<Block as BlockT>::Extrinsic>;

		/// Check that the inherents are valid. The inherent data will vary from chain to chain.
		fn check_inherents(block: Block, data: InherentData) -> CheckInherentsResult;
	}
}
