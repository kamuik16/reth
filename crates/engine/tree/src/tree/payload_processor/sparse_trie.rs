//! Sparse Trie task related functionality.

use crate::tree::payload_processor::{
    executor::WorkloadExecutor,
    multiproof::{MultiProofTaskMetrics, SparseTrieUpdate},
};
use alloy_primitives::B256;
use rayon::iter::{ParallelBridge, ParallelIterator};
use reth_trie::{updates::TrieUpdates, Nibbles};
use reth_trie_parallel::root::ParallelStateRootError;
use reth_trie_sparse::{
    blinded::{BlindedProvider, BlindedProviderFactory},
    errors::{SparseStateTrieResult, SparseTrieErrorKind},
    SparseStateTrie, SparseTrieState,
};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use tracing::{debug, trace, trace_span};

/// The level below which the sparse trie hashes are calculated in
/// [`update_sparse_trie`].
const SPARSE_TRIE_INCREMENTAL_LEVEL: usize = 2;

/// A task responsible for populating the sparse trie.
pub(super) struct SparseTrieTask<BPF>
where
    BPF: BlindedProviderFactory + Send + Sync,
    BPF::AccountNodeProvider: BlindedProvider + Send + Sync,
    BPF::StorageNodeProvider: BlindedProvider + Send + Sync,
{
    /// Executor used to spawn subtasks.
    #[expect(unused)] // TODO use this for spawning trie tasks
    pub(super) executor: WorkloadExecutor,
    /// Receives updates from the state root task.
    pub(super) updates: mpsc::Receiver<SparseTrieUpdate>,
    /// Sparse Trie initialized with the blinded provider factory.
    ///
    /// It's kept as a field on the struct to prevent blocking on de-allocation in [`Self::run`].
    pub(super) trie: SparseStateTrie,
    pub(super) metrics: MultiProofTaskMetrics,
    /// Blinded node provider factory.
    blinded_provider_factory: BPF,
}

impl<BPF> SparseTrieTask<BPF>
where
    BPF: BlindedProviderFactory + Send + Sync + Clone,
    BPF::AccountNodeProvider: BlindedProvider + Send + Sync,
    BPF::StorageNodeProvider: BlindedProvider + Send + Sync,
{
    /// Creates a new sparse trie task.
    pub(super) fn new(
        executor: WorkloadExecutor,
        updates: mpsc::Receiver<SparseTrieUpdate>,
        blinded_provider_factory: BPF,
        metrics: MultiProofTaskMetrics,
    ) -> Self {
        Self {
            executor,
            updates,
            metrics,
            trie: SparseStateTrie::new().with_updates(true),
            blinded_provider_factory,
        }
    }

    /// Creates a new sparse trie, populating the accounts trie with the given cleared
    /// `SparseTrieState` if it exists.
    pub(super) fn new_with_stored_trie(
        executor: WorkloadExecutor,
        updates: mpsc::Receiver<SparseTrieUpdate>,
        blinded_provider_factory: BPF,
        trie_metrics: MultiProofTaskMetrics,
        sparse_trie_state: Option<SparseTrieState>,
    ) -> Self {
        if let Some(sparse_trie_state) = sparse_trie_state {
            Self::with_accounts_trie(
                executor,
                updates,
                blinded_provider_factory,
                trie_metrics,
                sparse_trie_state,
            )
        } else {
            Self::new(executor, updates, blinded_provider_factory, trie_metrics)
        }
    }

    /// Creates a new sparse trie task, using the given cleared `SparseTrieState` for the accounts
    /// trie.
    pub(super) fn with_accounts_trie(
        executor: WorkloadExecutor,
        updates: mpsc::Receiver<SparseTrieUpdate>,
        blinded_provider_factory: BPF,
        metrics: MultiProofTaskMetrics,
        sparse_trie_state: SparseTrieState,
    ) -> Self {
        let mut trie = SparseStateTrie::new().with_updates(true);
        trie.populate_from(sparse_trie_state);

        Self { executor, updates, metrics, trie, blinded_provider_factory }
    }

    /// Runs the sparse trie task to completion.
    ///
    /// This waits for new incoming [`SparseTrieUpdate`].
    ///
    /// This concludes once the last trie update has been received.
    ///
    /// NOTE: This function does not take `self` by value to prevent blocking on [`SparseStateTrie`]
    /// drop.
    pub(super) fn run(&mut self) -> Result<StateRootComputeOutcome, ParallelStateRootError> {
        let now = Instant::now();

        let mut num_iterations = 0;

        while let Ok(mut update) = self.updates.recv() {
            num_iterations += 1;
            let mut num_updates = 1;
            while let Ok(next) = self.updates.try_recv() {
                update.extend(next);
                num_updates += 1;
            }

            debug!(
                target: "engine::root",
                num_updates,
                account_proofs = update.multiproof.account_subtree.len(),
                storage_proofs = update.multiproof.storages.len(),
                "Updating sparse trie"
            );

            let elapsed =
                update_sparse_trie(&mut self.trie, update, &self.blinded_provider_factory)
                    .map_err(|e| {
                        ParallelStateRootError::Other(format!(
                            "could not calculate state root: {e:?}"
                        ))
                    })?;
            self.metrics.sparse_trie_update_duration_histogram.record(elapsed);
            trace!(target: "engine::root", ?elapsed, num_iterations, "Root calculation completed");
        }

        debug!(target: "engine::root", num_iterations, "All proofs processed, ending calculation");

        let start = Instant::now();
        let (state_root, trie_updates) =
            self.trie.root_with_updates(&self.blinded_provider_factory).map_err(|e| {
                ParallelStateRootError::Other(format!("could not calculate state root: {e:?}"))
            })?;

        self.metrics.sparse_trie_final_update_duration_histogram.record(start.elapsed());
        self.metrics.sparse_trie_total_duration_histogram.record(now.elapsed());

        // take the account trie
        let trie = self.trie.take_cleared_account_trie_state();

        Ok(StateRootComputeOutcome { state_root, trie_updates, trie })
    }
}

/// Outcome of the state root computation, including the state root itself with
/// the trie updates.
#[derive(Debug)]
pub struct StateRootComputeOutcome {
    /// The state root.
    pub state_root: B256,
    /// The trie updates.
    pub trie_updates: TrieUpdates,
    /// The account state trie.
    pub trie: SparseTrieState,
}

/// Updates the sparse trie with the given proofs and state, and returns the elapsed time.
pub(crate) fn update_sparse_trie<BPF>(
    trie: &mut SparseStateTrie,
    SparseTrieUpdate { mut state, multiproof }: SparseTrieUpdate,
    blinded_provider_factory: &BPF,
) -> SparseStateTrieResult<Duration>
where
    BPF: BlindedProviderFactory + Send + Sync,
    BPF::AccountNodeProvider: BlindedProvider + Send + Sync,
    BPF::StorageNodeProvider: BlindedProvider + Send + Sync,
{
    trace!(target: "engine::root::sparse", "Updating sparse trie");
    let started_at = Instant::now();

    // Reveal new accounts and storage slots.
    trie.reveal_decoded_multiproof(multiproof)?;
    let reveal_multiproof_elapsed = started_at.elapsed();
    trace!(
        target: "engine::root::sparse",
        ?reveal_multiproof_elapsed,
        "Done revealing multiproof"
    );

    // Update storage slots with new values and calculate storage roots.
    let (tx, rx) = mpsc::channel();
    state
        .storages
        .into_iter()
        .map(|(address, storage)| (address, storage, trie.take_storage_trie(&address)))
        .par_bridge()
        .map(|(address, storage, storage_trie)| {
            let span = trace_span!(target: "engine::root::sparse", "Storage trie", ?address);
            let _enter = span.enter();
            trace!(target: "engine::root::sparse", "Updating storage");
            let storage_provider = blinded_provider_factory.storage_node_provider(address);
            let mut storage_trie = storage_trie.ok_or(SparseTrieErrorKind::Blind)?;

            if storage.wiped {
                trace!(target: "engine::root::sparse", "Wiping storage");
                storage_trie.wipe()?;
            }
            for (slot, value) in storage.storage {
                let slot_nibbles = Nibbles::unpack(slot);
                if value.is_zero() {
                    trace!(target: "engine::root::sparse", ?slot, "Removing storage slot");
                    storage_trie.remove_leaf(&slot_nibbles, &storage_provider)?;
                } else {
                    trace!(target: "engine::root::sparse", ?slot, "Updating storage slot");
                    storage_trie.update_leaf(
                        slot_nibbles,
                        alloy_rlp::encode_fixed_size(&value).to_vec(),
                        &storage_provider,
                    )?;
                }
            }

            storage_trie.root();

            SparseStateTrieResult::Ok((address, storage_trie))
        })
        .for_each_init(|| tx.clone(), |tx, result| tx.send(result).unwrap());
    drop(tx);

    // Update account storage roots
    for result in rx {
        let (address, storage_trie) = result?;
        trie.insert_storage_trie(address, storage_trie);

        if let Some(account) = state.accounts.remove(&address) {
            // If the account itself has an update, remove it from the state update and update in
            // one go instead of doing it down below.
            trace!(target: "engine::root::sparse", ?address, "Updating account and its storage root");
            trie.update_account(address, account.unwrap_or_default(), blinded_provider_factory)?;
        } else if trie.is_account_revealed(address) {
            // Otherwise, if the account is revealed, only update its storage root.
            trace!(target: "engine::root::sparse", ?address, "Updating account storage root");
            trie.update_account_storage_root(address, blinded_provider_factory)?;
        }
    }

    // Update accounts
    for (address, account) in state.accounts {
        trace!(target: "engine::root::sparse", ?address, "Updating account");
        trie.update_account(address, account.unwrap_or_default(), blinded_provider_factory)?;
    }

    let elapsed_before = started_at.elapsed();
    trace!(
        target: "engine::root::sparse",
        level=SPARSE_TRIE_INCREMENTAL_LEVEL,
        "Calculating intermediate nodes below trie level"
    );
    trie.calculate_below_level(SPARSE_TRIE_INCREMENTAL_LEVEL);

    let elapsed = started_at.elapsed();
    let below_level_elapsed = elapsed - elapsed_before;
    trace!(
        target: "engine::root::sparse",
        level=SPARSE_TRIE_INCREMENTAL_LEVEL,
        ?below_level_elapsed,
        "Intermediate nodes calculated"
    );

    Ok(elapsed)
}
