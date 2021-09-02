// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! A malicious overseer backing a particular candidate with a
//! malicious proof of validity that is received.

#![allow(missing_docs)]

use polkadot_cli::{
	create_default_subsystems,
	service::{
		AuthorityDiscoveryApi, AuxStore, BabeApi, Block, Error, HeaderBackend, Overseer,
		OverseerGen, OverseerGenArgs, OverseerHandle, ParachainHost, ProvideRuntimeApi, SpawnNamed,
	},
};

// Import extra types relevant to the particular
// subsystem.
use polkadot_node_core_candidate_validation::CandidateValidationSubsystem;
use polkadot_node_subsystem::messages::{
	AvailabilityRecoveryMessage, CandidateValidationMessage, ValidationFailed,
};
use polkadot_node_subsystem_util as util;

// Filter wrapping related types.
use crate::{interceptor::*, shared::*};
use polkadot_node_primitives::{PoV, ValidationResult};

use polkadot_primitives::v1::{
	CandidateCommitments, CandidateDescriptor, CandidateReceipt, PersistedValidationData,
	ValidationCode,
};

use futures::channel::oneshot;
use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
struct BribedPassageInner<Spawner> {
	spawner: Spawner,
	cache: HashMap<CandidateDescriptor, CandidateReceipt>,
}

#[derive(Clone, Debug)]
struct BribedPassage<Spawner> {
	inner: Arc<Mutex<BribedPassageInner<Spawner>>>,
}

impl<Spawner> BribedPassage<Spawner>
where
	Spawner: SpawnNamed,
{
	fn let_pass(
		persisted_validation_data: PersistedValidationData,
		validation_code: Option<ValidationCode>,
		_candidate_descriptor: CandidateDescriptor,
		_pov: Arc<PoV>,
		response_sender: oneshot::Sender<Result<ValidationResult, ValidationFailed>>,
	) {
		let candidate_commitmentments = CandidateCommitments {
			head_data: persisted_validation_data.parent_head.clone(),
			new_validation_code: validation_code,
			..Default::default()
		};

		response_sender
			.send(Ok(ValidationResult::Valid(candidate_commitmentments, persisted_validation_data)))
			.unwrap();
	}
}

impl<Sender, Spawner> MessageInterceptor<Sender> for BribedPassage<Spawner>
where
	Sender: overseer::SubsystemSender<CandidateValidationMessage>
		+ overseer::SubsystemSender<AllMessages>
		+ Clone
		+ Send
		+ 'static,
	Spawner: SpawnNamed + Send + Clone + 'static,
{
	type Message = CandidateValidationMessage;

	fn intercept_incoming(
		&self,
		sender: &mut Sender,
		msg: FromOverseer<Self::Message>,
	) -> Option<FromOverseer<Self::Message>> {
		match msg {
			FromOverseer::Communication {
				msg:
					CandidateValidationMessage::ValidateFromExhaustive(
						persisted_validation_data,
						validation_code,
						candidate_descriptor,
						pov,
						response_sender,
					),
			} if pov.block_data.0.as_slice() == MALICIOUS_POV => {
				Self::let_pass(
					persisted_validation_data,
					Some(validation_code),
					candidate_descriptor,
					pov,
					response_sender,
				);
				None
			},
			FromOverseer::Communication {
				msg:
					CandidateValidationMessage::ValidateFromChainState(
						candidate_descriptor,
						pov,
						response_sender,
					),
			} if pov.block_data.0.as_slice() == MALICIOUS_POV => {
				if let Some(candidate_receipt) =
					self.inner.lock().unwrap().cache.get(&candidate_descriptor).cloned()
				{
					let mut subsystem_sender = sender.clone();
					let spawner = self.inner.lock().unwrap().spawner.clone();
					spawner.spawn(
						"malus-back-garbage-adhoc",
						Box::pin(async move {
							let relay_parent = candidate_descriptor.relay_parent;
							let session_index = util::request_session_index_for_child(
								relay_parent,
								&mut subsystem_sender,
							)
							.await;
							let session_index = session_index.await.unwrap().unwrap();

							let (a_tx, a_rx) = oneshot::channel();

							subsystem_sender
								.send_message(AllMessages::from(
									AvailabilityRecoveryMessage::RecoverAvailableData(
										candidate_receipt,
										session_index,
										None,
										a_tx,
									),
								))
								.await;

							if let Ok(Ok(availability_data)) = a_rx.await {
								Self::let_pass(
									availability_data.validation_data,
									None,
									candidate_descriptor,
									pov,
									response_sender,
								);
							} else {
								tracing::info!(
									target = MALUS,
									"Could not get availability data, can't back"
								);
							}
						}),
					);
				} else {
					tracing::info!(target = MALUS, "No CandidateReceipt available to work with");
				}
				None
			},
			msg => Some(msg),
		}
	}

	fn intercept_outgoing(&self, msg: AllMessages) -> Option<AllMessages> {
		Some(msg)
	}
}

/// Generates an overseer that exposes bad behavior.
pub(crate) struct BackGarbageCandidate;

impl OverseerGen for BackGarbageCandidate {
	fn generate<'a, Spawner, RuntimeClient>(
		&self,
		args: OverseerGenArgs<'a, Spawner, RuntimeClient>,
	) -> Result<(Overseer<Spawner, Arc<RuntimeClient>>, OverseerHandle), Error>
	where
		RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
		RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
		Spawner: 'static + SpawnNamed + Clone + Unpin,
	{
		let spawner = args.spawner.clone();
		let leaves = args.leaves.clone();
		let runtime_client = args.runtime_client.clone();
		let registry = args.registry.clone();
		let candidate_validation_config = args.candidate_validation_config.clone();

		// modify the subsystem(s) as needed:
		let all_subsystems = create_default_subsystems(args)?.replace_candidate_validation(
			// create the filtered subsystem
			FilteredSubsystem::new(
				CandidateValidationSubsystem::with_config(
					candidate_validation_config,
					Default::default(), // FIXME: pass the real metrics
					Default::default(),
				),
				BribedPassage::<Spawner> {
					inner: Arc::new(Mutex::new(BribedPassageInner {
						spawner: spawner.clone(),
						cache: Default::default(),
					})),
				},
			),
		);

		let (overseer, handle) =
			Overseer::new(leaves, all_subsystems, registry, runtime_client, spawner)?;

		Ok((overseer, handle))
	}
}