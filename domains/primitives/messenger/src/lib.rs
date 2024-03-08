// Copyright (C) 2021 Subspace Labs, Inc.
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

//! Primitives for Messenger.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod endpoint;
pub mod messages;

#[cfg(not(feature = "std"))]
extern crate alloc;

use crate::messages::MessageKey;
#[cfg(not(feature = "std"))]
use alloc::collections::BTreeSet;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use codec::{Decode, Encode};
use frame_support::__private::sp_inherents;
use frame_support::__private::sp_inherents::Error;
use frame_support::inherent::{InherentData, InherentIdentifier, IsFatalError};
use frame_support::pallet_prelude::TypeInfo;
use messages::{BlockMessagesWithStorageKey, CrossDomainMessage, MessageId};
use sp_domains::{ChainId, DomainId};
use sp_mmr_primitives::{EncodableOpaqueLeaf, Proof};
#[cfg(feature = "std")]
use std::collections::BTreeSet;

/// Messenger inherent identifier.
pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"messengr";

/// Trait to handle XDM rewards.
pub trait OnXDMRewards<Balance> {
    fn on_xdm_rewards(rewards: Balance);
}

impl<Balance> OnXDMRewards<Balance> for () {
    fn on_xdm_rewards(_: Balance) {}
}

/// Trait to verify MMR proofs
pub trait MmrProofVerifier<MmrHash, StateRoot> {
    /// Returns consensus state root if the given MMR proof is valid
    fn verify_proof_and_extract_consensus_state_root(
        leaf: EncodableOpaqueLeaf,
        proof: Proof<MmrHash>,
    ) -> Option<StateRoot>;
}

impl<MmrHash, StateRoot> MmrProofVerifier<MmrHash, StateRoot> for () {
    fn verify_proof_and_extract_consensus_state_root(
        _leaf: EncodableOpaqueLeaf,
        _proof: Proof<MmrHash>,
    ) -> Option<StateRoot> {
        None
    }
}

/// Trait that return various storage keys for storages on Consensus chain and domains
pub trait StorageKeys {
    /// Returns the storage key for confirmed domain block on conensus chain
    fn confirmed_domain_block_storage_key(domain_id: DomainId) -> Option<Vec<u8>>;

    /// Returns the outbox storage key for given chain.
    fn outbox_storage_key(chain_id: ChainId, message_key: MessageKey) -> Option<Vec<u8>>;

    /// Returns the inbox responses storage key for given chain.
    fn inbox_responses_storage_key(chain_id: ChainId, message_key: MessageKey) -> Option<Vec<u8>>;
}

impl StorageKeys for () {
    fn confirmed_domain_block_storage_key(_domain_id: DomainId) -> Option<Vec<u8>> {
        None
    }

    fn outbox_storage_key(_chain_id: ChainId, _message_key: MessageKey) -> Option<Vec<u8>> {
        None
    }

    fn inbox_responses_storage_key(
        _chain_id: ChainId,
        _message_key: MessageKey,
    ) -> Option<Vec<u8>> {
        None
    }
}

/// Domain allowlist updates.
#[derive(Default, Debug, Encode, Decode, PartialEq, Clone, TypeInfo)]
pub struct DomainAllowlistUpdates {
    /// Chains that are allowed to open channel with this chain.
    pub allow_chains: BTreeSet<ChainId>,
    /// Chains that are not allowed to open channel with this chain.
    pub remove_chains: BTreeSet<ChainId>,
}

/// The type of the messenger inherent data.
#[derive(Debug, Encode, Decode)]
pub struct InherentType {
    pub maybe_updates: Option<DomainAllowlistUpdates>,
}

/// Inherent specific errors
#[derive(Debug, Encode)]
#[cfg_attr(feature = "std", derive(Decode))]
pub enum InherentError {
    MissingAllowlistUpdates,
    IncorrectAllowlistUpdates,
}

impl IsFatalError for InherentError {
    fn is_fatal_error(&self) -> bool {
        true
    }
}

/// Provides the set code inherent data.
#[cfg(feature = "std")]
pub struct InherentDataProvider {
    data: InherentType,
}

#[cfg(feature = "std")]
impl InherentDataProvider {
    /// Create new inherent data provider from the given `data`.
    pub fn new(data: InherentType) -> Self {
        Self { data }
    }

    /// Returns the `data` of this inherent data provider.
    pub fn data(&self) -> &InherentType {
        &self.data
    }
}

#[cfg(feature = "std")]
#[async_trait::async_trait]
impl sp_inherents::InherentDataProvider for InherentDataProvider {
    async fn provide_inherent_data(
        &self,
        inherent_data: &mut InherentData,
    ) -> Result<(), sp_inherents::Error> {
        inherent_data.put_data(INHERENT_IDENTIFIER, &self.data)
    }

    async fn try_handle_error(
        &self,
        identifier: &InherentIdentifier,
        error: &[u8],
    ) -> Option<Result<(), sp_inherents::Error>> {
        if *identifier != INHERENT_IDENTIFIER {
            return None;
        }

        let error = InherentError::decode(&mut &*error).ok()?;

        Some(Err(Error::Application(Box::from(format!("{error:?}")))))
    }
}

sp_api::decl_runtime_apis! {
    /// Api useful for relayers to fetch messages and submit transactions.
    pub trait RelayerApi<BlockNumber, CHash>
    where
        BlockNumber: Encode + Decode,
        CHash: Encode + Decode,
    {
        /// Returns all the outbox and inbox responses to deliver.
        /// Storage key is used to generate the storage proof for the message.
        fn block_messages() -> BlockMessagesWithStorageKey;

        /// Constructs an outbox message to the dst_chain as an unsigned extrinsic.
        fn outbox_message_unsigned(
            msg: CrossDomainMessage<CHash, sp_core::H256>,
        ) -> Option<Block::Extrinsic>;

        /// Constructs an inbox response message to the dst_chain as an unsigned extrinsic.
        fn inbox_response_message_unsigned(
            msg: CrossDomainMessage<CHash, sp_core::H256>,
        ) -> Option<Block::Extrinsic>;

        /// Returns true if the outbox message is ready to be relayed to dst_chain.
        fn should_relay_outbox_message(dst_chain_id: ChainId, msg_id: MessageId) -> bool;

        /// Returns true if the inbox message response is ready to be relayed to dst_chain.
        fn should_relay_inbox_message_response(dst_chain_id: ChainId, msg_id: MessageId) -> bool;
    }

    /// Api to provide XDM extraction from Runtime Calls.
    #[api_version(3)]
    pub trait MessengerApi {
        /// Returns `Some(true)` if valid XDM or `Some(false)` if not
        /// Returns None if this is not an XDM
        fn is_xdm_valid(
            extrinsic: Vec<u8>
        ) -> Option<bool>;


        /// Returns the confirmed domain block storage for given domain.
        fn confirmed_domain_block_storage_key(domain_id: DomainId) -> Vec<u8>;

        /// Returns storage key for outbox for a given message_id.
        fn outbox_storage_key(message_key: MessageKey) -> Vec<u8>;

        /// Returns storage key for inbox response for a given message_id.
        fn inbox_response_storage_key(message_key: MessageKey) -> Vec<u8>;

        /// Returns any domain's chains allowlist updates on consensus chain.
        fn domain_chains_allowlist_update(domain_id: DomainId) -> Option<DomainAllowlistUpdates>;
    }
}
