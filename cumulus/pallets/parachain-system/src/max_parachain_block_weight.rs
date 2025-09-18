// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! Utilities for calculating maximum parachain block weight based on core assignments.

use crate::Config;
use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode};
use cumulus_primitives_core::CumulusDigestItem;
use frame_support::{
	dispatch::{DispatchInfo, PostDispatchInfo},
	pallet_prelude::{
		InvalidTransaction, TransactionSource, TransactionValidityError, ValidTransaction,
	},
	traits::PreInherents,
	weights::{constants::WEIGHT_REF_TIME_PER_SECOND, Weight},
};
use polkadot_primitives::MAX_POV_SIZE;
use scale_info::TypeInfo;
use sp_core::Get;
use sp_runtime::{
	traits::{DispatchInfoOf, Dispatchable, Implication, PostDispatchInfoOf, TransactionExtension},
	Digest, DispatchResult,
};

const LOG_TARGET: &str = "runtime::parachain-system::block-weight";

#[derive(Debug, Encode, Decode, Clone, Copy, TypeInfo)]
pub enum BlockWeightMode {
	FullCore,
	PotentialFullCore { first_transaction_index: Option<u32> },
	FractionOfCore { first_transaction_index: Option<u32> },
}

/// A utility type for calculating the maximum block weight for a parachain based on
/// the number of relay chain cores assigned and the target number of blocks.
pub struct MaxParachainBlockWeight;

impl MaxParachainBlockWeight {
	// Maximum ref time per core
	const MAX_REF_TIME_PER_CORE_NS: u64 = 2 * WEIGHT_REF_TIME_PER_SECOND;
	const FULL_CORE_WEIGHT: Weight =
		Weight::from_parts(Self::MAX_REF_TIME_PER_CORE_NS, MAX_POV_SIZE as u64);

	/// Calculate the maximum block weight based on target blocks and core assignments.
	///
	/// This function examines the current block's digest from `frame_system::Digests` storage
	/// to find `CumulusDigestItem::CoreInfo` entries, which contain information about the
	/// number of relay chain cores assigned to the parachain. Each core has a maximum
	/// reference time of 2 seconds and the total maximum PoV size of `MAX_POV_SIZE` is
	/// shared across all target blocks.
	///
	/// # Parameters
	/// - `target_blocks`: The target number of blocks to be produced
	///
	/// # Returns
	/// Returns the calculated maximum weight, or a conservative default if no core info is found
	/// or if an error occurs during calculation.
	pub fn get<T: Config>(target_blocks: u32) -> Weight {
		let digest = frame_system::Pallet::<T>::digest();
		let target_block_weight =
			Self::target_block_weight_with_digest::<T>(target_blocks, &digest);

		let maybe_full_core_weight = if is_first_block_in_core_with_digest(&digest) {
			Self::FULL_CORE_WEIGHT
		} else {
			target_block_weight
		};

		// If we are in `on_initialize` or at applying the inherents, we allow the maximum block
		// weight as allowed by the current context.
		if !frame_system::Pallet::<T>::inherents_applied() {
			return maybe_full_core_weight
		}

		match crate::BlockWeightMode::<T>::get() {
			// We allow the full core.
			Some(BlockWeightMode::FullCore | BlockWeightMode::PotentialFullCore { .. }) =>
				Self::FULL_CORE_WEIGHT,
			// Let's calculate below how much weight we can use.
			Some(BlockWeightMode::FractionOfCore { .. }) => target_block_weight,
			// Either the runtime is not using the `DynamicMaxBlockWeight` extension or there is a
			// bug. The value should be set before applying the first extrinsic.
			None => maybe_full_core_weight,
		}
	}

	fn target_block_weight<T: Config>(target_blocks: u32) -> Weight {
		let digest = frame_system::Pallet::<T>::digest();
		Self::target_block_weight_with_digest::<T>(target_blocks, &digest)
	}

	fn target_block_weight_with_digest<T: Config>(target_blocks: u32, digest: &Digest) -> Weight {
		let Some(core_info) = CumulusDigestItem::find_core_info(&digest) else {
			return Self::FULL_CORE_WEIGHT;
		};

		let number_of_cores = core_info.number_of_cores.0 as u32;

		// Ensure we have at least one core and valid target blocks
		if number_of_cores == 0 || target_blocks == 0 {
			return Self::FULL_CORE_WEIGHT;
		}

		let total_ref_time =
			(number_of_cores as u64).saturating_mul(Self::MAX_REF_TIME_PER_CORE_NS);
		let ref_time_per_block = total_ref_time
			.saturating_div(target_blocks as u64)
			.min(Self::MAX_REF_TIME_PER_CORE_NS);

		let total_pov_size = (number_of_cores as u64).saturating_mul(MAX_POV_SIZE as u64);
		let proof_size_per_block = total_pov_size.saturating_div(target_blocks as u64);

		Weight::from_parts(ref_time_per_block, proof_size_per_block)
	}
}

/// Is this the first block in a core?
fn is_first_block_in_core<T: Config>() -> bool {
	let digest = frame_system::Pallet::<T>::digest();
	is_first_block_in_core_with_digest(&digest)
}

/// Is this the first block in a core? (takes digest as parameter)
fn is_first_block_in_core_with_digest(digest: &Digest) -> bool {
	CumulusDigestItem::find_bundle_info(digest).map_or(false, |bi| bi.index == 0)
}

/// Is the `BlockWeight` already above the target block weight?
fn block_weight_over_target_block_weight<T: Config, TargetBlockRate: Get<u32>>() -> bool {
	let target_block_weight =
		MaxParachainBlockWeight::target_block_weight::<T>(TargetBlockRate::get());

	frame_system::Pallet::<T>::remaining_block_weight()
		.consumed()
		.any_gt(target_block_weight)
}

pub struct MaxBlockWeightHooks<T, TargetBlockRate>(core::marker::PhantomData<(T, TargetBlockRate)>);

impl<T, TargetBlockRate> PreInherents for MaxBlockWeightHooks<T, TargetBlockRate>
where
	T: Config,
	TargetBlockRate: Get<u32>,
{
	fn pre_inherents() {
		if block_weight_over_target_block_weight::<T, TargetBlockRate>() {
			let is_first_block_in_core = is_first_block_in_core::<T>();

			if !is_first_block_in_core {
				log::error!(
					target: LOG_TARGET,
					"Inherent block logic took longer than the target block weight, THIS IS A BUG!!!",
				);
			} else {
				log::debug!(
					target: LOG_TARGET,
					"Inherent block logic took longer than the target block weight, going to use the full core",
				);
			}

			crate::BlockWeightMode::<T>::put(BlockWeightMode::FullCore);

			// Inform the node that this block uses the full core.
			frame_system::Pallet::<T>::deposit_log(CumulusDigestItem::UseFullCore.to_digest_item());
		}
	}
}

#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo)]
#[derive_where::derive_where(Clone, Eq, PartialEq, Default; S)]
#[scale_info(skip_type_params(T, TargetBlockRate))]
pub struct DynamicMaxBlockWeight<T, S, TargetBlockRate>(
	pub S,
	core::marker::PhantomData<(T, TargetBlockRate)>,
);

impl<T, S, TargetBlockRate> DynamicMaxBlockWeight<T, S, TargetBlockRate> {
	/// Create a new `StorageWeightReclaim` instance.
	pub fn new(s: S) -> Self {
		Self(s, Default::default())
	}
}

impl<T, S, TargetBlockRate> DynamicMaxBlockWeight<T, S, TargetBlockRate>
where
	T: Config,
	TargetBlockRate: Get<u32>,
{
	fn pre_validate_extrinsic(
		info: &DispatchInfo,
		len: usize,
	) -> Result<(), TransactionValidityError> {
		let is_not_inherent = frame_system::Pallet::<T>::inherents_applied();
		let extrinsic_index = is_not_inherent
			.then(|| frame_system::Pallet::<T>::extrinsic_index().unwrap_or_default());

		crate::BlockWeightMode::<T>::mutate(|mode| {
			let current_mode = *mode.get_or_insert_with(|| BlockWeightMode::FractionOfCore {
				first_transaction_index: extrinsic_index,
			});

			match current_mode {
				// We are already allowing the full core, not that much more to do here.
				BlockWeightMode::FullCore => {},
				BlockWeightMode::PotentialFullCore { first_transaction_index } |
				BlockWeightMode::FractionOfCore { first_transaction_index } => {
					let is_potential =
						matches!(current_mode, BlockWeightMode::PotentialFullCore { .. });
					debug_assert!(
						!is_potential,
						"`PotentialFullCore` should resolve to `FullCore` or `FractionOfCore` after applying a transaction.",
					);

					let block_weight_over_limit = first_transaction_index == extrinsic_index
						&& block_weight_over_target_block_weight::<T, TargetBlockRate>();

					// Protection against a misconfiguration as this should be detected by the pre-inherent hook.
					if block_weight_over_limit {
						*mode = Some(BlockWeightMode::FullCore);

						// Inform the node that this block uses the full core.
						frame_system::Pallet::<T>::deposit_log(
							CumulusDigestItem::UseFullCore.to_digest_item(),
						);

						log::error!(
							target: LOG_TARGET,
							"Inherent block logic took longer than the target block weight, \
							`MaxBlockWeightHooks` not registered as `PreInherents` hook!",
						);
					} else if info
						.total_weight()
						// The extrinsic lengths counts towards the POV size
						.saturating_add(Weight::from_parts(0, len as u64))
						.any_gt(MaxParachainBlockWeight::target_block_weight::<T>(
							TargetBlockRate::get(),
						)) && is_first_block_in_core::<T>()
					{
						// TODO: make 10 configurable
						if extrinsic_index.unwrap_or_default().saturating_sub(first_transaction_index.unwrap_or_default()) < 10 {
							*mode = Some(BlockWeightMode::PotentialFullCore {
								// While applying inherents `extrinsic_index` and `first_transaction_index` will be `None`.
								// When the first transaction is applied, we want to store the index.
								first_transaction_index: first_transaction_index.or(extrinsic_index),
							});
						} else {
							return Err(InvalidTransaction::ExhaustsResources)
						}
					} else if is_potential {
						*mode =
							Some(BlockWeightMode::FractionOfCore { first_transaction_index });
					}
				},
			};

			Ok(())
		}).map_err(Into::into)
	}

	fn post_dispatch_extrinsic() {
		crate::BlockWeightMode::<T>::mutate(|weight_mode| {
			let Some(mode) = *weight_mode else { return };

			let target_block_weight =
				MaxParachainBlockWeight::target_block_weight::<T>(TargetBlockRate::get());

			let is_above_limit = frame_system::Pallet::<T>::remaining_block_weight()
				.consumed()
				.any_gt(target_block_weight);

			match mode {
				// If the previous mode was already `FullCore`, we are fine.
				BlockWeightMode::FullCore => {},
				BlockWeightMode::FractionOfCore { .. } =>
				// If we are above the limit, it means the transaction used more weight than what it
				// had announced, which should not happen.
					if is_above_limit {
						log::error!(
							target: LOG_TARGET,
							"Extrinsic ({}) used more weight than what it had announced and pushed the \
							block above the allowed weight limit!",
							frame_system::Pallet::<T>::extrinsic_index().unwrap_or_default()
						);

						// If this isn't the first block in a core, we register the full core weight
						// to ensure that we don't include any other transactions. Because we don't
						// know how many weight of the core was already used by the blocks before.
						if !is_first_block_in_core::<T>() {
							log::error!(
								target: LOG_TARGET,
								"Registering `FULL_CORE_WEIGHT` to ensure no other transaction is included \
								in this block, because this isn't the first block in the core!",
							);

							frame_system::Pallet::<T>::register_extra_weight_unchecked(
								MaxParachainBlockWeight::FULL_CORE_WEIGHT,
								frame_support::dispatch::DispatchClass::Mandatory,
							);
						}

						*weight_mode = Some(BlockWeightMode::FullCore);

						// Inform the node that this block uses the full core.
						frame_system::Pallet::<T>::deposit_log(
							CumulusDigestItem::UseFullCore.to_digest_item(),
						);
					},
				// Now we need to check if the transaction required more weight than a fraction of a
				// core block.
				BlockWeightMode::PotentialFullCore { first_transaction_index } =>
					if is_above_limit {
						*weight_mode = Some(BlockWeightMode::FullCore);

						// Inform the node that this block uses the full core.
						frame_system::Pallet::<T>::deposit_log(
							CumulusDigestItem::UseFullCore.to_digest_item(),
						);
					} else {
						*weight_mode =
							Some(BlockWeightMode::FractionOfCore { first_transaction_index });
					},
			}
		});
	}
}

impl<T, S, TargetBlockRate> From<S> for DynamicMaxBlockWeight<T, S, TargetBlockRate> {
	fn from(s: S) -> Self {
		Self::new(s)
	}
}

impl<T, S: core::fmt::Debug, TargetBlockRate> core::fmt::Debug
	for DynamicMaxBlockWeight<T, S, TargetBlockRate>
{
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
		write!(f, "DynamicMaxBlockWeight<{:?}>", self.0)
	}
}

impl<
		T: Config + Send + Sync,
		S: TransactionExtension<T::RuntimeCall>,
		TargetBlockRate: Get<u32> + Send + Sync + 'static,
	> TransactionExtension<T::RuntimeCall> for DynamicMaxBlockWeight<T, S, TargetBlockRate>
where
	T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
{
	const IDENTIFIER: &'static str = "DynamicMaxBlockWeight<Use `metadata()`!>";

	type Implicit = S::Implicit;

	type Val = S::Val;

	type Pre = S::Pre;

	fn implicit(&self) -> Result<Self::Implicit, TransactionValidityError> {
		self.0.implicit()
	}

	fn metadata() -> Vec<sp_runtime::traits::TransactionExtensionMetadata> {
		let mut inner = S::metadata();
		inner.push(sp_runtime::traits::TransactionExtensionMetadata {
			identifier: "DynamicMaxBlockWeight",
			ty: scale_info::meta_type::<()>(),
			implicit: scale_info::meta_type::<()>(),
		});
		inner
	}

	fn weight(&self, _: &T::RuntimeCall) -> Weight {
		Weight::zero()
	}

	fn validate(
		&self,
		origin: T::RuntimeOrigin,
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
		self_implicit: Self::Implicit,
		inherited_implication: &impl Implication,
		source: TransactionSource,
	) -> Result<(ValidTransaction, Self::Val, T::RuntimeOrigin), TransactionValidityError> {
		Self::pre_validate_extrinsic(info, len)?;

		self.0
			.validate(origin, call, info, len, self_implicit, inherited_implication, source)
	}

	fn prepare(
		self,
		val: Self::Val,
		origin: &T::RuntimeOrigin,
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
	) -> Result<Self::Pre, TransactionValidityError> {
		self.0.prepare(val, origin, call, info, len)
	}

	fn post_dispatch(
		pre: Self::Pre,
		info: &DispatchInfoOf<T::RuntimeCall>,
		post_info: &mut PostDispatchInfo,
		len: usize,
		result: &DispatchResult,
	) -> Result<(), TransactionValidityError> {
		S::post_dispatch(pre, info, post_info, len, result)?;

		Self::post_dispatch_extrinsic();

		Ok(())
	}

	fn bare_validate(
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
	) -> frame_support::pallet_prelude::TransactionValidity {
		S::bare_validate(call, info, len)
	}

	fn bare_validate_and_prepare(
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
	) -> Result<(), TransactionValidityError> {
		S::bare_validate_and_prepare(call, info, len)?;

		Self::pre_validate_extrinsic(info, len)?;

		Ok(())
	}

	fn bare_post_dispatch(
		info: &DispatchInfoOf<T::RuntimeCall>,
		post_info: &mut PostDispatchInfoOf<T::RuntimeCall>,
		len: usize,
		result: &DispatchResult,
	) -> Result<(), TransactionValidityError> {
		S::bare_post_dispatch(info, post_info, len, result)?;

		Self::post_dispatch_extrinsic();

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate as parachain_system;
	use codec::Compact;
	use cumulus_primitives_core::{ClaimQueueOffset, CoreInfo, CoreSelector};
	use frame_support::{construct_runtime, derive_impl};
	use sp_io;
	use sp_runtime::{traits::IdentityLookup, BuildStorage};

	type Block = frame_system::mocking::MockBlock<Test>;

	// Configure a mock runtime to test the functionality
	#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
	impl frame_system::Config for Test {
		type Block = Block;
		type AccountId = u64;
		type AccountData = ();
		type Lookup = IdentityLookup<Self::AccountId>;
		type OnSetCode = crate::ParachainSetCode<Test>;
	}

	impl crate::Config for Test {
		type RuntimeEvent = RuntimeEvent;
		type OnSystemEvent = ();
		type SelfParaId = ();
		type OutboundXcmpMessageSource = ();
		type DmpQueue = ();
		type ReservedDmpWeight = ();
		type XcmpMessageHandler = ();
		type ReservedXcmpWeight = ();
		type CheckAssociatedRelayNumber = crate::RelayNumberStrictlyIncreases;
		type WeightInfo = ();
		type ConsensusHook = crate::ExpectParentIncluded;
		type RelayParentOffset = ();
	}

	construct_runtime!(
		pub enum Test {
			System: frame_system,
			ParachainSystem: parachain_system,
		}
	);

	fn new_test_ext_with_digest(num_cores: Option<u16>) -> sp_io::TestExternalities {
		let storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

		let mut ext = sp_io::TestExternalities::from(storage);

		ext.execute_with(|| {
			if let Some(num_cores) = num_cores {
				let core_info = CoreInfo {
					selector: CoreSelector(0),
					claim_queue_offset: ClaimQueueOffset(0),
					number_of_cores: Compact(num_cores),
				};

				let digest = CumulusDigestItem::CoreInfo(core_info).to_digest_item();

				frame_system::Pallet::<Test>::deposit_log(digest);
			}
		});

		ext
	}

	#[test]
	fn test_single_core_single_block() {
		new_test_ext_with_digest(Some(1)).execute_with(|| {
			let weight = MaxParachainBlockWeight::get::<Test>(1);

			// With 1 core and 1 target block, should get full 2s ref time and full PoV size
			assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
			assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
		});
	}

	#[test]
	fn test_single_core_multiple_blocks() {
		new_test_ext_with_digest(Some(1)).execute_with(|| {
			let weight = MaxParachainBlockWeight::get::<Test>(4);

			// With 1 core and 4 target blocks, should get 0.5s ref time and 1/4 PoV size per block
			assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND / 4);
			assert_eq!(weight.proof_size(), (1 * MAX_POV_SIZE as u64) / 4);
		});
	}

	#[test]
	fn test_multiple_cores_single_block() {
		new_test_ext_with_digest(Some(3)).execute_with(|| {
			let weight = MaxParachainBlockWeight::get::<Test>(1);

			// With 3 cores and 1 target block, should get max 2s ref time (capped per core) and 3x
			// PoV size
			assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
			assert_eq!(weight.proof_size(), 3 * MAX_POV_SIZE as u64);
		});
	}

	#[test]
	fn test_multiple_cores_multiple_blocks() {
		new_test_ext_with_digest(Some(2)).execute_with(|| {
			let weight = MaxParachainBlockWeight::get::<Test>(4);

			// With 2 cores and 4 target blocks, should get 1s ref time and 2x PoV size / 4 per
			// block
			assert_eq!(weight.ref_time(), 2 * 2 * WEIGHT_REF_TIME_PER_SECOND / 4);
			assert_eq!(weight.proof_size(), (2 * MAX_POV_SIZE as u64) / 4);
		});
	}

	#[test]
	fn test_no_core_info() {
		new_test_ext_with_digest(None).execute_with(|| {
			let weight = MaxParachainBlockWeight::get::<Test>(1);

			// Without core info, should return conservative default
			assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
			assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
		});
	}

	#[test]
	fn test_zero_cores() {
		new_test_ext_with_digest(Some(0)).execute_with(|| {
			let weight = MaxParachainBlockWeight::get::<Test>(1);

			// With 0 cores, should return conservative default
			assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
			assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
		});
	}

	#[test]
	fn test_zero_target_blocks() {
		new_test_ext_with_digest(Some(2)).execute_with(|| {
			let weight = MaxParachainBlockWeight::get::<Test>(0);

			// With 0 target blocks, should return conservative default
			assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
			assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
		});
	}
}
