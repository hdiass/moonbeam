// Copyright 2019-2021 PureStake Inc.
// This file is part of Moonbeam.

// Moonbeam is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Moonbeam is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Moonbeam.  If not, see <http://www.gnu.org/licenses/>.

//! # Liquid Staking Module
//!
//! ## Overview
//!
//! Module to provide interaction with Relay Chain Tokens directly
//! This module allows to
//! - Token transfer from parachain to relay chain.
//! - Token transfer from relay to parachain
//! - Exposure to staking functions

#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::pallet;

pub use pallet::*;
#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;

#[pallet]
pub mod pallet {

	use frame_support::dispatch::fmt::Debug;

	use frame_support::pallet_prelude::*;
	use frame_system::{ensure_signed, pallet_prelude::*};
	use orml_traits::location::{Parse, Reserve};
	use sp_runtime::traits::{AtLeast32BitUnsigned, Convert};
	use sp_std::prelude::*;

	use xcm::v1::prelude::*;

	use xcm_executor::traits::{InvertLocation, WeightBounds};

	#[pallet::pallet]
	pub struct Pallet<T>(PhantomData<T>);

	/// Stores info about how many DOTS someone has staked and the relation with the ratio
	#[derive(Default, Clone, Encode, Decode, RuntimeDebug)]
	pub struct DerivativeInfo<T: Config> {
		pub index: u16,
		pub account: T::AccountId,
	}

	/// Configuration trait of this pallet. We tightly couple to Parachain Staking in order to
	/// ensure that only staked accounts can create registrations in the first place. This could be
	/// generalized.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;
		/// The balance type.
		type Balance: Parameter
			+ Member
			+ AtLeast32BitUnsigned
			+ Default
			+ Copy
			+ MaybeSerializeDeserialize
			+ Into<u128>;

		/// XCM executor.
		type CallEncoder: EncodeCall<Self>;

		type DerivativeAddressRegistrationOrigin: EnsureOrigin<Self::Origin>;

		/// XCM executor.
		type XcmExecutor: ExecuteXcm<Self::Call>;

		/// Convert `T::AccountId` to `MultiLocation`.
		type AccountIdToMultiLocation: Convert<Self::AccountId, MultiLocation>;

		/// Means of measuring the weight consumed by an XCM message locally.
		type Weigher: WeightBounds<Self::Call>;

		/// Means of inverting a location.
		type LocationInverter: InvertLocation;

		/// Self chain location.
		#[pallet::constant]
		type SelfLocation: Get<MultiLocation>;
	}

	#[derive(Debug, PartialEq, Eq)]
	pub enum AvailableCalls {
		AsDerivative(u16, Vec<u8>),
	}

	pub trait EncodeCall<T: Config> {
		/// Encode call from the relay.
		fn encode_call(call: AvailableCalls) -> Vec<u8>;
	}

	#[pallet::storage]
	#[pallet::getter(fn claimed_indices)]
	pub type ClaimedIndices<T: Config> = StorageMap<_, Blake2_128Concat, u16, T::AccountId>;

	#[pallet::storage]
	#[pallet::getter(fn account_to_index)]
	pub type AccountToIndex<T: Config> = StorageMap<_, Blake2_128Concat, T::AccountId, u16>;

	/// An error that can occur while executing the mapping pallet's logic.
	#[pallet::error]
	pub enum Error<T> {
		IndexAlreadyClaimed,
		UnclaimedIndex,
		NotOwner,
		UnweighableMessage,
		CannotReanchor,
		AssetHasNoReserve,
		InvalidDest,
		NotCrossChainTransfer,
		NotAllowed,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		Transacted(T::AccountId, MultiLocation, Vec<u8>),
		RegisterdDerivative(T::AccountId, u16),
		TransactFailed(XcmError),
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(0)]
		pub fn register(origin: OriginFor<T>, who: T::AccountId, index: u16) -> DispatchResult {
			T::DerivativeAddressRegistrationOrigin::ensure_origin(origin)?;

			ensure!(
				ClaimedIndices::<T>::get(&index).is_none(),
				Error::<T>::IndexAlreadyClaimed
			);

			ClaimedIndices::<T>::insert(&index, who.clone());
			AccountToIndex::<T>::insert(&who, index);

			// Deposit event
			Self::deposit_event(Event::<T>::RegisterdDerivative(who, index));

			Ok(())
		}

		#[pallet::weight(0)]
		pub fn transact_through_derivative(
			origin: OriginFor<T>,
			dest: MultiLocation,
			index: u16,
			fee: MultiAsset,
			dest_weight: Weight,
			inner_call: Vec<u8>,
		) -> DispatchResult {
			let who = ensure_signed(origin.clone())?;

			// The index exists
			let account = ClaimedIndices::<T>::get(index).ok_or(Error::<T>::UnclaimedIndex)?;
			// The derivative index is owned by the origin
			ensure!(account == who, Error::<T>::NotOwner);

			let dest = Self::transfer_kind(&fee, &dest)?;

			// Encode call bytes
			let call_bytes: Vec<u8> =
				T::CallEncoder::encode_call(AvailableCalls::AsDerivative(index, inner_call));

			let origin_as_mult = T::AccountIdToMultiLocation::convert(who.clone());

			let mut xcm: Xcm<T::Call> = Self::transact_fee_in_dest_chain_asset(
				dest.clone(),
				fee,
				dest_weight,
				OriginKind::SovereignAccount,
				call_bytes.clone(),
			)?;

			let weight =
				T::Weigher::weight(&mut xcm).map_err(|()| Error::<T>::UnweighableMessage)?;
			let outcome =
				T::XcmExecutor::execute_xcm_in_credit(origin_as_mult, xcm, weight, weight);

			let maybe_xcm_err: Option<XcmError> = match outcome {
				Outcome::Complete(_w) => Option::None,
				Outcome::Incomplete(_w, err) => Some(err),
				Outcome::Error(err) => Some(err),
			};
			if let Some(xcm_err) = maybe_xcm_err {
				Self::deposit_event(Event::<T>::TransactFailed(xcm_err));
			} else {
				// Deposit event
				Self::deposit_event(Event::<T>::Transacted(who.clone(), dest, call_bytes));
			}

			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		fn transact_fee_in_dest_chain_asset(
			dest: MultiLocation,
			asset: MultiAsset,
			dest_weight: Weight,
			origin_kind: OriginKind,
			call: Vec<u8>,
		) -> Result<Xcm<T::Call>, DispatchError> {
			let transact_instructions: Vec<Xcm<()>> = vec![Transact {
				origin_type: origin_kind,
				require_weight_at_most: dest_weight,
				call: call.into(),
			}];
			let effects: Vec<Order<()>> = vec![Self::buy_execution(
				asset.clone(),
				&dest,
				dest_weight,
				transact_instructions,
			)?];

			Ok(Xcm::WithdrawAsset {
				assets: vec![asset].into(),
				effects: vec![Order::InitiateReserveWithdraw {
					assets: All.into(),
					reserve: dest,
					effects: effects,
				}],
			})
		}

		fn buy_execution(
			asset: MultiAsset,
			at: &MultiLocation,
			weight: u64,
			instructions: Vec<Xcm<()>>,
		) -> Result<Order<()>, DispatchError> {
			let inv_at = T::LocationInverter::invert_location(at);
			let fees = asset
				.reanchored(&inv_at)
				.map_err(|_| Error::<T>::CannotReanchor)?;
			Ok(BuyExecution {
				fees,
				weight: 0,
				debt: weight,
				halt_on_error: false,
				instructions: instructions,
			})
		}

		/// Ensure has the `dest` has chain part and recipient part.
		fn ensure_valid_dest(dest: &MultiLocation) -> Result<MultiLocation, DispatchError> {
			if let (Some(dest), None) = (dest.chain_part(), dest.non_chain_part()) {
				Ok(dest)
			} else {
				Err(Error::<T>::InvalidDest.into())
			}
		}

		/// Get the transfer kind.
		///
		/// Returns `Err` if `asset` and `dest` combination doesn't make sense,
		/// else returns a tuple of:
		/// - `transfer_kind`.
		/// - asset's `reserve` parachain or relay chain location,
		/// - `dest` parachain or relay chain location.
		/// - `recipient` location.
		fn transfer_kind(
			asset: &MultiAsset,
			dest: &MultiLocation,
		) -> Result<MultiLocation, DispatchError> {
			let dest = Self::ensure_valid_dest(dest)?;

			let self_location = T::SelfLocation::get();
			ensure!(dest != self_location, Error::<T>::NotCrossChainTransfer);

			let reserve = asset.reserve().ok_or(Error::<T>::AssetHasNoReserve)?;
			ensure!(reserve == dest, Error::<T>::NotAllowed);

			Ok(dest)
		}
	}
}
