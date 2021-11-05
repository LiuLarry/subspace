use crate::commitments::Commitments;
use crate::identity::Identity;
use crate::plot::Plot;
use crate::rpc::RpcClient;
use anyhow::Result;
use futures::{future, future::Either};
use log::{debug, error, info, trace};
use std::time::Instant;
use subspace_core_primitives::{LocalChallenge, Salt};
use subspace_rpc_primitives::{SlotInfo, Solution, SolutionResponse};
use tokio::task::JoinHandle;

/// Farming Instance to store the necessary information for the farming operations,
/// and also a channel to stop/pause the background farming task
pub struct Farming {
    sender: Option<async_oneshot::Sender<()>>,
    handle: Option<JoinHandle<Result<()>>>,
}

impl Farming {
    /// returns an instance of farming, and also starts a concurrent background farming task
    pub fn start(
        plot: Plot,
        commitments: Commitments,
        client: RpcClient,
        identity: Identity,
    ) -> Self {
        let (sender, receiver) = async_oneshot::oneshot();

        let farming_handle = tokio::spawn(background_farming(
            client,
            plot,
            commitments,
            identity,
            receiver,
        ));

        Farming {
            sender: Some(sender),
            handle: Some(farming_handle),
        }
    }

    pub async fn wait(mut self) -> Result<()> {
        self.handle.take().unwrap().await?
    }
}

impl Drop for Farming {
    fn drop(&mut self) {
        // we don't have to do anything in here
        // this is for clarity and verbosity
        let _ = self.sender.take().unwrap().send(());
    }
}

async fn background_farming(
    client: RpcClient,
    plot: Plot,
    commitments: Commitments,
    identity: Identity,
    receiver: async_oneshot::Receiver<()>,
) -> Result<()> {
    match future::select(
        Box::pin(
            async move { subscribe_to_slot_info(&client, &plot, &commitments, &identity).await },
        ),
        receiver,
    )
    .await
    {
        Either::Left((farming_result, _)) => {
            if let Err(val) = farming_result {
                return Err(anyhow!("Farming process ended with error: {}", val));
            }
        }
        Either::Right((channel_result, _)) => match channel_result {
            Ok(_) => {
                info!("Farming stopped!");
            }
            Err(_) => {
                return Err(anyhow!("Unable to receive messages: Sender was dropped."));
            }
        },
    }
    Ok(())
}

#[derive(Default)]
struct Salts {
    current: Option<Salt>,
    next: Option<Salt>,
}

async fn subscribe_to_slot_info(
    client: &RpcClient,
    plot: &Plot,
    commitments: &Commitments,
    identity: &Identity,
) -> Result<()> {
    info!("Subscribing to slot info");
    let mut new_slots = client.subscribe_slot_info().await?;

    let mut salts = Salts::default();

    while let Some(slot_info) = new_slots.next().await? {
        debug!("New slot: {:?}", slot_info);

        update_commitments(plot, commitments, &mut salts, &slot_info);

        let local_challenge = derive_local_challenge(slot_info.global_challenge, identity);

        let maybe_solution = match commitments
            .find_by_range(
                local_challenge.derive_target(),
                slot_info.solution_range,
                slot_info.salt,
            )
            .await
        {
            Some((tag, piece_index)) => {
                let encoding = plot.read(piece_index).await?;
                let solution = Solution {
                    public_key: identity.public_key().to_bytes().into(),
                    piece_index,
                    encoding,
                    signature: identity.sign(&tag).to_bytes().into(),
                    local_challenge,
                    tag,
                };
                debug!("Solution found");
                trace!("Solution found: {:?}", solution);

                Some(solution)
            }
            None => {
                debug!("Solution not found");
                None
            }
        };

        client
            .submit_solution_response(SolutionResponse {
                slot_number: slot_info.slot_number,
                maybe_solution,
                secret_key: identity.secret_key().to_bytes().into(),
            })
            .await?;
    }

    Ok(())
}

/// Compare salts in `slot_info` to those known from `salts` and start update plot commitments
/// accordingly if necessary (in background)
fn update_commitments(
    plot: &Plot,
    commitments: &Commitments,
    salts: &mut Salts,
    slot_info: &SlotInfo,
) {
    // Check if current salt has changed
    if salts.current != Some(slot_info.salt) {
        salts.current.replace(slot_info.salt);

        if salts.next != Some(slot_info.salt) {
            // If previous `salts.next` is the same as current (expected behavior), need to re-commit

            tokio::spawn({
                let salt = slot_info.salt;
                let plot = plot.clone();
                let commitments = commitments.clone();

                async move {
                    let started = Instant::now();
                    info!(
                        "Salt updated to {}, recommitting in background",
                        hex::encode(salt)
                    );

                    if let Err(error) = commitments.create(salt, plot).await {
                        error!(
                            "Failed to create commitment for {}: {}",
                            hex::encode(salt),
                            error
                        );
                    } else {
                        info!(
                            "Finished recommitment for {} in {} seconds",
                            hex::encode(salt),
                            started.elapsed().as_secs_f32()
                        );
                    }
                }
            });
        }
    }

    if let Some(new_next_salt) = slot_info.next_salt {
        if salts.next != Some(new_next_salt) {
            salts.next.replace(new_next_salt);

            tokio::spawn({
                let plot = plot.clone();
                let commitments = commitments.clone();

                async move {
                    let started = Instant::now();
                    info!(
                        "Salt will update to {} soon, recommitting in background",
                        hex::encode(new_next_salt)
                    );
                    if let Err(error) = commitments.create(new_next_salt, plot).await {
                        error!(
                            "Recommitting salt in background failed for {}: {}",
                            hex::encode(new_next_salt),
                            error
                        );
                        return;
                    }
                    info!(
                        "Finished recommitment in background for {} in {} seconds",
                        hex::encode(new_next_salt),
                        started.elapsed().as_secs_f32()
                    );
                }
            });
        }
    }
}

/// Derive local challenge for farmer's identity from the global challenge.
fn derive_local_challenge<C: AsRef<[u8]>>(
    global_challenge: C,
    identity: &Identity,
) -> LocalChallenge {
    identity.sign(global_challenge.as_ref()).to_bytes().into()
}
