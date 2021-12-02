use crate::{
    optimistic_confirmation_verifier::OptimisticConfirmationVerifier,
    replay_stage::DUPLICATE_THRESHOLD,
    result::{Error, Result},
    sigverify,
    verified_vote_packets::{
        ValidatorGossipVotesIterator, VerifiedVoteMetadata, VerifiedVotePackets,
    },
    vote_stake_tracker::VoteStakeTracker,
};
use crossbeam_channel::{
    unbounded, Receiver as CrossbeamReceiver, RecvTimeoutError, Select, Sender as CrossbeamSender,
};
use itertools::izip;
use log::*;
use solana_gossip::{
    cluster_info::{ClusterInfo, GOSSIP_SLEEP_MILLIS},
    crds::Cursor,
};
use solana_ledger::blockstore::Blockstore;
use solana_measure::measure::Measure;
use solana_metrics::inc_new_counter_debug;
use solana_perf::packet::{self, Packets};
use solana_poh::poh_recorder::PohRecorder;
use solana_rpc::{
    optimistically_confirmed_bank_tracker::{BankNotification, BankNotificationSender},
    rpc_subscriptions::RpcSubscriptions,
};
use solana_runtime::{
    bank::Bank,
    bank_forks::BankForks,
    commitment::VOTE_THRESHOLD_SIZE,
    epoch_stakes::{EpochAuthorizedVoters, EpochStakes},
    vote_sender_types::{ReplayVoteReceiver, ReplayedVote},
};
use solana_sdk::{
    clock::{Epoch, Slot, DEFAULT_MS_PER_SLOT, DEFAULT_TICKS_PER_SLOT},
    epoch_schedule::EpochSchedule,
    hash::Hash,
    pubkey::Pubkey,
    signature::Signature,
    slot_hashes,
    transaction::Transaction,
};
use solana_vote_program::{self, vote_state::Vote, vote_transaction};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        {Arc, Mutex, RwLock},
    },
    thread::{self, sleep, Builder, JoinHandle},
    time::{Duration, Instant},
};

// Map from a vote account to the authorized voter for an epoch
pub type ThresholdConfirmedSlots = Vec<(Slot, Hash)>;
pub type VotedHashUpdates = HashMap<Hash, Vec<Pubkey>>;
pub type VerifiedLabelVotePacketsSender = CrossbeamSender<Vec<VerifiedVoteMetadata>>;
pub type VerifiedLabelVotePacketsReceiver = CrossbeamReceiver<Vec<VerifiedVoteMetadata>>;
pub type VerifiedVoteTransactionsSender = CrossbeamSender<Vec<Transaction>>;
pub type VerifiedVoteTransactionsReceiver = CrossbeamReceiver<Vec<Transaction>>;
pub type VerifiedVoteSender = CrossbeamSender<(Pubkey, Vec<Slot>)>;
pub type VerifiedVoteReceiver = CrossbeamReceiver<(Pubkey, Vec<Slot>)>;
pub type GossipVerifiedVoteHashSender = CrossbeamSender<(Pubkey, Slot, Hash)>;
pub type GossipVerifiedVoteHashReceiver = CrossbeamReceiver<(Pubkey, Slot, Hash)>;
pub type GossipDuplicateConfirmedSlotsSender = CrossbeamSender<ThresholdConfirmedSlots>;
pub type GossipDuplicateConfirmedSlotsReceiver = CrossbeamReceiver<ThresholdConfirmedSlots>;

const THRESHOLDS_TO_CHECK: [f64; 2] = [DUPLICATE_THRESHOLD, VOTE_THRESHOLD_SIZE];
const BANK_SEND_VOTES_LOOP_SLEEP_MS: u128 = 10;

#[derive(Default)]
pub struct SlotVoteTracker {
    // Maps pubkeys that have voted for this slot
    // to whether or not we've seen the vote on gossip.
    // True if seen on gossip, false if only seen in replay.
    voted: HashMap<Pubkey, bool>,
    optimistic_votes_tracker: HashMap<Hash, VoteStakeTracker>,
    voted_slot_updates: Option<Vec<Pubkey>>,
    gossip_only_stake: u64,
}

impl SlotVoteTracker {
    pub fn get_voted_slot_updates(&mut self) -> Option<Vec<Pubkey>> {
        self.voted_slot_updates.take()
    }

    pub fn get_or_insert_optimistic_votes_tracker(&mut self, hash: Hash) -> &mut VoteStakeTracker {
        self.optimistic_votes_tracker.entry(hash).or_default()
    }
    pub fn optimistic_votes_tracker(&self, hash: &Hash) -> Option<&VoteStakeTracker> {
        self.optimistic_votes_tracker.get(hash)
    }
}

#[derive(Default)]
pub struct VoteTracker {
    // Map from a slot to a set of validators who have voted for that slot
    slot_vote_trackers: RwLock<HashMap<Slot, Arc<RwLock<SlotVoteTracker>>>>,
    // Don't track votes from people who are not staked, acts as a spam filter
    epoch_authorized_voters: RwLock<HashMap<Epoch, Arc<EpochAuthorizedVoters>>>,
    leader_schedule_epoch: RwLock<Epoch>,
    current_epoch: RwLock<Epoch>,
    epoch_schedule: EpochSchedule,
}

impl VoteTracker {
    pub fn new(root_bank: &Bank) -> Self {
        let current_epoch = root_bank.epoch();
        let vote_tracker = Self {
            leader_schedule_epoch: RwLock::new(current_epoch),
            current_epoch: RwLock::new(current_epoch),
            epoch_schedule: *root_bank.epoch_schedule(),
            ..VoteTracker::default()
        };
        vote_tracker.progress_with_new_root_bank(root_bank);
        assert_eq!(
            *vote_tracker.leader_schedule_epoch.read().unwrap(),
            root_bank.get_leader_schedule_epoch(root_bank.slot())
        );
        assert_eq!(*vote_tracker.current_epoch.read().unwrap(), current_epoch,);
        vote_tracker
    }

    pub fn get_or_insert_slot_tracker(&self, slot: Slot) -> Arc<RwLock<SlotVoteTracker>> {
        let mut slot_tracker = self.slot_vote_trackers.read().unwrap().get(&slot).cloned();

        if slot_tracker.is_none() {
            let new_slot_tracker = Arc::new(RwLock::new(SlotVoteTracker {
                voted: HashMap::new(),
                optimistic_votes_tracker: HashMap::default(),
                voted_slot_updates: None,
                gossip_only_stake: 0,
            }));
            self.slot_vote_trackers
                .write()
                .unwrap()
                .insert(slot, new_slot_tracker.clone());
            slot_tracker = Some(new_slot_tracker);
        }

        slot_tracker.unwrap()
    }

    pub fn get_slot_vote_tracker(&self, slot: Slot) -> Option<Arc<RwLock<SlotVoteTracker>>> {
        self.slot_vote_trackers.read().unwrap().get(&slot).cloned()
    }

    pub fn get_authorized_voter(&self, pubkey: &Pubkey, slot: Slot) -> Option<Pubkey> {
        let epoch = self.epoch_schedule.get_epoch(slot);
        self.epoch_authorized_voters
            .read()
            .unwrap()
            .get(&epoch)
            .map(|epoch_authorized_voters| epoch_authorized_voters.get(pubkey))
            .unwrap_or(None)
            .cloned()
    }

    pub fn vote_contains_authorized_voter(
        vote_tx: &Transaction,
        authorized_voter: &Pubkey,
    ) -> bool {
        let message = &vote_tx.message;
        for (i, key) in message.account_keys.iter().enumerate() {
            if message.is_signer(i) && key == authorized_voter {
                return true;
            }
        }

        false
    }

    #[cfg(test)]
    pub fn insert_vote(&self, slot: Slot, pubkey: Pubkey) {
        let mut w_slot_vote_trackers = self.slot_vote_trackers.write().unwrap();

        let slot_vote_tracker = w_slot_vote_trackers.entry(slot).or_default();

        let mut w_slot_vote_tracker = slot_vote_tracker.write().unwrap();

        w_slot_vote_tracker.voted.insert(pubkey, true);
        if let Some(ref mut voted_slot_updates) = w_slot_vote_tracker.voted_slot_updates {
            voted_slot_updates.push(pubkey)
        } else {
            w_slot_vote_tracker.voted_slot_updates = Some(vec![pubkey]);
        }
    }

    fn progress_leader_schedule_epoch(&self, root_bank: &Bank) {
        // Update with any newly calculated epoch state about future epochs
        let start_leader_schedule_epoch = *self.leader_schedule_epoch.read().unwrap();
        let mut greatest_leader_schedule_epoch = start_leader_schedule_epoch;
        for leader_schedule_epoch in
            start_leader_schedule_epoch..=root_bank.get_leader_schedule_epoch(root_bank.slot())
        {
            let exists = self
                .epoch_authorized_voters
                .read()
                .unwrap()
                .contains_key(&leader_schedule_epoch);
            if !exists {
                let epoch_authorized_voters = root_bank
                    .epoch_stakes(leader_schedule_epoch)
                    .unwrap()
                    .epoch_authorized_voters()
                    .clone();
                self.epoch_authorized_voters
                    .write()
                    .unwrap()
                    .insert(leader_schedule_epoch, epoch_authorized_voters);
                greatest_leader_schedule_epoch = leader_schedule_epoch;
            }
        }

        if greatest_leader_schedule_epoch != start_leader_schedule_epoch {
            *self.leader_schedule_epoch.write().unwrap() = greatest_leader_schedule_epoch;
        }
    }

    fn purge_stale_state(&self, root_bank: &Bank) {
        // Purge any outdated slot data
        let new_root = root_bank.slot();
        let root_epoch = root_bank.epoch();
        self.slot_vote_trackers
            .write()
            .unwrap()
            .retain(|slot, _| *slot >= new_root);

        let current_epoch = *self.current_epoch.read().unwrap();
        if root_epoch != current_epoch {
            // If root moved to a new epoch, purge outdated state
            self.epoch_authorized_voters
                .write()
                .unwrap()
                .retain(|epoch, _| *epoch >= root_epoch);
            *self.current_epoch.write().unwrap() = root_epoch;
        }
    }

    fn progress_with_new_root_bank(&self, root_bank: &Bank) {
        self.progress_leader_schedule_epoch(root_bank);
        self.purge_stale_state(root_bank);
    }
}

struct BankVoteSenderState {
    bank: Arc<Bank>,
    previously_sent_to_bank_votes: HashSet<Signature>,
    bank_send_votes_stats: BankSendVotesStats,
}

impl BankVoteSenderState {
    fn new(bank: Arc<Bank>) -> Self {
        Self {
            bank,
            previously_sent_to_bank_votes: HashSet::new(),
            bank_send_votes_stats: BankSendVotesStats::default(),
        }
    }

    fn report_metrics(&self) {
        self.bank_send_votes_stats.report_metrics(self.bank.slot());
    }
}

#[derive(Default)]
struct BankSendVotesStats {
    num_votes_sent: usize,
    num_batches_sent: usize,
    total_elapsed: u64,
}

impl BankSendVotesStats {
    fn report_metrics(&self, slot: Slot) {
        datapoint_info!(
            "cluster_info_vote_listener-bank-send-vote-stats",
            ("slot", slot, i64),
            ("num_votes_sent", self.num_votes_sent, i64),
            ("total_elapsed", self.total_elapsed, i64),
            ("num_batches_sent", self.num_batches_sent, i64),
        );
    }
}

pub struct ClusterInfoVoteListener {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl ClusterInfoVoteListener {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        exit: &Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        verified_packets_sender: CrossbeamSender<Vec<Packets>>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        vote_tracker: Arc<VoteTracker>,
        bank_forks: Arc<RwLock<BankForks>>,
        subscriptions: Arc<RpcSubscriptions>,
        verified_vote_sender: VerifiedVoteSender,
        gossip_verified_vote_hash_sender: GossipVerifiedVoteHashSender,
        replay_votes_receiver: ReplayVoteReceiver,
        blockstore: Arc<Blockstore>,
        bank_notification_sender: Option<BankNotificationSender>,
        cluster_confirmed_slot_sender: GossipDuplicateConfirmedSlotsSender,
    ) -> Self {
        let exit_ = exit.clone();

        let (verified_vote_label_packets_sender, verified_vote_label_packets_receiver) =
            unbounded();
        let (verified_vote_transactions_sender, verified_vote_transactions_receiver) = unbounded();
        let listen_thread = Builder::new()
            .name("solana-cluster_info_vote_listener".to_string())
            .spawn(move || {
                let _ = Self::recv_loop(
                    exit_,
                    &cluster_info,
                    verified_vote_label_packets_sender,
                    verified_vote_transactions_sender,
                );
            })
            .unwrap();

        let exit_ = exit.clone();
        let poh_recorder = poh_recorder.clone();
        let bank_send_thread = Builder::new()
            .name("solana-cluster_info_bank_send".to_string())
            .spawn(move || {
                let _ = Self::bank_send_loop(
                    exit_,
                    verified_vote_label_packets_receiver,
                    poh_recorder,
                    &verified_packets_sender,
                );
            })
            .unwrap();

        let exit_ = exit.clone();
        let send_thread = Builder::new()
            .name("solana-cluster_info_process_votes".to_string())
            .spawn(move || {
                let _ = Self::process_votes_loop(
                    exit_,
                    verified_vote_transactions_receiver,
                    vote_tracker,
                    bank_forks,
                    subscriptions,
                    gossip_verified_vote_hash_sender,
                    verified_vote_sender,
                    replay_votes_receiver,
                    blockstore,
                    bank_notification_sender,
                    cluster_confirmed_slot_sender,
                );
            })
            .unwrap();

        Self {
            thread_hdls: vec![listen_thread, send_thread, bank_send_thread],
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }

    fn recv_loop(
        exit: Arc<AtomicBool>,
        cluster_info: &ClusterInfo,
        verified_vote_label_packets_sender: VerifiedLabelVotePacketsSender,
        verified_vote_transactions_sender: VerifiedVoteTransactionsSender,
    ) -> Result<()> {
        let mut cursor = Cursor::default();
        while !exit.load(Ordering::Relaxed) {
            let votes = cluster_info.get_votes(&mut cursor);
            inc_new_counter_debug!("cluster_info_vote_listener-recv_count", votes.len());
            if !votes.is_empty() {
                let (vote_txs, packets) = Self::verify_votes(votes);
                verified_vote_transactions_sender.send(vote_txs)?;
                verified_vote_label_packets_sender.send(packets)?;
            }
            sleep(Duration::from_millis(GOSSIP_SLEEP_MILLIS));
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn verify_votes(votes: Vec<Transaction>) -> (Vec<Transaction>, Vec<VerifiedVoteMetadata>) {
        let mut msgs = packet::to_packets_chunked(&votes, 1);

        // Votes should already be filtered by this point.
        let reject_non_vote = false;
        sigverify::ed25519_verify_cpu(&mut msgs, reject_non_vote);

        let (vote_txs, vote_metadata) = izip!(votes.into_iter(), msgs,)
            .filter_map(|(vote_tx, packet)| {
                let (vote, vote_account_key) = vote_transaction::parse_vote_transaction(&vote_tx)
                    .and_then(|(vote_account_key, vote, _)| {
                    if vote.slots.is_empty() {
                        None
                    } else {
                        Some((vote, vote_account_key))
                    }
                })?;

                // to_packets_chunked() above split into 1 packet long chunks
                assert_eq!(packet.packets.len(), 1);
                if !packet.packets[0].meta.discard {
                    if let Some(signature) = vote_tx.signatures.first().cloned() {
                        return Some((
                            vote_tx,
                            VerifiedVoteMetadata {
                                vote_account_key,
                                vote,
                                packet,
                                signature,
                            },
                        ));
                    }
                }
                None
            })
            .unzip();
        (vote_txs, vote_metadata)
    }

    fn bank_send_loop(
        exit: Arc<AtomicBool>,
        verified_vote_label_packets_receiver: VerifiedLabelVotePacketsReceiver,
        poh_recorder: Arc<Mutex<PohRecorder>>,
        verified_packets_sender: &CrossbeamSender<Vec<Packets>>,
    ) -> Result<()> {
        let mut verified_vote_packets = VerifiedVotePackets::default();
        let mut time_since_lock = Instant::now();
        let mut bank_vote_sender_state_option: Option<BankVoteSenderState> = None;

        loop {
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            let would_be_leader = poh_recorder
                .lock()
                .unwrap()
                .would_be_leader(3 * slot_hashes::MAX_ENTRIES as u64 * DEFAULT_TICKS_PER_SLOT);

            if let Err(e) = verified_vote_packets.receive_and_process_vote_packets(
                &verified_vote_label_packets_receiver,
                would_be_leader,
            ) {
                match e {
                    Error::CrossbeamRecvTimeout(RecvTimeoutError::Disconnected)
                    | Error::ReadyTimeout => (),
                    _ => {
                        error!("thread {:?} error {:?}", thread::current().name(), e);
                    }
                }
            }

            if time_since_lock.elapsed().as_millis() > BANK_SEND_VOTES_LOOP_SLEEP_MS as u128 {
                // Always set this to avoid taking the poh lock too often
                time_since_lock = Instant::now();
                // We will take this lock at most once every `BANK_SEND_VOTES_LOOP_SLEEP_MS`
                if let Some(current_working_bank) = poh_recorder.lock().unwrap().bank() {
                    Self::check_for_leader_bank_and_send_votes(
                        &mut bank_vote_sender_state_option,
                        current_working_bank,
                        verified_packets_sender,
                        &verified_vote_packets,
                    )?;
                }
            }
        }
    }

    fn check_for_leader_bank_and_send_votes(
        bank_vote_sender_state_option: &mut Option<BankVoteSenderState>,
        current_working_bank: Arc<Bank>,
        verified_packets_sender: &CrossbeamSender<Vec<Packets>>,
        verified_vote_packets: &VerifiedVotePackets,
    ) -> Result<()> {
        // We will take this lock at most once every `BANK_SEND_VOTES_LOOP_SLEEP_MS`
        if let Some(bank_vote_sender_state) = bank_vote_sender_state_option {
            if bank_vote_sender_state.bank.slot() != current_working_bank.slot() {
                bank_vote_sender_state.report_metrics();
                *bank_vote_sender_state_option =
                    Some(BankVoteSenderState::new(current_working_bank));
            }
        } else {
            *bank_vote_sender_state_option = Some(BankVoteSenderState::new(current_working_bank));
        }

        let bank_vote_sender_state = bank_vote_sender_state_option.as_mut().unwrap();
        let BankVoteSenderState {
            ref bank,
            ref mut bank_send_votes_stats,
            ref mut previously_sent_to_bank_votes,
        } = bank_vote_sender_state;

        // This logic may run multiple times for the same leader bank,
        // we just have to ensure that the same votes are not sent
        // to the bank multiple times, which is guaranteed by
        // `previously_sent_to_bank_votes`
        let gossip_votes_iterator = ValidatorGossipVotesIterator::new(
            bank.clone(),
            verified_vote_packets,
            previously_sent_to_bank_votes,
        );

        let mut filter_gossip_votes_timing = Measure::start("filter_gossip_votes");

        // Send entire batch at a time so that there is no partial processing of
        // a single validator's votes by two different banks. This might happen
        // if we sent each vote individually, for instance if we creaed two different
        // leader banks from the same common parent, one leader bank may process
        // only the later votes and ignore the earlier votes.
        for single_validator_votes in gossip_votes_iterator {
            bank_send_votes_stats.num_votes_sent += single_validator_votes.len();
            bank_send_votes_stats.num_batches_sent += 1;
            verified_packets_sender.send(single_validator_votes)?;
        }
        filter_gossip_votes_timing.stop();
        bank_send_votes_stats.total_elapsed += filter_gossip_votes_timing.as_us();

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn process_votes_loop(
        exit: Arc<AtomicBool>,
        gossip_vote_txs_receiver: VerifiedVoteTransactionsReceiver,
        vote_tracker: Arc<VoteTracker>,
        bank_forks: Arc<RwLock<BankForks>>,
        subscriptions: Arc<RpcSubscriptions>,
        gossip_verified_vote_hash_sender: GossipVerifiedVoteHashSender,
        verified_vote_sender: VerifiedVoteSender,
        replay_votes_receiver: ReplayVoteReceiver,
        blockstore: Arc<Blockstore>,
        bank_notification_sender: Option<BankNotificationSender>,
        cluster_confirmed_slot_sender: GossipDuplicateConfirmedSlotsSender,
    ) -> Result<()> {
        let mut confirmation_verifier =
            OptimisticConfirmationVerifier::new(bank_forks.read().unwrap().root());
        let mut last_process_root = Instant::now();
        let cluster_confirmed_slot_sender = Some(cluster_confirmed_slot_sender);
        loop {
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            let root_bank = bank_forks.read().unwrap().root_bank().clone();
            if last_process_root.elapsed().as_millis() > DEFAULT_MS_PER_SLOT as u128 {
                let unrooted_optimistic_slots = confirmation_verifier
                    .verify_for_unrooted_optimistic_slots(&root_bank, &blockstore);
                // SlotVoteTracker's for all `slots` in `unrooted_optimistic_slots`
                // should still be available because we haven't purged in
                // `progress_with_new_root_bank()` yet, which is called below
                OptimisticConfirmationVerifier::log_unrooted_optimistic_slots(
                    &root_bank,
                    &vote_tracker,
                    &unrooted_optimistic_slots,
                );
                vote_tracker.progress_with_new_root_bank(&root_bank);
                last_process_root = Instant::now();
            }
            let confirmed_slots = Self::listen_and_confirm_votes(
                &gossip_vote_txs_receiver,
                &vote_tracker,
                &root_bank,
                &subscriptions,
                &gossip_verified_vote_hash_sender,
                &verified_vote_sender,
                &replay_votes_receiver,
                &bank_notification_sender,
                &cluster_confirmed_slot_sender,
            );
            match confirmed_slots {
                Ok(confirmed_slots) => {
                    confirmation_verifier
                        .add_new_optimistic_confirmed_slots(confirmed_slots.clone());
                }
                Err(e) => match e {
                    Error::CrossbeamRecvTimeout(RecvTimeoutError::Disconnected) => {
                        return Ok(());
                    }
                    Error::ReadyTimeout => (),
                    _ => {
                        error!("thread {:?} error {:?}", thread::current().name(), e);
                    }
                },
            }
        }
    }

    #[cfg(test)]
    pub fn get_and_process_votes_for_tests(
        gossip_vote_txs_receiver: &VerifiedVoteTransactionsReceiver,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        subscriptions: &RpcSubscriptions,
        gossip_verified_vote_hash_sender: &GossipVerifiedVoteHashSender,
        verified_vote_sender: &VerifiedVoteSender,
        replay_votes_receiver: &ReplayVoteReceiver,
    ) -> Result<ThresholdConfirmedSlots> {
        Self::listen_and_confirm_votes(
            gossip_vote_txs_receiver,
            vote_tracker,
            root_bank,
            subscriptions,
            gossip_verified_vote_hash_sender,
            verified_vote_sender,
            replay_votes_receiver,
            &None,
            &None,
        )
    }

    fn listen_and_confirm_votes(
        gossip_vote_txs_receiver: &VerifiedVoteTransactionsReceiver,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        subscriptions: &RpcSubscriptions,
        gossip_verified_vote_hash_sender: &GossipVerifiedVoteHashSender,
        verified_vote_sender: &VerifiedVoteSender,
        replay_votes_receiver: &ReplayVoteReceiver,
        bank_notification_sender: &Option<BankNotificationSender>,
        cluster_confirmed_slot_sender: &Option<GossipDuplicateConfirmedSlotsSender>,
    ) -> Result<ThresholdConfirmedSlots> {
        let mut sel = Select::new();
        sel.recv(gossip_vote_txs_receiver);
        sel.recv(replay_votes_receiver);
        let mut remaining_wait_time = 200;
        loop {
            if remaining_wait_time == 0 {
                break;
            }
            let start = Instant::now();
            // Wait for one of the receivers to be ready. `ready_timeout`
            // will return if channels either have something, or are
            // disconnected. `ready_timeout` can wake up spuriously,
            // hence the loop
            let _ = sel.ready_timeout(Duration::from_millis(remaining_wait_time))?;

            // Should not early return from this point onwards until `process_votes()`
            // returns below to avoid missing any potential `optimistic_confirmed_slots`
            let gossip_vote_txs: Vec<_> = gossip_vote_txs_receiver.try_iter().flatten().collect();
            let replay_votes: Vec<_> = replay_votes_receiver.try_iter().collect();
            if !gossip_vote_txs.is_empty() || !replay_votes.is_empty() {
                return Ok(Self::filter_and_confirm_with_new_votes(
                    vote_tracker,
                    gossip_vote_txs,
                    replay_votes,
                    root_bank,
                    subscriptions,
                    gossip_verified_vote_hash_sender,
                    verified_vote_sender,
                    bank_notification_sender,
                    cluster_confirmed_slot_sender,
                ));
            } else {
                remaining_wait_time = remaining_wait_time
                    .saturating_sub(std::cmp::max(start.elapsed().as_millis() as u64, 1));
            }
        }
        Ok(vec![])
    }

    #[allow(clippy::too_many_arguments)]
    fn track_new_votes_and_notify_confirmations(
        vote: Vote,
        vote_pubkey: &Pubkey,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        subscriptions: &RpcSubscriptions,
        verified_vote_sender: &VerifiedVoteSender,
        gossip_verified_vote_hash_sender: &GossipVerifiedVoteHashSender,
        diff: &mut HashMap<Slot, HashMap<Pubkey, bool>>,
        new_optimistic_confirmed_slots: &mut ThresholdConfirmedSlots,
        is_gossip_vote: bool,
        bank_notification_sender: &Option<BankNotificationSender>,
        cluster_confirmed_slot_sender: &Option<GossipDuplicateConfirmedSlotsSender>,
    ) {
        if vote.slots.is_empty() {
            return;
        }

        let last_vote_slot = *vote.slots.last().unwrap();
        let last_vote_hash = vote.hash;

        let root = root_bank.slot();
        let mut is_new_vote = false;
        // If slot is before the root, ignore it
        for slot in vote.slots.iter().filter(|slot| **slot > root).rev() {
            let slot = *slot;

            // if we don't have stake information, ignore it
            let epoch = root_bank.epoch_schedule().get_epoch(slot);
            let epoch_stakes = root_bank.epoch_stakes(epoch);
            if epoch_stakes.is_none() {
                continue;
            }
            let epoch_stakes = epoch_stakes.unwrap();

            // The last vote slot, which is the greatest slot in the stack
            // of votes in a vote transaction, qualifies for optimistic confirmation.
            if slot == last_vote_slot {
                let vote_accounts = epoch_stakes.stakes().vote_accounts();
                let stake = vote_accounts
                    .get(vote_pubkey)
                    .map(|(stake, _)| *stake)
                    .unwrap_or_default();
                let total_stake = epoch_stakes.total_stake();

                // Fast track processing of the last slot in a vote transactions
                // so that notifications for optimistic confirmation can be sent
                // as soon as possible.
                let (reached_threshold_results, is_new) = Self::track_optimistic_confirmation_vote(
                    vote_tracker,
                    last_vote_slot,
                    last_vote_hash,
                    *vote_pubkey,
                    stake,
                    total_stake,
                );

                if is_gossip_vote && is_new && stake > 0 {
                    let _ = gossip_verified_vote_hash_sender.send((
                        *vote_pubkey,
                        last_vote_slot,
                        last_vote_hash,
                    ));
                }

                if reached_threshold_results[0] {
                    if let Some(sender) = cluster_confirmed_slot_sender {
                        let _ = sender.send(vec![(last_vote_slot, last_vote_hash)]);
                    }
                }
                if reached_threshold_results[1] {
                    new_optimistic_confirmed_slots.push((last_vote_slot, last_vote_hash));
                    // Notify subscribers about new optimistic confirmation
                    if let Some(sender) = bank_notification_sender {
                        sender
                            .send(BankNotification::OptimisticallyConfirmed(last_vote_slot))
                            .unwrap_or_else(|err| {
                                warn!("bank_notification_sender failed: {:?}", err)
                            });
                    }
                }

                if !is_new && !is_gossip_vote {
                    // By now:
                    // 1) The vote must have come from ReplayStage,
                    // 2) We've seen this vote from replay for this hash before
                    // (`track_optimistic_confirmation_vote()` will not set `is_new == true`
                    // for same slot different hash), so short circuit because this vote
                    // has no new information

                    // Note gossip votes will always be processed because those should be unique
                    // and we need to update the gossip-only stake in the `VoteTracker`.
                    return;
                }

                is_new_vote = is_new;
            }

            diff.entry(slot)
                .or_default()
                .entry(*vote_pubkey)
                .and_modify(|seen_in_gossip_previously| {
                    *seen_in_gossip_previously = *seen_in_gossip_previously || is_gossip_vote
                })
                .or_insert(is_gossip_vote);
        }

        if is_new_vote {
            subscriptions.notify_vote(&vote);
            let _ = verified_vote_sender.send((*vote_pubkey, vote.slots));
        }
    }

    fn filter_gossip_votes(
        vote_tracker: &VoteTracker,
        vote_pubkey: &Pubkey,
        vote: &Vote,
        gossip_tx: &Transaction,
    ) -> bool {
        if vote.slots.is_empty() {
            return false;
        }
        let last_vote_slot = vote.slots.last().unwrap();
        // Votes from gossip need to be verified as they have not been
        // verified by the replay pipeline. Determine the authorized voter
        // based on the last vote slot. This will  drop votes from authorized
        // voters trying to make votes for slots earlier than the epoch for
        // which they are authorized
        let actual_authorized_voter =
            vote_tracker.get_authorized_voter(vote_pubkey, *last_vote_slot);

        if actual_authorized_voter.is_none() {
            return false;
        }

        // Voting without the correct authorized pubkey, dump the vote
        if !VoteTracker::vote_contains_authorized_voter(
            gossip_tx,
            &actual_authorized_voter.unwrap(),
        ) {
            return false;
        }

        true
    }

    fn filter_and_confirm_with_new_votes(
        vote_tracker: &VoteTracker,
        gossip_vote_txs: Vec<Transaction>,
        replayed_votes: Vec<ReplayedVote>,
        root_bank: &Bank,
        subscriptions: &RpcSubscriptions,
        gossip_verified_vote_hash_sender: &GossipVerifiedVoteHashSender,
        verified_vote_sender: &VerifiedVoteSender,
        bank_notification_sender: &Option<BankNotificationSender>,
        cluster_confirmed_slot_sender: &Option<GossipDuplicateConfirmedSlotsSender>,
    ) -> ThresholdConfirmedSlots {
        let mut diff: HashMap<Slot, HashMap<Pubkey, bool>> = HashMap::new();
        let mut new_optimistic_confirmed_slots = vec![];

        // Process votes from gossip and ReplayStage
        for (is_gossip, (vote_pubkey, vote, _)) in gossip_vote_txs
            .iter()
            .filter_map(|gossip_tx| {
                vote_transaction::parse_vote_transaction(gossip_tx)
                    .filter(|(vote_pubkey, vote, _)| {
                        Self::filter_gossip_votes(vote_tracker, vote_pubkey, vote, gossip_tx)
                    })
                    .map(|v| (true, v))
            })
            .chain(replayed_votes.into_iter().map(|v| (false, v)))
        {
            Self::track_new_votes_and_notify_confirmations(
                vote,
                &vote_pubkey,
                vote_tracker,
                root_bank,
                subscriptions,
                verified_vote_sender,
                gossip_verified_vote_hash_sender,
                &mut diff,
                &mut new_optimistic_confirmed_slots,
                is_gossip,
                bank_notification_sender,
                cluster_confirmed_slot_sender,
            );
        }

        // Process all the slots accumulated from replay and gossip.
        for (slot, mut slot_diff) in diff {
            let slot_tracker = vote_tracker.get_or_insert_slot_tracker(slot);
            {
                let r_slot_tracker = slot_tracker.read().unwrap();
                // Only keep the pubkeys we haven't seen voting for this slot
                slot_diff.retain(|pubkey, seen_in_gossip_above| {
                    let seen_in_gossip_previously = r_slot_tracker.voted.get(pubkey);
                    let is_new = seen_in_gossip_previously.is_none();
                    // `is_new_from_gossip` means we observed a vote for this slot
                    // for the first time in gossip
                    let is_new_from_gossip = !seen_in_gossip_previously.cloned().unwrap_or(false)
                        && *seen_in_gossip_above;
                    is_new || is_new_from_gossip
                });
            }
            let mut w_slot_tracker = slot_tracker.write().unwrap();
            if w_slot_tracker.voted_slot_updates.is_none() {
                w_slot_tracker.voted_slot_updates = Some(vec![]);
            }
            let mut gossip_only_stake = 0;
            let epoch = root_bank.epoch_schedule().get_epoch(slot);
            let epoch_stakes = root_bank.epoch_stakes(epoch);

            for (pubkey, seen_in_gossip_above) in slot_diff {
                if seen_in_gossip_above {
                    // By this point we know if the vote was seen in gossip above,
                    // it was not seen in gossip at any point in the past (if it was seen
                    // in gossip in the past, `is_new` would be false and it would have
                    // been filtered out above), so it's safe to increment the gossip-only
                    // stake
                    Self::sum_stake(&mut gossip_only_stake, epoch_stakes, &pubkey);
                }

                // From the `slot_diff.retain` earlier, we know because there are
                // no other writers to `slot_vote_tracker` that
                // `is_new || is_new_from_gossip`. In both cases we want to record
                // `is_new_from_gossip` for the `pubkey` entry.
                w_slot_tracker.voted.insert(pubkey, seen_in_gossip_above);
                w_slot_tracker
                    .voted_slot_updates
                    .as_mut()
                    .unwrap()
                    .push(pubkey);
            }

            w_slot_tracker.gossip_only_stake += gossip_only_stake
        }
        new_optimistic_confirmed_slots
    }

    // Returns if the slot was optimistically confirmed, and whether
    // the slot was new
    fn track_optimistic_confirmation_vote(
        vote_tracker: &VoteTracker,
        slot: Slot,
        hash: Hash,
        pubkey: Pubkey,
        stake: u64,
        total_epoch_stake: u64,
    ) -> (Vec<bool>, bool) {
        let slot_tracker = vote_tracker.get_or_insert_slot_tracker(slot);
        // Insert vote and check for optimistic confirmation
        let mut w_slot_tracker = slot_tracker.write().unwrap();

        w_slot_tracker
            .get_or_insert_optimistic_votes_tracker(hash)
            .add_vote_pubkey(pubkey, stake, total_epoch_stake, &THRESHOLDS_TO_CHECK)
    }

    fn sum_stake(sum: &mut u64, epoch_stakes: Option<&EpochStakes>, pubkey: &Pubkey) {
        if let Some(stakes) = epoch_stakes {
            if let Some(vote_account) = stakes.stakes().vote_accounts().get(pubkey) {
                *sum += vote_account.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_perf::packet;
    use solana_rpc::optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank;
    use solana_runtime::{
        bank::Bank,
        commitment::BlockCommitmentCache,
        genesis_utils::{self, create_genesis_config, GenesisConfigInfo, ValidatorVoteKeypairs},
        vote_sender_types::ReplayVoteSender,
    };
    use solana_sdk::{
        hash::Hash,
        pubkey::Pubkey,
        signature::{Keypair, Signature, Signer},
    };
    use solana_vote_program::vote_state::Vote;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    #[test]
    fn test_max_vote_tx_fits() {
        solana_logger::setup();
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let slots: Vec<_> = (0..31).collect();

        let vote_tx = vote_transaction::new_vote_transaction(
            slots,
            Hash::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &vote_keypair,
            Some(Hash::default()),
        );

        use bincode::serialized_size;
        info!("max vote size {}", serialized_size(&vote_tx).unwrap());

        let msgs = packet::to_packets_chunked(&[vote_tx], 1); // panics if won't fit

        assert_eq!(msgs.len(), 1);
    }

    fn run_vote_contains_authorized_voter(hash: Option<Hash>) {
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let authorized_voter = Keypair::new();

        let vote_tx = vote_transaction::new_vote_transaction(
            vec![0],
            Hash::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &authorized_voter,
            hash,
        );

        // Check that the two signing keys pass the check
        assert!(VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &node_keypair.pubkey()
        ));

        assert!(VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &authorized_voter.pubkey()
        ));

        // Non signing key shouldn't pass the check
        assert!(!VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &vote_keypair.pubkey()
        ));

        // Set the authorized voter == vote keypair
        let vote_tx = vote_transaction::new_vote_transaction(
            vec![0],
            Hash::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &vote_keypair,
            hash,
        );

        // Check that the node_keypair and vote keypair pass the authorized voter check
        assert!(VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &node_keypair.pubkey()
        ));

        assert!(VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &vote_keypair.pubkey()
        ));

        // The other keypair should not pass the check
        assert!(!VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &authorized_voter.pubkey()
        ));
    }

    #[test]
    fn test_vote_contains_authorized_voter() {
        run_vote_contains_authorized_voter(None);
        run_vote_contains_authorized_voter(Some(Hash::default()));
    }

    #[test]
    fn test_update_new_root() {
        let (vote_tracker, bank, _, _) = setup();

        // Check outdated slots are purged with new root
        let new_voter = solana_sdk::pubkey::new_rand();
        // Make separate copy so the original doesn't count toward
        // the ref count, which would prevent cleanup
        let new_voter_ = new_voter;
        vote_tracker.insert_vote(bank.slot(), new_voter_);
        assert!(vote_tracker
            .slot_vote_trackers
            .read()
            .unwrap()
            .contains_key(&bank.slot()));
        let bank1 = Bank::new_from_parent(&bank, &Pubkey::default(), bank.slot() + 1);
        vote_tracker.progress_with_new_root_bank(&bank1);
        assert!(!vote_tracker
            .slot_vote_trackers
            .read()
            .unwrap()
            .contains_key(&bank.slot()));

        // Check `keys` and `epoch_authorized_voters` are purged when new
        // root bank moves to the next epoch
        let current_epoch = bank.epoch();
        let new_epoch_bank = Bank::new_from_parent(
            &bank,
            &Pubkey::default(),
            bank.epoch_schedule()
                .get_first_slot_in_epoch(current_epoch + 1),
        );
        vote_tracker.progress_with_new_root_bank(&new_epoch_bank);
        assert_eq!(
            *vote_tracker.current_epoch.read().unwrap(),
            current_epoch + 1
        );
    }

    #[test]
    fn test_update_new_leader_schedule_epoch() {
        let (vote_tracker, bank, _, _) = setup();

        // Check outdated slots are purged with new root
        let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let next_leader_schedule_epoch = leader_schedule_epoch + 1;
        let mut next_leader_schedule_computed = bank.slot();
        loop {
            next_leader_schedule_computed += 1;
            if bank.get_leader_schedule_epoch(next_leader_schedule_computed)
                == next_leader_schedule_epoch
            {
                break;
            }
        }
        assert_eq!(
            bank.get_leader_schedule_epoch(next_leader_schedule_computed),
            next_leader_schedule_epoch
        );
        let next_leader_schedule_bank =
            Bank::new_from_parent(&bank, &Pubkey::default(), next_leader_schedule_computed);
        vote_tracker.progress_leader_schedule_epoch(&next_leader_schedule_bank);
        assert_eq!(
            *vote_tracker.leader_schedule_epoch.read().unwrap(),
            next_leader_schedule_epoch
        );
        assert_eq!(
            vote_tracker
                .epoch_authorized_voters
                .read()
                .unwrap()
                .get(&next_leader_schedule_epoch)
                .unwrap(),
            next_leader_schedule_bank
                .epoch_stakes(next_leader_schedule_epoch)
                .unwrap()
                .epoch_authorized_voters()
        );
    }

    #[test]
    fn test_votes_in_range() {
        // Create some voters at genesis
        let stake_per_validator = 100;
        let (vote_tracker, _, validator_voting_keypairs, subscriptions) = setup();
        let (votes_sender, votes_receiver) = unbounded();
        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver) = unbounded();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );

        let bank0 = Bank::new_for_tests(&genesis_config);
        // Votes for slots less than the provided root bank's slot should not be processed
        let bank3 = Arc::new(Bank::new_from_parent(
            &Arc::new(bank0),
            &Pubkey::default(),
            3,
        ));
        let vote_slots = vec![1, 2];
        send_vote_txs(
            vote_slots,
            vec![],
            &validator_voting_keypairs,
            None,
            &votes_sender,
            &replay_votes_sender,
        );
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_receiver,
            &vote_tracker,
            &bank3,
            &subscriptions,
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
            &None,
        )
        .unwrap();

        // Vote slots for slots greater than root bank's set of currently calculated epochs
        // are ignored
        let max_epoch = bank3.get_leader_schedule_epoch(bank3.slot());
        assert!(bank3.epoch_stakes(max_epoch).is_some());
        let unknown_epoch = max_epoch + 1;
        assert!(bank3.epoch_stakes(unknown_epoch).is_none());
        let first_slot_in_unknown_epoch = bank3
            .epoch_schedule()
            .get_first_slot_in_epoch(unknown_epoch);
        let vote_slots = vec![first_slot_in_unknown_epoch, first_slot_in_unknown_epoch + 1];
        send_vote_txs(
            vote_slots,
            vec![],
            &validator_voting_keypairs,
            None,
            &votes_sender,
            &replay_votes_sender,
        );
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_receiver,
            &vote_tracker,
            &bank3,
            &subscriptions,
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
            &None,
        )
        .unwrap();

        // Should be no updates since everything was ignored
        assert!(vote_tracker.slot_vote_trackers.read().unwrap().is_empty());
    }

    fn send_vote_txs(
        gossip_vote_slots: Vec<Slot>,
        replay_vote_slots: Vec<Slot>,
        validator_voting_keypairs: &[ValidatorVoteKeypairs],
        switch_proof_hash: Option<Hash>,
        votes_sender: &VerifiedVoteTransactionsSender,
        replay_votes_sender: &ReplayVoteSender,
    ) {
        validator_voting_keypairs.iter().for_each(|keypairs| {
            let node_keypair = &keypairs.node_keypair;
            let vote_keypair = &keypairs.vote_keypair;
            let vote_tx = vote_transaction::new_vote_transaction(
                gossip_vote_slots.clone(),
                Hash::default(),
                Hash::default(),
                node_keypair,
                vote_keypair,
                vote_keypair,
                switch_proof_hash,
            );
            votes_sender.send(vec![vote_tx]).unwrap();
            let replay_vote = Vote::new(replay_vote_slots.clone(), Hash::default());
            // Send same vote twice, but should only notify once
            for _ in 0..2 {
                replay_votes_sender
                    .send((
                        vote_keypair.pubkey(),
                        replay_vote.clone(),
                        switch_proof_hash,
                    ))
                    .unwrap();
            }
        });
    }

    fn run_test_process_votes(hash: Option<Hash>) {
        // Create some voters at genesis
        let stake_per_validator = 100;
        let (vote_tracker, _, validator_voting_keypairs, subscriptions) = setup();
        let (votes_txs_sender, votes_txs_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, gossip_verified_vote_hash_receiver) = unbounded();
        let (verified_vote_sender, verified_vote_receiver) = unbounded();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );
        let bank0 = Bank::new_for_tests(&genesis_config);

        let gossip_vote_slots = vec![1, 2];
        let replay_vote_slots = vec![3, 4];
        send_vote_txs(
            gossip_vote_slots.clone(),
            replay_vote_slots.clone(),
            &validator_voting_keypairs,
            hash,
            &votes_txs_sender,
            &replay_votes_sender,
        );

        // Check that all the votes were registered for each validator correctly
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_txs_receiver,
            &vote_tracker,
            &bank0,
            &subscriptions,
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
            &None,
        )
        .unwrap();

        let mut gossip_verified_votes: HashMap<Slot, HashMap<Hash, Vec<Pubkey>>> = HashMap::new();
        for (pubkey, slot, hash) in gossip_verified_vote_hash_receiver.try_iter() {
            // send_vote_txs() will send each vote twice, but we should only get a notification
            // once for each via this channel
            let exists = gossip_verified_votes
                .get(&slot)
                .and_then(|slot_hashes| slot_hashes.get(&hash))
                .map(|slot_hash_voters| slot_hash_voters.contains(&pubkey))
                .unwrap_or(false);
            assert!(!exists);
            gossip_verified_votes
                .entry(slot)
                .or_default()
                .entry(hash)
                .or_default()
                .push(pubkey);
        }

        // Only the last vote in the `gossip_vote` set should count towards
        // the `voted_hash_updates` set. Important to note here that replay votes
        // should not count
        let last_gossip_vote_slot = *gossip_vote_slots.last().unwrap();
        assert_eq!(gossip_verified_votes.len(), 1);
        let slot_hashes = gossip_verified_votes.get(&last_gossip_vote_slot).unwrap();
        assert_eq!(slot_hashes.len(), 1);
        let slot_hash_votes = slot_hashes.get(&Hash::default()).unwrap();
        assert_eq!(slot_hash_votes.len(), validator_voting_keypairs.len());
        for voting_keypairs in &validator_voting_keypairs {
            let pubkey = voting_keypairs.vote_keypair.pubkey();
            assert!(slot_hash_votes.contains(&pubkey));
        }

        // Check that the received votes were pushed to other commponents
        // subscribing via `verified_vote_receiver`
        let all_expected_slots: BTreeSet<_> = gossip_vote_slots
            .clone()
            .into_iter()
            .chain(replay_vote_slots.clone().into_iter())
            .collect();
        let mut pubkey_to_votes: HashMap<Pubkey, BTreeSet<Slot>> = HashMap::new();
        for (received_pubkey, new_votes) in verified_vote_receiver.try_iter() {
            let already_received_votes = pubkey_to_votes.entry(received_pubkey).or_default();
            for new_vote in new_votes {
                // `new_vote` should only be received once
                assert!(already_received_votes.insert(new_vote));
            }
        }
        assert_eq!(pubkey_to_votes.len(), validator_voting_keypairs.len());
        for keypairs in &validator_voting_keypairs {
            assert_eq!(
                *pubkey_to_votes
                    .get(&keypairs.vote_keypair.pubkey())
                    .unwrap(),
                all_expected_slots
            );
        }

        // Check the vote trackers were updated correctly
        for vote_slot in all_expected_slots {
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(vote_slot).unwrap();
            let r_slot_vote_tracker = slot_vote_tracker.read().unwrap();
            for voting_keypairs in &validator_voting_keypairs {
                let pubkey = voting_keypairs.vote_keypair.pubkey();
                assert!(r_slot_vote_tracker.voted.contains_key(&pubkey));
                assert!(r_slot_vote_tracker
                    .voted_slot_updates
                    .as_ref()
                    .unwrap()
                    .contains(&Arc::new(pubkey)));
                // Only the last vote in the stack of `gossip_vote` and `replay_vote_slots`
                // should count towards the `optimistic` vote set,
                let optimistic_votes_tracker =
                    r_slot_vote_tracker.optimistic_votes_tracker(&Hash::default());
                if vote_slot == *gossip_vote_slots.last().unwrap()
                    || vote_slot == *replay_vote_slots.last().unwrap()
                {
                    let optimistic_votes_tracker = optimistic_votes_tracker.unwrap();
                    assert!(optimistic_votes_tracker.voted().contains(&pubkey));
                    assert_eq!(
                        optimistic_votes_tracker.stake(),
                        stake_per_validator * validator_voting_keypairs.len() as u64
                    );
                } else {
                    assert!(optimistic_votes_tracker.is_none())
                }
            }
        }
    }

    #[test]
    fn test_process_votes1() {
        run_test_process_votes(None);
        run_test_process_votes(Some(Hash::default()));
    }

    #[test]
    fn test_process_votes2() {
        // Create some voters at genesis
        let (vote_tracker, _, validator_voting_keypairs, subscriptions) = setup();

        // Create bank with the voters
        let stake_per_validator = 100;
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );
        let bank0 = Bank::new_for_tests(&genesis_config);

        // Send some votes to process
        let (votes_txs_sender, votes_txs_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        let (verified_vote_sender, verified_vote_receiver) = unbounded();
        let (_replay_votes_sender, replay_votes_receiver) = unbounded();

        let mut expected_votes = vec![];
        let num_voters_per_slot = 2;
        let bank_hash = Hash::default();
        for (i, keyset) in validator_voting_keypairs
            .chunks(num_voters_per_slot)
            .enumerate()
        {
            let validator_votes: Vec<_> = keyset
                .iter()
                .map(|keypairs| {
                    let node_keypair = &keypairs.node_keypair;
                    let vote_keypair = &keypairs.vote_keypair;
                    expected_votes.push((vote_keypair.pubkey(), vec![i as Slot + 1]));
                    vote_transaction::new_vote_transaction(
                        vec![i as u64 + 1],
                        bank_hash,
                        Hash::default(),
                        node_keypair,
                        vote_keypair,
                        vote_keypair,
                        None,
                    )
                })
                .collect();
            votes_txs_sender.send(validator_votes).unwrap();
        }

        // Read and process votes from channel `votes_receiver`
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_txs_receiver,
            &vote_tracker,
            &bank0,
            &subscriptions,
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
            &None,
        )
        .unwrap();

        // Check that the received votes were pushed to other commponents
        // subscribing via a channel
        let received_votes: Vec<_> = verified_vote_receiver.try_iter().collect();
        assert_eq!(received_votes.len(), validator_voting_keypairs.len());
        for (expected_pubkey_vote, received_pubkey_vote) in
            expected_votes.iter().zip(received_votes.iter())
        {
            assert_eq!(expected_pubkey_vote, received_pubkey_vote);
        }

        // Check that all the votes were registered for each validator correctly
        for (i, keyset) in validator_voting_keypairs.chunks(2).enumerate() {
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(i as u64 + 1).unwrap();
            let r_slot_vote_tracker = &slot_vote_tracker.read().unwrap();
            for voting_keypairs in keyset {
                let pubkey = voting_keypairs.vote_keypair.pubkey();
                assert!(r_slot_vote_tracker.voted.contains_key(&pubkey));
                assert!(r_slot_vote_tracker
                    .voted_slot_updates
                    .as_ref()
                    .unwrap()
                    .contains(&Arc::new(pubkey)));
                // All the votes were single votes, so they should all count towards
                // the optimistic confirmation vote set
                let optimistic_votes_tracker = r_slot_vote_tracker
                    .optimistic_votes_tracker(&bank_hash)
                    .unwrap();
                assert!(optimistic_votes_tracker.voted().contains(&pubkey));
                assert_eq!(
                    optimistic_votes_tracker.stake(),
                    num_voters_per_slot as u64 * stake_per_validator
                );
            }
        }
    }

    fn run_test_process_votes3(switch_proof_hash: Option<Hash>) {
        let (votes_sender, votes_receiver) = unbounded();
        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver) = unbounded();

        let vote_slot = 1;
        let vote_bank_hash = Hash::default();
        // Events:
        // 0: Send gossip vote
        // 1: Send replay vote
        // 2: Send both
        let ordered_events = vec![
            vec![0],
            vec![1],
            vec![0, 1],
            vec![1, 0],
            vec![2],
            vec![0, 1, 2],
            vec![1, 0, 2],
            vec![0, 1, 2, 0, 1, 2],
        ];
        for events in ordered_events {
            let (vote_tracker, bank, validator_voting_keypairs, subscriptions) = setup();
            let node_keypair = &validator_voting_keypairs[0].node_keypair;
            let vote_keypair = &validator_voting_keypairs[0].vote_keypair;
            for &e in &events {
                if e == 0 || e == 2 {
                    // Create vote transaction
                    let vote_tx = vote_transaction::new_vote_transaction(
                        vec![vote_slot],
                        vote_bank_hash,
                        Hash::default(),
                        node_keypair,
                        vote_keypair,
                        vote_keypair,
                        switch_proof_hash,
                    );
                    votes_sender.send(vec![vote_tx.clone()]).unwrap();
                }
                if e == 1 || e == 2 {
                    replay_votes_sender
                        .send((
                            vote_keypair.pubkey(),
                            Vote::new(vec![vote_slot], Hash::default()),
                            switch_proof_hash,
                        ))
                        .unwrap();
                }
                let _ = ClusterInfoVoteListener::listen_and_confirm_votes(
                    &votes_receiver,
                    &vote_tracker,
                    &bank,
                    &subscriptions,
                    &gossip_verified_vote_hash_sender,
                    &verified_vote_sender,
                    &replay_votes_receiver,
                    &None,
                    &None,
                );
            }
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(vote_slot).unwrap();
            let r_slot_vote_tracker = &slot_vote_tracker.read().unwrap();

            assert_eq!(
                r_slot_vote_tracker
                    .optimistic_votes_tracker(&vote_bank_hash)
                    .unwrap()
                    .stake(),
                100
            );
            if events == vec![1] {
                // Check `gossip_only_stake` is not incremented
                assert_eq!(r_slot_vote_tracker.gossip_only_stake, 0);
            } else {
                // Check that both the `gossip_only_stake` and `total_voted_stake` both
                // increased
                assert_eq!(r_slot_vote_tracker.gossip_only_stake, 100);
            }
        }
    }

    #[test]
    fn test_run_test_process_votes3() {
        run_test_process_votes3(None);
        run_test_process_votes3(Some(Hash::default()));
    }

    #[test]
    fn test_get_voters_by_epoch() {
        // Create some voters at genesis
        let (vote_tracker, bank, validator_voting_keypairs, _) = setup();
        let last_known_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let last_known_slot = bank
            .epoch_schedule()
            .get_last_slot_in_epoch(last_known_epoch);

        // Check we can get the authorized voters
        for keypairs in &validator_voting_keypairs {
            assert!(vote_tracker
                .get_authorized_voter(&keypairs.vote_keypair.pubkey(), last_known_slot)
                .is_some());
            assert!(vote_tracker
                .get_authorized_voter(&keypairs.vote_keypair.pubkey(), last_known_slot + 1)
                .is_none());
        }

        // Create the set of relevant voters for the next epoch
        let new_epoch = last_known_epoch + 1;
        let first_slot_in_new_epoch = bank.epoch_schedule().get_first_slot_in_epoch(new_epoch);
        let new_keypairs: Vec<_> = (0..10).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let new_epoch_authorized_voters: HashMap<_, _> = new_keypairs
            .iter()
            .chain(validator_voting_keypairs[0..5].iter())
            .map(|keypair| (keypair.vote_keypair.pubkey(), keypair.vote_keypair.pubkey()))
            .collect();

        vote_tracker
            .epoch_authorized_voters
            .write()
            .unwrap()
            .insert(new_epoch, Arc::new(new_epoch_authorized_voters));

        // These keypairs made it into the new epoch
        for keypairs in new_keypairs
            .iter()
            .chain(validator_voting_keypairs[0..5].iter())
        {
            assert!(vote_tracker
                .get_authorized_voter(&keypairs.vote_keypair.pubkey(), first_slot_in_new_epoch)
                .is_some());
        }

        // These keypairs were not refreshed in new epoch
        for keypairs in validator_voting_keypairs[5..10].iter() {
            assert!(vote_tracker
                .get_authorized_voter(&keypairs.vote_keypair.pubkey(), first_slot_in_new_epoch)
                .is_none());
        }
    }

    #[test]
    fn test_vote_tracker_references() {
        // Create some voters at genesis
        let validator_keypairs: Vec<_> =
            (0..2).map(|_| ValidatorVoteKeypairs::new_rand()).collect();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_keypairs,
                vec![100; validator_keypairs.len()],
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let exit = Arc::new(AtomicBool::new(false));
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank = bank_forks.read().unwrap().get(0).unwrap().clone();
        let vote_tracker = VoteTracker::new(&bank);
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks,
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));

        // Send a vote to process, should add a reference to the pubkey for that voter
        // in the tracker
        let validator0_keypairs = &validator_keypairs[0];
        let voted_slot = bank.slot() + 1;
        let vote_tx = vec![vote_transaction::new_vote_transaction(
            // Must vote > root to be processed
            vec![voted_slot],
            Hash::default(),
            Hash::default(),
            &validator0_keypairs.node_keypair,
            &validator0_keypairs.vote_keypair,
            &validator0_keypairs.vote_keypair,
            None,
        )];

        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        ClusterInfoVoteListener::filter_and_confirm_with_new_votes(
            &vote_tracker,
            vote_tx,
            // Add gossip vote for same slot, should not affect outcome
            vec![(
                validator0_keypairs.vote_keypair.pubkey(),
                Vote::new(vec![voted_slot], Hash::default()),
                None,
            )],
            &bank,
            &subscriptions,
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &None,
            &None,
        );

        // Setup next epoch
        let old_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let new_epoch = old_epoch + 1;
        let new_epoch_vote_accounts: HashMap<_, _> = vec![(
            validator0_keypairs.vote_keypair.pubkey(),
            validator0_keypairs.vote_keypair.pubkey(),
        )]
        .into_iter()
        .collect();
        vote_tracker
            .epoch_authorized_voters
            .write()
            .unwrap()
            .insert(new_epoch, Arc::new(new_epoch_vote_accounts));

        // Test with votes across two epochs
        let first_slot_in_new_epoch = bank.epoch_schedule().get_first_slot_in_epoch(new_epoch);

        // Make 2 new votes in two different epochs for the same pubkey,
        // the ref count should go up by 3 * ref_count_per_vote
        // Add 1 vote through the replay channel for a different pubkey,
        // ref count should equal `current_ref_count` for that pubkey.
        let vote_txs: Vec<_> = [first_slot_in_new_epoch - 1, first_slot_in_new_epoch]
            .iter()
            .map(|slot| {
                vote_transaction::new_vote_transaction(
                    // Must vote > root to be processed
                    vec![*slot],
                    Hash::default(),
                    Hash::default(),
                    &validator0_keypairs.node_keypair,
                    &validator0_keypairs.vote_keypair,
                    &validator0_keypairs.vote_keypair,
                    None,
                )
            })
            .collect();

        let new_root_bank =
            Bank::new_from_parent(&bank, &Pubkey::default(), first_slot_in_new_epoch - 2);
        ClusterInfoVoteListener::filter_and_confirm_with_new_votes(
            &vote_tracker,
            vote_txs,
            vec![(
                validator_keypairs[1].vote_keypair.pubkey(),
                Vote::new(vec![first_slot_in_new_epoch], Hash::default()),
                None,
            )],
            &new_root_bank,
            &subscriptions,
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &None,
            &None,
        );
    }

    fn setup() -> (
        Arc<VoteTracker>,
        Arc<Bank>,
        Vec<ValidatorVoteKeypairs>,
        Arc<RpcSubscriptions>,
    ) {
        let validator_voting_keypairs: Vec<_> =
            (0..10).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![100; validator_voting_keypairs.len()],
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let vote_tracker = VoteTracker::new(&bank);
        let exit = Arc::new(AtomicBool::new(false));
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank = bank_forks.read().unwrap().get(0).unwrap().clone();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks,
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));

        // Integrity Checks
        let current_epoch = bank.epoch();
        let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());

        // Check the vote tracker has all the known epoch state on construction
        for epoch in current_epoch..=leader_schedule_epoch {
            assert_eq!(
                vote_tracker
                    .epoch_authorized_voters
                    .read()
                    .unwrap()
                    .get(&epoch)
                    .unwrap(),
                bank.epoch_stakes(epoch).unwrap().epoch_authorized_voters()
            );
        }

        // Check the epoch state is correct
        assert_eq!(
            *vote_tracker.leader_schedule_epoch.read().unwrap(),
            leader_schedule_epoch,
        );
        assert_eq!(*vote_tracker.current_epoch.read().unwrap(), current_epoch);
        (
            Arc::new(vote_tracker),
            bank,
            validator_voting_keypairs,
            subscriptions,
        )
    }

    #[test]
    fn test_verify_votes_empty() {
        solana_logger::setup();
        let votes = vec![];
        let (vote_txs, packets) = ClusterInfoVoteListener::verify_votes(votes);
        assert!(vote_txs.is_empty());
        assert!(packets.is_empty());
    }

    fn verify_packets_len(packets: &[VerifiedVoteMetadata], ref_value: usize) {
        let num_packets: usize = packets
            .iter()
            .map(|vote_metadata| vote_metadata.packet.packets.len())
            .sum();
        assert_eq!(num_packets, ref_value);
    }

    fn test_vote_tx(hash: Option<Hash>) -> Transaction {
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let auth_voter_keypair = Keypair::new();
        vote_transaction::new_vote_transaction(
            vec![0],
            Hash::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &auth_voter_keypair,
            hash,
        )
    }

    fn run_test_verify_votes_1_pass(hash: Option<Hash>) {
        let vote_tx = test_vote_tx(hash);
        let votes = vec![vote_tx];
        let (vote_txs, packets) = ClusterInfoVoteListener::verify_votes(votes);
        assert_eq!(vote_txs.len(), 1);
        verify_packets_len(&packets, 1);
    }

    #[test]
    fn test_verify_votes_1_pass() {
        run_test_verify_votes_1_pass(None);
        run_test_verify_votes_1_pass(Some(Hash::default()));
    }

    fn run_test_bad_vote(hash: Option<Hash>) {
        let vote_tx = test_vote_tx(hash);
        let mut bad_vote = vote_tx.clone();
        bad_vote.signatures[0] = Signature::default();
        let votes = vec![vote_tx.clone(), bad_vote, vote_tx];
        let (vote_txs, packets) = ClusterInfoVoteListener::verify_votes(votes);
        assert_eq!(vote_txs.len(), 2);
        verify_packets_len(&packets, 2);
    }

    #[test]
    fn test_sum_stake() {
        let (_, bank, validator_voting_keypairs, _) = setup();
        let vote_keypair = &validator_voting_keypairs[0].vote_keypair;
        let epoch_stakes = bank.epoch_stakes(bank.epoch()).unwrap();
        let mut gossip_only_stake = 0;

        ClusterInfoVoteListener::sum_stake(
            &mut gossip_only_stake,
            Some(epoch_stakes),
            &vote_keypair.pubkey(),
        );
        assert_eq!(gossip_only_stake, 100);
    }

    #[test]
    fn test_bad_vote() {
        run_test_bad_vote(None);
        run_test_bad_vote(Some(Hash::default()));
    }

    #[test]
    fn test_check_for_leader_bank_and_send_votes() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1000);
        let current_leader_bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let mut bank_vote_sender_state_option: Option<BankVoteSenderState> = None;
        let verified_vote_packets = VerifiedVotePackets::default();
        let (verified_packets_sender, _verified_packets_receiver) = unbounded();

        // 1) If we hand over a `current_leader_bank`, vote sender state should be updated
        ClusterInfoVoteListener::check_for_leader_bank_and_send_votes(
            &mut bank_vote_sender_state_option,
            current_leader_bank.clone(),
            &verified_packets_sender,
            &verified_vote_packets,
        )
        .unwrap();

        assert_eq!(
            bank_vote_sender_state_option.as_ref().unwrap().bank.slot(),
            current_leader_bank.slot()
        );
        bank_vote_sender_state_option
            .as_mut()
            .unwrap()
            .previously_sent_to_bank_votes
            .insert(Signature::new_unique());

        // 2) Handing over the same leader bank again should not update the state
        ClusterInfoVoteListener::check_for_leader_bank_and_send_votes(
            &mut bank_vote_sender_state_option,
            current_leader_bank.clone(),
            &verified_packets_sender,
            &verified_vote_packets,
        )
        .unwrap();
        // If we hand over a `current_leader_bank`, vote sender state should be updated
        assert_eq!(
            bank_vote_sender_state_option.as_ref().unwrap().bank.slot(),
            current_leader_bank.slot()
        );
        assert_eq!(
            bank_vote_sender_state_option
                .as_ref()
                .unwrap()
                .previously_sent_to_bank_votes
                .len(),
            1
        );

        let current_leader_bank = Arc::new(Bank::new_from_parent(
            &current_leader_bank,
            &Pubkey::default(),
            current_leader_bank.slot() + 1,
        ));
        ClusterInfoVoteListener::check_for_leader_bank_and_send_votes(
            &mut bank_vote_sender_state_option,
            current_leader_bank.clone(),
            &verified_packets_sender,
            &verified_vote_packets,
        )
        .unwrap();

        // 3) If we hand over a new `current_leader_bank`, vote sender state should be updated
        // to the new bank
        assert_eq!(
            bank_vote_sender_state_option.as_ref().unwrap().bank.slot(),
            current_leader_bank.slot()
        );
        assert!(bank_vote_sender_state_option
            .as_ref()
            .unwrap()
            .previously_sent_to_bank_votes
            .is_empty());
    }
}
