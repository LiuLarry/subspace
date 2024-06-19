//! Schema for channel update storage.
#![allow(dead_code)]

use parity_scale_codec::{Decode, Encode};
use sc_client_api::backend::AuxStore;
use sp_blockchain::{Error as ClientError, Result as ClientResult};
use sp_messenger::messages::{ChainId, ChannelId, ChannelState, Nonce};
use subspace_runtime_primitives::BlockNumber;

const CHANNEL_DETAIL: &[u8] = b"channel_detail";

fn channel_detail_key(
    src_chain_id: ChainId,
    dst_chain_id: ChainId,
    channel_id: ChannelId,
) -> Vec<u8> {
    (CHANNEL_DETAIL, src_chain_id, dst_chain_id, channel_id).encode()
}

fn load_decode<Backend: AuxStore, T: Decode>(
    backend: &Backend,
    key: &[u8],
) -> ClientResult<Option<T>> {
    match backend.get_aux(key)? {
        None => Ok(None),
        Some(t) => T::decode(&mut &t[..])
            .map_err(|e| {
                ClientError::Backend(format!("Relayer DB is corrupted. Decode error: {e}"))
            })
            .map(Some),
    }
}

/// Channel detail between src and dst chain.
#[derive(Debug, Encode, Decode, Clone)]
pub struct ChannelDetail {
    // Block number of chain at which the channel state is verified.
    pub block_number: BlockNumber,
    /// Channel identifier.
    pub channel_id: ChannelId,
    /// State of the channel.
    pub state: ChannelState,
    /// Next inbox nonce.
    pub next_inbox_nonce: Nonce,
    /// Next outbox nonce.
    pub next_outbox_nonce: Nonce,
    /// Latest outbox message nonce for which response was received from dst_chain.
    pub latest_response_received_message_nonce: Option<Nonce>,
}

/// Load the channel detail between src and dst chains.
/// Channel detail is the state of channel on src_chain.
pub fn get_channel_details<Backend>(
    backend: &Backend,
    src_chain_id: ChainId,
    dst_chain_id: ChainId,
    channel_id: ChannelId,
) -> ClientResult<Option<ChannelDetail>>
where
    Backend: AuxStore,
{
    load_decode(
        backend,
        channel_detail_key(src_chain_id, dst_chain_id, channel_id).as_slice(),
    )
}

pub(crate) fn set_channel_detail<Backend>(
    backend: &Backend,
    src_chain_id: ChainId,
    dst_chain_id: ChainId,
    channel_detail: ChannelDetail,
) -> ClientResult<()>
where
    Backend: AuxStore,
{
    backend.insert_aux(
        &[(
            channel_detail_key(src_chain_id, dst_chain_id, channel_detail.channel_id).as_slice(),
            channel_detail.encode().as_slice(),
        )],
        vec![],
    )
}
