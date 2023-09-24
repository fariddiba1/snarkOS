// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::helpers::{check_timestamp_for_liveness, fmt_id};
use snarkos_node_narwhal_ledger_service::LedgerService;
use snarkvm::{
    ledger::{
        block::Block,
        narwhal::{BatchCertificate, BatchHeader, Transmission, TransmissionID},
    },
    prelude::{bail, ensure, Address, Field, Network, Result},
};

use indexmap::{map::Entry, IndexMap, IndexSet};
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
};

/// The storage for the memory pool.
///
/// The storage is used to store the following:
/// - `current_height` tracker.
/// - `current_round` tracker.
/// - `round` to `(certificate ID, batch ID, author)` entries.
/// - `certificate ID` to `certificate` entries.
/// - `batch ID` to `round` entries.
/// - `transmission ID` to `(transmission, certificate IDs)` entries.
///
/// The chain of events is as follows:
/// 1. A `transmission` is received.
/// 2. After a `batch` is ready to be stored:
///   - The `certificate` is inserted, triggering updates to the
///     `rounds`, `certificates`, `batch_ids`, and `transmissions` maps.
///   - The missing `transmissions` from storage are inserted into the `transmissions` map.
///   - The certificate ID is inserted into the `transmissions` map.
/// 3. After a `round` reaches quorum threshold:
///  - The next round is inserted into the `current_round`.
#[derive(Clone, Debug)]
pub struct Storage<N: Network> {
    /// The ledger service.
    ledger: Arc<dyn LedgerService<N>>,
    /* Once per block */
    /// The current height.
    current_height: Arc<AtomicU32>,
    /* Once per round */
    /// The current round.
    current_round: Arc<AtomicU64>,
    /// The `round` for which garbage collection has occurred **up to** (inclusive).
    gc_round: Arc<AtomicU64>,
    /// The maximum number of rounds to keep in storage.
    max_gc_rounds: u64,
    /* Once per batch */
    /// The map of `round` to a list of `(certificate ID, batch ID, author)` entries.
    rounds: Arc<RwLock<IndexMap<u64, IndexSet<(Field<N>, Field<N>, Address<N>)>>>>,
    /// The map of `certificate ID` to `certificate`.
    certificates: Arc<RwLock<IndexMap<Field<N>, BatchCertificate<N>>>>,
    /// The map of `batch ID` to `round`.
    batch_ids: Arc<RwLock<IndexMap<Field<N>, u64>>>,
    /// The map of `transmission ID` to `(transmission, certificate IDs)` entries.
    transmissions: Arc<RwLock<IndexMap<TransmissionID<N>, (Transmission<N>, IndexSet<Field<N>>)>>>,
}

impl<N: Network> Storage<N> {
    /// Initializes a new instance of storage.
    pub fn new(ledger: Arc<dyn LedgerService<N>>, max_gc_rounds: u64) -> Self {
        // Retrieve the current committee.
        let committee = ledger.current_committee().expect("Ledger is missing a committee.");
        // Retrieve the current round.
        let current_round = committee.starting_round().max(1);

        // Return the storage.
        let storage = Self {
            ledger,
            current_height: Default::default(),
            current_round: Default::default(),
            gc_round: Default::default(),
            max_gc_rounds,
            rounds: Default::default(),
            certificates: Default::default(),
            batch_ids: Default::default(),
            transmissions: Default::default(),
        };
        // Update the storage to the current round.
        storage.update_current_round(current_round);
        // Return the storage.
        storage
    }
}

impl<N: Network> Storage<N> {
    /// Returns the current height.
    pub fn current_height(&self) -> u32 {
        // Get the current height.
        self.current_height.load(Ordering::SeqCst)
    }
}

impl<N: Network> Storage<N> {
    /// Returns the current round.
    pub fn current_round(&self) -> u64 {
        // Get the current round.
        self.current_round.load(Ordering::SeqCst)
    }

    /// Returns the `round` that garbage collection has occurred **up to** (inclusive).
    pub fn gc_round(&self) -> u64 {
        // Get the GC round.
        self.gc_round.load(Ordering::SeqCst)
    }

    /// Returns the maximum number of rounds to keep in storage.
    pub const fn max_gc_rounds(&self) -> u64 {
        self.max_gc_rounds
    }

    /// Increments storage to the next round, updating the current round.
    /// Note: This method is only called once per round, upon certification of the primary's batch.
    pub fn increment_to_next_round(&self) -> Result<()> {
        // Retrieve the next round.
        let next_round = self.current_round() + 1;
        // Ensure there are no certificates for the next round yet.
        ensure!(!self.contains_certificates_for_round(next_round), "Certificates for the next round cannot exist yet");

        // Retrieve the current committee.
        let current_committee = self.ledger.current_committee()?;
        // Ensure the next round is at or after the current committee's starting round.
        ensure!(next_round >= current_committee.starting_round(), "Next round is behind the current committee");

        // Update the storage to the next round.
        self.update_current_round(next_round);

        // Retrieve the current round.
        let current_round = self.current_round();
        // Retrieve the GC round.
        let gc_round = self.gc_round();
        // Ensure the next round matches in storage.
        ensure!(next_round == current_round, "The next round {next_round} does not match in storage ({current_round})");
        // Ensure the next round is greater than or equal to the GC round.
        ensure!(next_round >= gc_round, "The next round {next_round} is behind the GC round {gc_round}");

        // Log the updated round.
        info!("Starting round {next_round}...");
        Ok(())
    }

    /// Updates the storage to the next round.
    fn update_current_round(&self, next_round: u64) {
        // Update the current round.
        self.current_round.store(next_round, Ordering::SeqCst);

        // Fetch the current GC round.
        let current_gc_round = self.gc_round();
        // Compute the next GC round.
        let next_gc_round = next_round.saturating_sub(self.max_gc_rounds);
        // Check if storage needs to be garbage collected.
        if next_gc_round > current_gc_round {
            // Remove the GC round(s) from storage.
            for gc_round in current_gc_round..next_gc_round {
                // Iterate over the certificates for the GC round.
                for certificate in self.get_certificates_for_round(gc_round).iter() {
                    // Remove the certificate from storage.
                    self.remove_certificate(certificate.certificate_id());
                }
            }
            // Update the GC round.
            self.gc_round.store(next_gc_round, Ordering::SeqCst);
        }
    }
}

impl<N: Network> Storage<N> {
    /// Returns `true` if the storage contains the specified `round`.
    pub fn contains_certificates_for_round(&self, round: u64) -> bool {
        // Check if the round exists in storage.
        self.rounds.read().contains_key(&round)
    }

    /// Returns `true` if the storage contains the specified `certificate ID`.
    pub fn contains_certificate(&self, certificate_id: Field<N>) -> bool {
        // Check if the certificate ID exists in storage.
        self.certificates.read().contains_key(&certificate_id)
    }

    /// Returns `true` if the storage contains a certificate from the specified `author` in the given `round`.
    pub fn contains_certificate_in_round_from(&self, round: u64, author: Address<N>) -> bool {
        self.rounds.read().get(&round).map_or(false, |set| set.iter().any(|(_, _, a)| a == &author))
    }

    /// Returns `true` if the storage contains the specified `batch ID`.
    pub fn contains_batch(&self, batch_id: Field<N>) -> bool {
        // Check if the batch ID exists in storage.
        self.batch_ids.read().contains_key(&batch_id)
    }

    /// Returns `true` if the storage contains the specified `transmission ID`.
    pub fn contains_transmission(&self, transmission_id: impl Into<TransmissionID<N>>) -> bool {
        // Check if the transmission ID exists in storage.
        self.transmissions.read().contains_key(&transmission_id.into())
    }

    /// Returns the transmission for the given `transmission ID`.
    /// If the transmission ID does not exist in storage, `None` is returned.
    pub fn get_transmission(&self, transmission_id: impl Into<TransmissionID<N>>) -> Option<Transmission<N>> {
        // Get the transmission.
        self.transmissions.read().get(&transmission_id.into()).map(|(transmission, _)| transmission).cloned()
    }

    /// Returns the round for the given `certificate ID`.
    /// If the certificate ID does not exist in storage, `None` is returned.
    pub fn get_round_for_certificate(&self, certificate_id: Field<N>) -> Option<u64> {
        // Get the round.
        self.certificates.read().get(&certificate_id).map(|certificate| certificate.round())
    }

    /// Returns the round for the given `batch ID`.
    /// If the batch ID does not exist in storage, `None` is returned.
    pub fn get_round_for_batch(&self, batch_id: Field<N>) -> Option<u64> {
        // Get the round.
        self.batch_ids.read().get(&batch_id).copied()
    }

    /// Returns the certificate round for the given `certificate ID`.
    /// If the certificate ID does not exist in storage, `None` is returned.
    pub fn get_certificate_round(&self, certificate_id: Field<N>) -> Option<u64> {
        // Get the batch certificate and return the round.
        self.certificates.read().get(&certificate_id).map(|certificate| certificate.round())
    }

    /// Returns the certificate for the given `certificate ID`.
    /// If the certificate ID does not exist in storage, `None` is returned.
    pub fn get_certificate(&self, certificate_id: Field<N>) -> Option<BatchCertificate<N>> {
        // Get the batch certificate.
        self.certificates.read().get(&certificate_id).cloned()
    }

    /// Returns the certificates for the given `round`.
    /// If the round does not exist in storage, `None` is returned.
    pub fn get_certificates_for_round(&self, round: u64) -> IndexSet<BatchCertificate<N>> {
        // The genesis round does not have batch certificates.
        if round == 0 {
            return Default::default();
        }
        // Retrieve the certificates.
        if let Some(entries) = self.rounds.read().get(&round) {
            let certificates = self.certificates.read();
            entries.iter().flat_map(|(certificate_id, _, _)| certificates.get(certificate_id).cloned()).collect()
        } else {
            Default::default()
        }
    }

    /// Checks the given `batch_header` for validity, returning the missing transmissions from storage.
    ///
    /// This method ensures the following invariants:
    /// - The batch ID does not already exist in storage.
    /// - The author is a member of the committee for the batch round.
    /// - The timestamp is within the allowed time range.
    /// - None of the transmissions are from any past rounds (up to GC).
    /// - All transmissions declared in the batch header are provided or exist in storage (up to GC).
    /// - All previous certificates declared in the certificate exist in storage (up to GC).
    /// - All previous certificates are for the previous round (i.e. round - 1).
    /// - All previous certificates contain a unique author.
    /// - The previous certificates reached the quorum threshold (2f+1).
    pub fn check_batch_header(
        &self,
        batch_header: &BatchHeader<N>,
        mut transmissions: HashMap<TransmissionID<N>, Transmission<N>>,
    ) -> Result<HashMap<TransmissionID<N>, Transmission<N>>> {
        // Retrieve the round.
        let round = batch_header.round();
        // Retrieve the GC round.
        let gc_round = self.gc_round();
        // Construct a GC log message.
        let gc_log = format!("(gc = {gc_round})");

        // Ensure the batch ID does not already exist in storage.
        if self.contains_batch(batch_header.batch_id()) {
            bail!("Batch for round {round} already exists in storage {gc_log}")
        }

        // Retrieve the committee for the batch round.
        let Ok(committee) = self.ledger.get_committee_for_round(round) else {
            bail!("Storage failed to retrieve the committee for round {round} {gc_log}")
        };
        // Ensure the author is in the committee.
        if !committee.is_committee_member(batch_header.author()) {
            bail!("Author {} is not in the committee for round {round} {gc_log}", batch_header.author())
        }

        // Check the timestamp for liveness.
        check_timestamp_for_liveness(batch_header.timestamp())?;

        // Initialize a list for the missing transmissions from storage.
        let mut missing_transmissions = HashMap::new();
        // Ensure the declared transmission IDs are all present in storage or the given transmissions map.
        for transmission_id in batch_header.transmission_ids() {
            // If the transmission ID does not exist, ensure it was provided by the caller.
            if !self.transmissions.read().contains_key(transmission_id) {
                // Retrieve the transmission.
                let Some(transmission) = transmissions.remove(transmission_id) else {
                    bail!("Failed to provide a transmission for round {round} {gc_log}");
                };
                // Append the transmission.
                missing_transmissions.insert(*transmission_id, transmission);
            }
        }

        // Compute the previous round.
        let previous_round = round.saturating_sub(1);
        // Check if the previous round is within range of the GC round.
        if previous_round > gc_round {
            // Retrieve the committee for the previous round.
            let Ok(previous_committee) = self.ledger.get_committee_for_round(previous_round) else {
                bail!("Missing committee for the previous round {previous_round} in storage {gc_log}")
            };
            // Ensure the previous round certificates exists in storage.
            if !self.contains_certificates_for_round(previous_round) {
                bail!("Missing certificates for the previous round {previous_round} in storage {gc_log}")
            }
            // Ensure the number of previous certificate IDs is at or below the number of committee members.
            if batch_header.previous_certificate_ids().len() > previous_committee.num_members() {
                bail!("Too many previous certificates for round {round} {gc_log}")
            }
            // Initialize a set of the previous authors.
            let mut previous_authors = HashSet::with_capacity(batch_header.previous_certificate_ids().len());
            // Ensure storage contains all declared previous certificates (up to GC).
            for previous_certificate_id in batch_header.previous_certificate_ids() {
                // Retrieve the previous certificate.
                let Some(previous_certificate) = self.get_certificate(*previous_certificate_id) else {
                    bail!(
                        "Missing previous certificate '{}' for certificate in round {round} {gc_log}",
                        fmt_id(previous_certificate_id)
                    )
                };
                // Ensure the previous certificate is for the previous round.
                if previous_certificate.round() != previous_round {
                    bail!("Round {round} certificate contains a round {previous_round} certificate {gc_log}")
                }
                // Ensure the previous author is new.
                if previous_authors.contains(&previous_certificate.author()) {
                    bail!("Round {round} certificate contains a duplicate author {gc_log}")
                }
                // Insert the author of the previous certificate.
                previous_authors.insert(previous_certificate.author());
            }
            // Ensure the previous certificates have reached the quorum threshold.
            if !previous_committee.is_quorum_threshold_reached(&previous_authors) {
                bail!("Previous certificates for a batch in round {round} did not reach quorum threshold {gc_log}")
            }
        }
        Ok(missing_transmissions)
    }

    /// Checks the given `certificate` for validity, returning the missing transmissions from storage.
    ///
    /// This method ensures the following invariants:
    /// - The certificate ID does not already exist in storage.
    /// - The batch ID does not already exist in storage.
    /// - The author is a member of the committee for the batch round.
    /// - The author has not already created a certificate for the batch round.
    /// - The timestamp is within the allowed time range.
    /// - None of the transmissions are from any past rounds (up to GC).
    /// - All transmissions declared in the batch header are provided or exist in storage (up to GC).
    /// - All previous certificates declared in the certificate exist in storage (up to GC).
    /// - All previous certificates are for the previous round (i.e. round - 1).
    /// - The previous certificates reached the quorum threshold (2f+1).
    /// - The timestamps from the signers are all within the allowed time range.
    /// - The signers have reached the quorum threshold (2f+1).
    pub fn check_certificate(
        &self,
        certificate: &BatchCertificate<N>,
        transmissions: HashMap<TransmissionID<N>, Transmission<N>>,
    ) -> Result<HashMap<TransmissionID<N>, Transmission<N>>> {
        // Retrieve the round.
        let round = certificate.round();
        // Retrieve the GC round.
        let gc_round = self.gc_round();
        // Construct a GC log message.
        let gc_log = format!("(gc = {gc_round})");

        // Ensure the certificate ID does not already exist in storage.
        if self.contains_certificate(certificate.certificate_id()) {
            bail!("Certificate for round {round} already exists in storage {gc_log}")
        }

        // Ensure the storage does not already contain a certificate for this author in this round.
        if self.contains_certificate_in_round_from(round, certificate.author()) {
            bail!("Certificate with this author for round {round} already exists in storage {gc_log}")
        }

        // Ensure the batch header is well-formed.
        let missing_transmissions = self.check_batch_header(certificate.batch_header(), transmissions)?;

        // Iterate over the timestamps.
        for timestamp in certificate.timestamps() {
            // Check the timestamp for liveness.
            check_timestamp_for_liveness(timestamp)?;
        }

        // Retrieve the committee for the batch round.
        let Ok(committee) = self.ledger.get_committee_for_round(round) else {
            bail!("Storage failed to retrieve the committee for round {round} {gc_log}")
        };

        // Initialize a set of the signers.
        let mut signers = HashSet::with_capacity(certificate.signatures().len() + 1);
        // Append the batch author.
        signers.insert(certificate.author());

        // Iterate over the signatures.
        for signature in certificate.signatures() {
            // Retrieve the signer.
            let signer = signature.to_address();
            // Ensure the signer is in the committee.
            if !committee.is_committee_member(signer) {
                bail!("Signer {signer} is not in the committee for round {round} {gc_log}")
            }
            // Append the signer.
            signers.insert(signer);
        }

        // Ensure the signatures have reached the quorum threshold.
        if !committee.is_quorum_threshold_reached(&signers) {
            bail!("Signatures for a batch in round {round} did not reach quorum threshold {gc_log}")
        }
        Ok(missing_transmissions)
    }

    /// Inserts the given `certificate` into storage.
    ///
    /// This method triggers updates to the `rounds`, `certificates`, `batch_ids`, and `transmissions` maps.
    ///
    /// This method ensures the following invariants:
    /// - The certificate ID does not already exist in storage.
    /// - The batch ID does not already exist in storage.
    /// - All transmissions declared in the certificate are provided or exist in storage (up to GC).
    /// - All previous certificates declared in the certificate exist in storage (up to GC).
    /// - All previous certificates are for the previous round (i.e. round - 1).
    /// - The previous certificates reached the quorum threshold (2f+1).
    pub fn insert_certificate(
        &self,
        certificate: BatchCertificate<N>,
        transmissions: HashMap<TransmissionID<N>, Transmission<N>>,
    ) -> Result<()> {
        // Ensure the certificate round is above the GC round.
        ensure!(certificate.round() > self.gc_round(), "Certificate round is at or below the GC round");
        // Ensure the certificate and its transmissions are valid.
        let missing_transmissions = self.check_certificate(&certificate, transmissions)?;
        // Insert the certificate into storage.
        self.insert_certificate_atomic(certificate, missing_transmissions);
        Ok(())
    }

    /// Inserts the given `certificate` into storage.
    ///
    /// This method assumes **all missing** transmissions are provided in the `missing_transmissions` map.
    ///
    /// This method triggers updates to the `rounds`, `certificates`, `batch_ids`, and `transmissions` maps.
    fn insert_certificate_atomic(
        &self,
        certificate: BatchCertificate<N>,
        mut missing_transmissions: HashMap<TransmissionID<N>, Transmission<N>>,
    ) {
        // Retrieve the round.
        let round = certificate.round();
        // Retrieve the certificate ID.
        let certificate_id = certificate.certificate_id();
        // Retrieve the batch ID.
        let batch_id = certificate.batch_id();
        // Retrieve the author of the batch.
        let author = certificate.author();

        // Insert the round to certificate ID entry.
        self.rounds.write().entry(round).or_default().insert((certificate_id, batch_id, author));
        // Obtain the certificate's transmission ids.
        let transmission_ids = certificate.transmission_ids().clone();
        // Insert the certificate.
        self.certificates.write().insert(certificate_id, certificate);
        // Insert the batch ID.
        self.batch_ids.write().insert(batch_id, round);
        // Acquire the transmissions write lock.
        let mut transmissions = self.transmissions.write();
        // Inserts the following:
        //   - Inserts **only the missing** transmissions from storage.
        //   - Inserts the certificate ID into the corresponding set for **all** transmissions.
        for transmission_id in transmission_ids {
            // Retrieve the transmission entry.
            transmissions.entry(transmission_id)
                // Insert **only the missing** transmissions from storage.
                .or_insert_with( || {
                    // Retrieve the missing transmission.
                    let transmission = missing_transmissions.remove(&transmission_id).expect("Missing transmission not found");
                    // Return the transmission and an empty set of certificate IDs.
                    (transmission, Default::default())
                })
                // Insert the certificate ID into the corresponding set for **all** transmissions.
                .1.insert(certificate_id);
        }
    }

    /// Removes the given `certificate ID` from storage.
    ///
    /// This method triggers updates to the `rounds`, `certificates`, `batch_ids`, and `transmissions` maps.
    ///
    /// If the certificate was successfully removed, `true` is returned.
    /// If the certificate did not exist in storage, `false` is returned.
    fn remove_certificate(&self, certificate_id: Field<N>) -> bool {
        // Retrieve the certificate.
        let Some(certificate) = self.get_certificate(certificate_id) else {
            warn!("Certificate {certificate_id} does not exist in storage");
            return false;
        };
        // Retrieve the round.
        let round = certificate.round();
        // Retrieve the batch ID.
        let batch_id = certificate.batch_id();
        // Compute the author of the batch.
        let author = certificate.author();

        // Insert the round.
        {
            // Acquire the write lock.
            let mut rounds = self.rounds.write();
            // Remove the round to certificate ID entry.
            rounds.entry(round).or_default().remove(&(certificate_id, batch_id, author));
            // If the round is empty, remove it.
            if rounds.get(&round).map_or(false, |entries| entries.is_empty()) {
                rounds.remove(&round);
            }
        }
        // Remove the certificate.
        self.certificates.write().remove(&certificate_id);
        // Remove the batch ID.
        self.batch_ids.write().remove(&batch_id);
        // Acquire the transmissions write lock.
        let mut transmissions = self.transmissions.write();
        // If this is the last certificate ID for the transmission ID, remove the transmission.
        for transmission_id in certificate.transmission_ids() {
            // Remove the certificate ID for the transmission ID, and determine if there are any more certificate IDs.
            match transmissions.entry(*transmission_id) {
                Entry::Occupied(mut occupied_entry) => {
                    let (_, certificate_ids) = occupied_entry.get_mut();
                    // Remove the certificate ID for the transmission ID.
                    certificate_ids.remove(&certificate_id);
                    // If there are no more certificate IDs for the transmission ID, remove the transmission.
                    if certificate_ids.is_empty() {
                        // Remove the entry for the transmission ID.
                        occupied_entry.remove();
                    }
                }
                Entry::Vacant(_) => {}
            }
        }
        // Return successfully.
        true
    }
}

impl<N: Network> Storage<N> {
    /// Syncs the current height with the block.
    pub(super) fn sync_height_with_block(&self, block: &Block<N>) {
        // If the block height is greater than the current height in storage, sync the height.
        if block.height() > self.current_height() {
            // Update the current height in storage.
            self.current_height.store(block.height(), Ordering::SeqCst);
        }
    }

    /// Syncs the current round with the block.
    pub(super) fn sync_round_with_block(&self, next_round: u64) {
        // Retrieve the current round in the block.
        let next_round = next_round.max(1);
        // If the round in the block is greater than the current round in storage, sync the round.
        if next_round > self.current_round() {
            // Update the current round in storage.
            self.update_current_round(next_round);
            // Log the updated round.
            info!("Synced to round {next_round}...");
        }
    }

    /// Syncs the batch certificate with the block.
    pub(super) fn sync_certificate_with_block(&self, block: &Block<N>, certificate: &BatchCertificate<N>) {
        // Skip if the certificate round is below the GC round.
        if certificate.round() <= self.gc_round() {
            return;
        }
        // If the certificate ID already exists in storage, skip it.
        if self.contains_certificate(certificate.certificate_id()) {
            return;
        }
        // Retrieve the transmissions for the certificate.
        let mut missing_transmissions = HashMap::new();
        // Iterate over the transmission IDs.
        for transmission_id in certificate.transmission_ids() {
            // If the transmission ID already exists in the map, skip it.
            if missing_transmissions.contains_key(transmission_id) {
                continue;
            }
            // If the transmission ID exists in storage, skip it.
            if self.contains_transmission(*transmission_id) {
                continue;
            }
            // Retrieve the transmission.
            match transmission_id {
                TransmissionID::Ratification => (),
                TransmissionID::Solution(puzzle_commitment) => {
                    // Retrieve the solution.
                    match block.get_solution(puzzle_commitment) {
                        // Insert the solution.
                        Some(solution) => missing_transmissions.insert(*transmission_id, (*solution).into()),
                        // Otherwise, try to load the solution from the ledger.
                        None => match self.ledger.get_solution(puzzle_commitment) {
                            // Insert the solution.
                            Ok(solution) => missing_transmissions.insert(*transmission_id, solution.into()),
                            Err(_) => {
                                error!("Missing solution {puzzle_commitment} in block {}", block.height());
                                continue;
                            }
                        },
                    };
                }
                TransmissionID::Transaction(transaction_id) => {
                    // Retrieve the transaction.
                    match block.get_transaction(transaction_id) {
                        // Insert the transaction.
                        Some(transaction) => missing_transmissions.insert(*transmission_id, transaction.clone().into()),
                        // Otherwise, try to load the transaction from the ledger.
                        None => match self.ledger.get_transaction(*transaction_id) {
                            // Insert the transaction.
                            Ok(transaction) => missing_transmissions.insert(*transmission_id, transaction.into()),
                            Err(_) => {
                                warn!("Missing transaction {transaction_id} in block {}", block.height());
                                continue;
                            }
                        },
                    };
                }
            }
        }
        // Insert the batch certificate into storage.
        let certificate_id = fmt_id(certificate.certificate_id());
        debug!(
            "Syncing certificate '{certificate_id}' in round {} (from {})",
            certificate.round(),
            certificate.author()
        );
        if let Err(error) = self.insert_certificate(certificate.clone(), missing_transmissions) {
            error!("Failed to insert certificate '{certificate_id}' from block {} - {error}", block.height());
        }
    }
}

#[cfg(test)]
impl<N: Network> Storage<N> {
    /// Returns the ledger service.
    pub const fn ledger(&self) -> &Arc<dyn LedgerService<N>> {
        &self.ledger
    }

    /// Returns an iterator over the `(round, (certificate ID, batch ID, author))` entries.
    pub fn rounds_iter(&self) -> impl Iterator<Item = (u64, IndexSet<(Field<N>, Field<N>, Address<N>)>)> {
        self.rounds.read().clone().into_iter()
    }

    /// Returns an iterator over the `(certificate ID, certificate)` entries.
    pub fn certificates_iter(&self) -> impl Iterator<Item = (Field<N>, BatchCertificate<N>)> {
        self.certificates.read().clone().into_iter()
    }

    /// Returns an iterator over the `(batch ID, round)` entries.
    pub fn batch_ids_iter(&self) -> impl Iterator<Item = (Field<N>, u64)> {
        self.batch_ids.read().clone().into_iter()
    }

    /// Returns an iterator over the `(transmission ID, (transmission, certificate IDs))` entries.
    pub fn transmissions_iter(
        &self,
    ) -> impl Iterator<Item = (TransmissionID<N>, (Transmission<N>, IndexSet<Field<N>>))> {
        self.transmissions.read().clone().into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snarkos_node_narwhal_ledger_service::MockLedgerService;
    use snarkvm::{
        ledger::narwhal::Data,
        prelude::{Rng, TestRng},
    };

    use ::bytes::Bytes;
    use indexmap::indexset;

    type CurrentNetwork = snarkvm::prelude::Testnet3;

    /// Asserts that the storage matches the expected layout.
    pub fn assert_storage<N: Network>(
        storage: &Storage<N>,
        rounds: &[(u64, IndexSet<(Field<N>, Field<N>, Address<N>)>)],
        certificates: &[(Field<N>, BatchCertificate<N>)],
        batch_ids: &[(Field<N>, u64)],
        transmissions: &HashMap<TransmissionID<N>, (Transmission<N>, IndexSet<Field<N>>)>,
    ) {
        // Ensure the rounds are well-formed.
        assert_eq!(storage.rounds_iter().collect::<Vec<_>>(), *rounds);
        // Ensure the certificates are well-formed.
        assert_eq!(storage.certificates_iter().collect::<Vec<_>>(), *certificates);
        // Ensure the batch IDs are well-formed.
        assert_eq!(storage.batch_ids_iter().collect::<Vec<_>>(), *batch_ids);
        // Ensure the transmissions are well-formed.
        assert_eq!(storage.transmissions_iter().collect::<HashMap<_, _>>(), *transmissions);
    }

    /// Samples a random transmission.
    fn sample_transmission(rng: &mut TestRng) -> Transmission<CurrentNetwork> {
        // Sample random fake solution bytes.
        let s = |rng: &mut TestRng| Data::Buffer(Bytes::from((0..512).map(|_| rng.gen::<u8>()).collect::<Vec<_>>()));
        // Sample random fake transaction bytes.
        let t = |rng: &mut TestRng| Data::Buffer(Bytes::from((0..2048).map(|_| rng.gen::<u8>()).collect::<Vec<_>>()));
        // Sample a random transmission.
        match rng.gen::<bool>() {
            true => Transmission::Solution(s(rng)),
            false => Transmission::Transaction(t(rng)),
        }
    }

    /// Samples the random transmissions, returning the missing transmissions and the transmissions.
    fn sample_transmissions(
        certificate: &BatchCertificate<CurrentNetwork>,
        rng: &mut TestRng,
    ) -> (
        HashMap<TransmissionID<CurrentNetwork>, Transmission<CurrentNetwork>>,
        HashMap<TransmissionID<CurrentNetwork>, (Transmission<CurrentNetwork>, IndexSet<Field<CurrentNetwork>>)>,
    ) {
        // Retrieve the certificate ID.
        let certificate_id = certificate.certificate_id();

        let mut missing_transmissions = HashMap::new();
        let mut transmissions = HashMap::<_, (_, IndexSet<Field<CurrentNetwork>>)>::new();
        for transmission_id in certificate.transmission_ids() {
            // Initialize the transmission.
            let transmission = sample_transmission(rng);
            // Update the missing transmissions.
            missing_transmissions.insert(*transmission_id, transmission.clone());
            // Update the transmissions map.
            transmissions
                .entry(*transmission_id)
                .or_insert((transmission, Default::default()))
                .1
                .insert(certificate_id);
        }
        (missing_transmissions, transmissions)
    }

    // TODO (howardwu): Testing with 'max_gc_rounds' set to '0' should ensure everything is cleared after insertion.

    #[test]
    fn test_certificate_insert_remove() {
        let rng = &mut TestRng::default();

        // Sample a committee.
        let committee = snarkvm::ledger::committee::test_helpers::sample_committee(rng);
        // Initialize the ledger.
        let ledger = Arc::new(MockLedgerService::new(committee));
        // Initialize the storage.
        let storage = Storage::<CurrentNetwork>::new(ledger, 1);

        // Ensure the storage is empty.
        assert_storage(&storage, &[], &[], &[], &Default::default());

        // Create a new certificate.
        let certificate = snarkvm::ledger::narwhal::batch_certificate::test_helpers::sample_batch_certificate(rng);
        // Retrieve the certificate ID.
        let certificate_id = certificate.certificate_id();
        // Retrieve the round.
        let round = certificate.round();
        // Retrieve the batch ID.
        let batch_id = certificate.batch_id();
        // Retrieve the author of the batch.
        let author = certificate.author();

        // Construct the sample 'transmissions'.
        let (missing_transmissions, transmissions) = sample_transmissions(&certificate, rng);

        // Insert the certificate.
        storage.insert_certificate_atomic(certificate.clone(), missing_transmissions);
        // Ensure the certificate exists in storage.
        assert!(storage.contains_certificate(certificate_id));
        // Ensure the certificate is stored in the correct round.
        assert_eq!(storage.get_certificates_for_round(round), indexset! { certificate.clone() });

        // Check that the underlying storage representation is correct.
        {
            // Construct the expected layout for 'rounds'.
            let rounds = [(round, indexset! { (certificate_id, batch_id, author) })];
            // Construct the expected layout for 'certificates'.
            let certificates = [(certificate_id, certificate.clone())];
            // Construct the expected layout for 'batch_ids'.
            let batch_ids = [(batch_id, round)];
            // Assert the storage is well-formed.
            assert_storage(&storage, &rounds, &certificates, &batch_ids, &transmissions);
        }

        // Retrieve the certificate.
        let candidate_certificate = storage.get_certificate(certificate_id).unwrap();
        // Ensure the retrieved certificate is the same as the inserted certificate.
        assert_eq!(certificate, candidate_certificate);

        // Remove the certificate.
        assert!(storage.remove_certificate(certificate_id));
        // Ensure the certificate does not exist in storage.
        assert!(!storage.contains_certificate(certificate_id));
        // Ensure the certificate is no longer stored in the round.
        assert!(storage.get_certificates_for_round(round).is_empty());
        // Ensure the storage is empty.
        assert_storage(&storage, &[], &[], &[], &Default::default());
    }

    #[test]
    fn test_certificate_duplicate() {
        let rng = &mut TestRng::default();

        // Sample a committee.
        let committee = snarkvm::ledger::committee::test_helpers::sample_committee(rng);
        // Initialize the ledger.
        let ledger = Arc::new(MockLedgerService::new(committee));
        // Initialize the storage.
        let storage = Storage::<CurrentNetwork>::new(ledger, 1);

        // Ensure the storage is empty.
        assert_storage(&storage, &[], &[], &[], &Default::default());

        // Create a new certificate.
        let certificate = snarkvm::ledger::narwhal::batch_certificate::test_helpers::sample_batch_certificate(rng);
        // Retrieve the certificate ID.
        let certificate_id = certificate.certificate_id();
        // Retrieve the round.
        let round = certificate.round();
        // Retrieve the batch ID.
        let batch_id = certificate.batch_id();
        // Retrieve the author of the batch.
        let author = certificate.author();

        // Construct the expected layout for 'rounds'.
        let rounds = [(round, indexset! { (certificate_id, batch_id, author) })];
        // Construct the expected layout for 'certificates'.
        let certificates = [(certificate_id, certificate.clone())];
        // Construct the expected layout for 'batch_ids'.
        let batch_ids = [(batch_id, round)];
        // Construct the sample 'transmissions'.
        let (missing_transmissions, transmissions) = sample_transmissions(&certificate, rng);

        // Insert the certificate.
        storage.insert_certificate_atomic(certificate.clone(), missing_transmissions.clone());
        // Ensure the certificate exists in storage.
        assert!(storage.contains_certificate(certificate_id));
        // Check that the underlying storage representation is correct.
        assert_storage(&storage, &rounds, &certificates, &batch_ids, &transmissions);

        // Insert the certificate again - without any missing transmissions.
        storage.insert_certificate_atomic(certificate.clone(), Default::default());
        // Ensure the certificate exists in storage.
        assert!(storage.contains_certificate(certificate_id));
        // Check that the underlying storage representation remains unchanged.
        assert_storage(&storage, &rounds, &certificates, &batch_ids, &transmissions);

        // Insert the certificate again - with all of the original missing transmissions.
        storage.insert_certificate_atomic(certificate, missing_transmissions);
        // Ensure the certificate exists in storage.
        assert!(storage.contains_certificate(certificate_id));
        // Check that the underlying storage representation remains unchanged.
        assert_storage(&storage, &rounds, &certificates, &batch_ids, &transmissions);
    }
}

#[cfg(test)]
pub mod prop_tests {
    use super::*;
    use crate::{
        helpers::{now, storage::tests::assert_storage},
        MAX_GC_ROUNDS,
    };
    use snarkos_node_narwhal_ledger_service::MockLedgerService;
    use snarkvm::{
        ledger::{
            coinbase::PuzzleCommitment,
            committee::prop_tests::{CommitteeContext, ValidatorSet},
            narwhal::Data,
        },
        prelude::{Signature, Uniform},
    };

    use ::bytes::Bytes;
    use indexmap::indexset;
    use proptest::{
        collection,
        prelude::{any, Arbitrary, BoxedStrategy, Just, Strategy},
        prop_oneof,
        sample::{size_range, Selector},
        test_runner::TestRng,
    };
    use rand::{CryptoRng, Error, Rng, RngCore};
    use std::fmt::Debug;
    use test_strategy::proptest;

    type CurrentNetwork = snarkvm::prelude::Testnet3;

    impl Arbitrary for Storage<CurrentNetwork> {
        type Parameters = CommitteeContext;
        type Strategy = BoxedStrategy<Storage<CurrentNetwork>>;

        fn arbitrary() -> Self::Strategy {
            (any::<CommitteeContext>(), 0..MAX_GC_ROUNDS)
                .prop_map(|(CommitteeContext(committee, _), gc_rounds)| {
                    let ledger = Arc::new(MockLedgerService::new(committee));
                    Storage::<CurrentNetwork>::new(ledger, gc_rounds)
                })
                .boxed()
        }

        fn arbitrary_with(context: Self::Parameters) -> Self::Strategy {
            (Just(context), 0..MAX_GC_ROUNDS)
                .prop_map(|(CommitteeContext(committee, _), gc_rounds)| {
                    let ledger = Arc::new(MockLedgerService::new(committee));
                    Storage::<CurrentNetwork>::new(ledger, gc_rounds)
                })
                .boxed()
        }
    }

    // The `proptest::TestRng` doesn't implement `rand_core::CryptoRng` trait which is required in snarkVM, so we use a wrapper
    #[derive(Debug)]
    pub struct CryptoTestRng(TestRng);

    impl Arbitrary for CryptoTestRng {
        type Parameters = ();
        type Strategy = BoxedStrategy<CryptoTestRng>;

        fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
            Just(0).prop_perturb(|_, rng| CryptoTestRng(rng)).boxed()
        }
    }
    impl RngCore for CryptoTestRng {
        fn next_u32(&mut self) -> u32 {
            self.0.next_u32()
        }

        fn next_u64(&mut self) -> u64 {
            self.0.next_u64()
        }

        fn fill_bytes(&mut self, dest: &mut [u8]) {
            self.0.fill_bytes(dest);
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), Error> {
            self.0.try_fill_bytes(dest)
        }
    }

    impl CryptoRng for CryptoTestRng {}

    #[derive(Debug, Clone)]
    pub struct AnyTransmission(pub Transmission<CurrentNetwork>);

    impl Arbitrary for AnyTransmission {
        type Parameters = ();
        type Strategy = BoxedStrategy<AnyTransmission>;

        fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
            any_transmission().prop_map(AnyTransmission).boxed()
        }
    }

    #[derive(Debug, Clone)]
    pub struct AnyTransmissionID(pub TransmissionID<CurrentNetwork>);

    impl Arbitrary for AnyTransmissionID {
        type Parameters = ();
        type Strategy = BoxedStrategy<AnyTransmissionID>;

        fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
            any_transmission_id().prop_map(AnyTransmissionID).boxed()
        }
    }

    fn any_transmission() -> BoxedStrategy<Transmission<CurrentNetwork>> {
        prop_oneof![
            (collection::vec(any::<u8>(), 512..=512))
                .prop_map(|bytes| Transmission::Solution(Data::Buffer(Bytes::from(bytes)))),
            (collection::vec(any::<u8>(), 2048..=2048))
                .prop_map(|bytes| Transmission::Transaction(Data::Buffer(Bytes::from(bytes)))),
        ]
        .boxed()
    }

    pub fn any_puzzle_commitment() -> BoxedStrategy<PuzzleCommitment<CurrentNetwork>> {
        Just(0).prop_perturb(|_, rng| PuzzleCommitment::from_g1_affine(CryptoTestRng(rng).gen())).boxed()
    }

    pub fn any_transaction_id() -> BoxedStrategy<<CurrentNetwork as Network>::TransactionID> {
        Just(0)
            .prop_perturb(|_, rng| {
                <CurrentNetwork as Network>::TransactionID::from(Field::rand(&mut CryptoTestRng(rng)))
            })
            .boxed()
    }

    pub fn any_transmission_id() -> BoxedStrategy<TransmissionID<CurrentNetwork>> {
        prop_oneof![
            any_transaction_id().prop_map(TransmissionID::Transaction),
            any_puzzle_commitment().prop_map(TransmissionID::Solution),
        ]
        .boxed()
    }

    pub fn sign_batch_header<R: Rng + CryptoRng>(
        validator_set: &ValidatorSet,
        batch_header: &BatchHeader<CurrentNetwork>,
        rng: &mut R,
    ) -> IndexMap<Signature<CurrentNetwork>, i64> {
        let mut signatures = IndexMap::with_capacity(validator_set.0.len());
        for validator in validator_set.0.iter() {
            let private_key = validator.private_key;
            let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
            let timestamp_field = Field::from_u64(timestamp as u64);
            signatures.insert(private_key.sign(&[batch_header.batch_id(), timestamp_field], rng).unwrap(), timestamp);
        }
        signatures
    }

    #[proptest]
    fn test_certificate_duplicate(
        context: CommitteeContext,
        #[any(size_range(1..16).lift())] transmissions: Vec<(AnyTransmissionID, AnyTransmission)>,
        mut rng: CryptoTestRng,
        selector: Selector,
    ) {
        let CommitteeContext(committee, ValidatorSet(validators)) = context;

        // Initialize the storage.
        let ledger = Arc::new(MockLedgerService::new(committee));
        let storage = Storage::<CurrentNetwork>::new(ledger, 1);

        // Ensure the storage is empty.
        assert_storage(&storage, &[], &[], &[], &Default::default());

        // Create a new certificate.
        let signer = selector.select(&validators);

        let mut transmission_map = IndexMap::new();

        for (AnyTransmissionID(id), AnyTransmission(t)) in transmissions.iter() {
            transmission_map.insert(*id, t.clone());
        }

        let batch_header = BatchHeader::new(
            &signer.private_key,
            0,
            now(),
            transmission_map.keys().cloned().collect(),
            Default::default(),
            &mut rng,
        )
        .unwrap();
        let certificate = BatchCertificate::new(
            batch_header.clone(),
            sign_batch_header(&ValidatorSet(validators), &batch_header, &mut rng),
        )
        .unwrap();

        // Retrieve the certificate ID.
        let certificate_id = certificate.certificate_id();
        let mut internal_transmissions = HashMap::<_, (_, IndexSet<Field<CurrentNetwork>>)>::new();
        for (AnyTransmissionID(id), AnyTransmission(t)) in transmissions.iter().cloned() {
            internal_transmissions.entry(id).or_insert((t, Default::default())).1.insert(certificate_id);
        }

        // Retrieve the round.
        let round = certificate.round();
        // Retrieve the batch ID.
        let batch_id = certificate.batch_id();
        // Retrieve the author of the batch.
        let author = certificate.author();

        // Construct the expected layout for 'rounds'.
        let rounds = [(round, indexset! { (certificate_id, batch_id, author) })];
        // Construct the expected layout for 'certificates'.
        let certificates = [(certificate_id, certificate.clone())];
        // Construct the expected layout for 'batch_ids'.
        let batch_ids = [(batch_id, round)];

        // Insert the certificate.
        let missing_transmissions: HashMap<TransmissionID<CurrentNetwork>, Transmission<CurrentNetwork>> =
            transmission_map.into_iter().collect();
        storage.insert_certificate_atomic(certificate.clone(), missing_transmissions.clone());
        // Ensure the certificate exists in storage.
        assert!(storage.contains_certificate(certificate_id));
        // Check that the underlying storage representation is correct.
        assert_storage(&storage, &rounds, &certificates, &batch_ids, &internal_transmissions);

        // Insert the certificate again - without any missing transmissions.
        storage.insert_certificate_atomic(certificate.clone(), Default::default());
        // Ensure the certificate exists in storage.
        assert!(storage.contains_certificate(certificate_id));
        // Check that the underlying storage representation remains unchanged.
        assert_storage(&storage, &rounds, &certificates, &batch_ids, &internal_transmissions);

        // Insert the certificate again - with all of the original missing transmissions.
        storage.insert_certificate_atomic(certificate, missing_transmissions);
        // Ensure the certificate exists in storage.
        assert!(storage.contains_certificate(certificate_id));
        // Check that the underlying storage representation remains unchanged.
        assert_storage(&storage, &rounds, &certificates, &batch_ids, &internal_transmissions);
    }
}
