// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::time::{Instant, Duration};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use ansi_term::Colour;
use ethereum_types::{H256, U256, Address};
use parking_lot::{Mutex, RwLock};
use bytes::Bytes;
use engines::{EthEngine, Seal};
use error::{Error, ExecutionError};
use ethcore_miner::pool::{self, TransactionQueue, VerifiedTransaction};
use ethcore_miner::work_notify::{WorkPoster, NotifyWork};
use ethcore_miner::gas_pricer::GasPricer;
use timer::PerfTimer;
use transaction::{
	self,
	Action,
	UnverifiedTransaction,
	SignedTransaction,
	PendingTransaction,
};
use using_queue::{UsingQueue, GetAction};

use account_provider::{AccountProvider, SignError as AccountError};
use block::{ClosedBlock, IsBlock, Block};
use client::{MiningBlockChainClient, BlockId};
use executive::contract_address;
use header::{Header, BlockNumber};
use miner::MinerService;
use miner::blockchain_client::BlockChainClient;
use receipt::{Receipt, RichReceipt};
use spec::Spec;
use state::State;

/// Different possible definitions for pending transaction set.
#[derive(Debug, PartialEq)]
pub enum PendingSet {
	/// Always just the transactions in the queue. These have had only cheap checks.
	AlwaysQueue,
	/// Always just the transactions in the sealing block. These have had full checks but
	/// may be empty if the node is not actively mining or has force_sealing enabled.
	AlwaysSealing,
	// TODO [ToDr] Enable mining if AlwaysSealing
}

// /// Transaction queue banning settings.
// #[derive(Debug, PartialEq, Clone)]
// pub enum Banning {
// 	/// Banning in transaction queue is disabled
// 	Disabled,
// 	/// Banning in transaction queue is enabled
// 	Enabled {
// 		/// Upper limit of transaction processing time before banning.
// 		offend_threshold: Duration,
// 		/// Number of similar offending transactions before banning.
// 		min_offends: u16,
// 		/// Number of seconds the offender is banned for.
// 		ban_duration: Duration,
// 	},
// }
//
//
const DEFAULT_MINIMAL_GAS_PRICE: u64 = 20_000_000_000;

/// Configures the behaviour of the miner.
#[derive(Debug, PartialEq)]
pub struct MinerOptions {
	/// Force the miner to reseal, even when nobody has asked for work.
	pub force_sealing: bool,
	/// Reseal on receipt of new external transactions.
	pub reseal_on_external_tx: bool,
	/// Reseal on receipt of new local transactions.
	pub reseal_on_own_tx: bool,
	/// Reseal when new uncle block has been imported.
	pub reseal_on_uncle: bool,
	/// Minimum period between transaction-inspired reseals.
	pub reseal_min_period: Duration,
	/// Maximum period between blocks (enables force sealing after that).
	pub reseal_max_period: Duration,
	/// Whether we should fallback to providing all the queue's transactions or just pending.
	pub pending_set: PendingSet,
	/// How many historical work packages can we store before running out?
	pub work_queue_size: usize,
	/// Can we submit two different solutions for the same block and expect both to result in an import?
	pub enable_resubmission: bool,
	/// Create a pending block with maximal possible gas limit.
	/// NOTE: Such block will contain all pending transactions but
	/// will be invalid if mined.
	pub infinite_pending_block: bool,


	// / Strategy to use for prioritizing transactions in the queue.
	// pub tx_queue_strategy: PrioritizationStrategy,
	// / Banning settings.
	// pub tx_queue_banning: Banning,
	/// Do we refuse to accept service transactions even if sender is certified.
	pub refuse_service_transactions: bool,
	/// Transaction pool limits.
	pub pool_limits: pool::Options,
	/// Initial transaction verification options.
	pub pool_verification_options: pool::verifier::Options,
}

impl Default for MinerOptions {
	fn default() -> Self {
		MinerOptions {
			force_sealing: false,
			reseal_on_external_tx: false,
			reseal_on_own_tx: true,
			reseal_on_uncle: false,
			pending_set: PendingSet::AlwaysQueue,
			reseal_min_period: Duration::from_secs(2),
			reseal_max_period: Duration::from_secs(120),
			work_queue_size: 20,
			enable_resubmission: true,
			infinite_pending_block: false,
			// tx_queue_strategy: PrioritizationStrategy::GasPriceOnly,
			// tx_queue_banning: Banning::Disabled,
			refuse_service_transactions: false,
			pool_limits: pool::Options {
				max_count: 16_384,
				max_per_sender: 64,
				max_mem_usage: 8 * 1024 * 1024,
			},
			pool_verification_options: pool::verifier::Options {
				minimal_gas_price: DEFAULT_MINIMAL_GAS_PRICE.into(),
				block_gas_limit: U256::max_value(),
				tx_gas_limit: U256::max_value(),
			},
		}
	}
}

/// Configurable parameters of block authoring.
#[derive(Debug, Default, Clone)]
pub struct AuthoringParams {
	/// Lower and upper bound of block gas limit that we are targeting
	pub gas_range_target: (U256, U256),
	/// Block author
	pub author: Address,
	/// Block extra data
	pub extra_data: Bytes,
}

struct SealingWork {
	queue: UsingQueue<ClosedBlock>,
	enabled: bool,
	next_allowed_reseal: Instant,
	next_mandatory_reseal: Instant,
	sealing_block_last_request: u64,
}

impl SealingWork {
	/// Are we allowed to do a non-mandatory reseal?
	fn reseal_allowed(&self) -> bool {
		Instant::now() > self.next_allowed_reseal
	}

}

/// Keeps track of transactions using priority queue and holds currently mined block.
/// Handles preparing work for "work sealing" or seals "internally" if Engine does not require work.
pub struct Miner {
	// NOTE [ToDr]  When locking always lock in this order!
	sealing: Mutex<SealingWork>,
	params: RwLock<AuthoringParams>,
	listeners: RwLock<Vec<Box<NotifyWork>>>,
	gas_pricer: Mutex<GasPricer>,
	options: MinerOptions,
	// TODO [ToDr] Arc is only required because of price updater
	transaction_queue: Arc<TransactionQueue>,
	engine: Arc<EthEngine>,
	accounts: Option<Arc<AccountProvider>>,
}

impl Miner {
	/// Push listener that will handle new jobs
	pub fn add_work_listener(&self, notifier: Box<NotifyWork>) {
		self.sealing.lock().enabled = true;
		self.listeners.write().push(notifier);
	}

	/// Push an URL that will get new job notifications.
	pub fn add_work_listener_url(&self, urls: &[String]) {
		self.add_work_listener(Box::new(WorkPoster::new(&urls)));
	}

	/// Creates new instance of miner Arc.
	pub fn new(options: MinerOptions, gas_pricer: GasPricer, spec: &Spec, accounts: Option<Arc<AccountProvider>>) -> Miner {
		let limits = options.pool_limits.clone();
		let verifier_options = options.pool_verification_options.clone();

		Miner {
			sealing: Mutex::new(SealingWork{
				queue: UsingQueue::new(options.work_queue_size),
				enabled: options.force_sealing
					|| spec.engine.seals_internally().is_some(),
				next_allowed_reseal: Instant::now(),
				next_mandatory_reseal: Instant::now() + options.reseal_max_period,
				sealing_block_last_request: 0,
			}),
			params: RwLock::new(AuthoringParams::default()),
			listeners: RwLock::new(vec![]),
			gas_pricer: Mutex::new(gas_pricer),
			options,
			transaction_queue: Arc::new(TransactionQueue::new(limits, verifier_options)),
			accounts,
			engine: spec.engine.clone(),
		}
	}

	/// Creates new instance of miner with given spec and accounts.
	///
	/// NOTE This should be only used for tests.
	pub fn new_for_tests(spec: &Spec, accounts: Option<Arc<AccountProvider>>) -> Miner {
		let minimal_gas_price = 0.into();
		Miner::new(MinerOptions {
			pool_verification_options: pool::verifier::Options {
				minimal_gas_price,
				block_gas_limit: U256::max_value(),
				tx_gas_limit: U256::max_value(),
			},
			..Default::default()
		}, GasPricer::new_fixed(minimal_gas_price), spec, accounts)
	}

	fn forced_sealing(&self) -> bool {
		self.options.force_sealing || !self.listeners.read().is_empty()
	}

	/// Clear all pending block states
	pub fn clear(&self) {
		self.sealing.lock().queue.reset();
	}

	/// Get `Some` `clone()` of the current pending block's state or `None` if we're not sealing.
	pub fn pending_state(&self, latest_block_number: BlockNumber) -> Option<State<::state_db::StateDB>> {
		self.map_existing_pending_block(|b| b.state().clone(), latest_block_number)
	}

	/// Get `Some` `clone()` of the current pending block or `None` if we're not sealing.
	pub fn pending_block(&self, latest_block_number: BlockNumber) -> Option<Block> {
		self.map_existing_pending_block(|b| b.to_base(), latest_block_number)
	}

	/// Get `Some` `clone()` of the current pending block header or `None` if we're not sealing.
	pub fn pending_block_header(&self, latest_block_number: BlockNumber) -> Option<Header> {
		self.map_existing_pending_block(|b| b.header().clone(), latest_block_number)
	}

	/// Retrieves an existing pending block iff it's not older than given block number.
	///
	/// NOTE: This will not prepare a new pending block if it's not existing.
	/// See `map_pending_block` for alternative behaviour.
	fn map_existing_pending_block<F, T>(&self, f: F, latest_block_number: BlockNumber) -> Option<T> where
		F: FnOnce(&ClosedBlock) -> T,
	{
		self.from_pending_block(
			latest_block_number,
			|| None,
			|block| Some(f(block)),
		)
	}

	// TODO [ToDr] Get rid of this method.
	//
	// We should never fall back to client, this can be handled on RPC level by returning Option<>
	fn from_pending_block<H, F, G>(&self, latest_block_number: BlockNumber, from_chain: F, map_block: G) -> H
		where F: Fn() -> H, G: FnOnce(&ClosedBlock) -> H {
		let sealing = self.sealing.lock();
		sealing.queue.peek_last_ref().map_or_else(
			|| from_chain(),
			|b| {
				if b.block().header().number() > latest_block_number {
					map_block(b)
				} else {
					from_chain()
				}
			}
		)
	}

	fn client<'a>(&'a self, chain: &'a MiningBlockChainClient) -> BlockChainClient<'a> {
		BlockChainClient::new(
			chain,
			&*self.engine,
			self.accounts.as_ref().map(|x| &**x),
			self.options.refuse_service_transactions,
		)
	}

	/// Prepares new block for sealing including top transactions from queue.
	fn prepare_block(&self, chain: &MiningBlockChainClient) -> (ClosedBlock, Option<H256>) {
		let _timer = PerfTimer::new("prepare_block");
		let chain_info = chain.chain_info();

		// Open block
		let (mut open_block, original_work_hash) = {
			let mut sealing = self.sealing.lock();
			let last_work_hash = sealing.queue.peek_last_ref().map(|pb| pb.block().fields().header.hash());
			let best_hash = chain_info.best_block_hash;

			// check to see if last ClosedBlock in would_seals is actually same parent block.
			// if so
			//   duplicate, re-open and push any new transactions.
			//   if at least one was pushed successfully, close and enqueue new ClosedBlock;
			//   otherwise, leave everything alone.
			// otherwise, author a fresh block.
			let mut open_block = match sealing.queue.pop_if(|b| b.block().fields().header.parent_hash() == &best_hash) {
				Some(old_block) => {
					trace!(target: "miner", "prepare_block: Already have previous work; updating and returning");
					// add transactions to old_block
					chain.reopen_block(old_block)
				}
				None => {
					// block not found - create it.
					trace!(target: "miner", "prepare_block: No existing work - making new block");
					let params = self.params.read().clone();
					chain.prepare_open_block(
						params.author,
						params.gas_range_target,
						params.extra_data,
					)
				}
			};

			if self.options.infinite_pending_block {
				open_block.set_gas_limit(!U256::zero());
			}

			(open_block, last_work_hash)
		};

		let mut invalid_transactions = HashSet::new();
		let mut not_allowed_transactions = HashSet::new();
		// let mut transactions_to_penalize = HashSet::new();
		let block_number = open_block.block().fields().header.number();

		let mut tx_count = 0usize;
		let mut skipped_transactions = 0usize;

		let client = self.client(chain);
		let engine_params = self.engine.params();
		let nonce_cap: Option<U256> = if chain_info.best_block_number + 1 >= engine_params.dust_protection_transition {
			Some((engine_params.nonce_cap_increment * (chain_info.best_block_number + 1)).into())
		} else {
			None
		};

		let pending: Vec<Arc<_>> = self.transaction_queue.pending(
			client.clone(),
			chain_info.best_block_number,
			chain_info.best_block_timestamp,
			// TODO [ToDr] Take only part?
			|transactions| transactions.collect(),
			// nonce_cap,
		);

		for tx in pending {
			let start = Instant::now();

			let transaction = tx.signed().clone();
			let hash = transaction.hash();

			// Re-verify transaction again vs current state.
			let result = client.verify_signed(&transaction)
				.map_err(Error::Transaction)
				.and_then(|_| {
					open_block.push_transaction(transaction, None)
				});

			let took = start.elapsed();

			// Check for heavy transactions
			// match self.options.tx_queue_banning {
			// 	Banning::Enabled { ref offend_threshold, .. } if &took > offend_threshold => {
			// 		match self.transaction_queue.write().ban_transaction(&hash) {
			// 			true => {
			// 				warn!(target: "miner", "Detected heavy transaction. Banning the sender and recipient/code.");
			// 			},
			// 			false => {
			// 				transactions_to_penalize.insert(hash);
			// 				debug!(target: "miner", "Detected heavy transaction. Penalizing sender.")
			// 			}
			// 		}
			// 	},
			// 	_ => {},
			// }
			trace!(target: "miner", "Adding tx {:?} took {:?}", hash, took);
			match result {
				Err(Error::Execution(ExecutionError::BlockGasLimitReached { gas_limit, gas_used, gas })) => {
					debug!(target: "miner", "Skipping adding transaction to block because of gas limit: {:?} (limit: {:?}, used: {:?}, gas: {:?})", hash, gas_limit, gas_used, gas);

					// Penalize transaction if it's above current gas limit
					if gas > gas_limit {
						invalid_transactions.insert(hash);
					}

					// Exit early if gas left is smaller then min_tx_gas
					let min_tx_gas: U256 = 21000.into();	// TODO: figure this out properly.
					let gas_left = gas_limit - gas_used;
					if gas_left < min_tx_gas {
						break;
					}

					// Avoid iterating over the entire queue in case block is almost full.
					skipped_transactions += 1;
					if skipped_transactions > 8 {
						break;
					}
				},
				// Invalid nonce error can happen only if previous transaction is skipped because of gas limit.
				// If there is errornous state of transaction queue it will be fixed when next block is imported.
				Err(Error::Execution(ExecutionError::InvalidNonce { expected, got })) => {
					debug!(target: "miner", "Skipping adding transaction to block because of invalid nonce: {:?} (expected: {:?}, got: {:?})", hash, expected, got);
				},
				// already have transaction - ignore
				Err(Error::Transaction(transaction::Error::AlreadyImported)) => {},
				Err(Error::Transaction(transaction::Error::NotAllowed)) => {
					not_allowed_transactions.insert(hash);
					debug!(target: "miner", "Skipping non-allowed transaction for sender {:?}", hash);
				},
				Err(e) => {
					invalid_transactions.insert(hash);
					debug!(
						target: "miner", "Error adding transaction to block: number={}. transaction_hash={:?}, Error: {:?}", block_number, hash, e
					);
				},
				// imported ok
				_ => tx_count += 1,
			}
		}
		trace!(target: "miner", "Pushed {} transactions", tx_count);

		let block = open_block.close();

		{
			self.transaction_queue.remove(invalid_transactions.iter(), true);
			self.transaction_queue.remove(not_allowed_transactions.iter(), false);

			// TODO [ToDr] Penalize
			// for hash in transactions_to_penalize {
				// queue.penalize(&hash);
			// }
		}

		(block, original_work_hash)
	}

	/// Check is reseal is allowed and necessary.
	fn requires_reseal(&self, best_block: BlockNumber) -> bool {
		let mut sealing = self.sealing.lock();
		if sealing.enabled {
			trace!(target: "miner", "requires_reseal: sealing is disabled");
			return false
		}

		let has_local_transactions = self.transaction_queue.has_local_transactions();
		trace!(target: "miner", "requires_reseal: sealing enabled");

		let last_request = sealing.sealing_block_last_request;
		let sealing_enabled = self.forced_sealing()
			|| has_local_transactions
			|| self.engine.seals_internally().is_some()
			|| (best_block > last_request && best_block - last_request > SEALING_TIMEOUT_IN_BLOCKS);

		let should_disable_sealing = !sealing_enabled;

		trace!(target: "miner", "requires_reseal: should_disable_sealing={}; best_block={}, last_request={}", should_disable_sealing, best_block, last_request);

		if should_disable_sealing {
			trace!(target: "miner", "Miner sleeping (current {}, last {})", best_block, last_request);
			sealing.enabled = false;
			sealing.queue.reset();
			false
		} else {
			// sealing enabled and we don't want to sleep.
			sealing.next_allowed_reseal = Instant::now() + self.options.reseal_min_period;
			true
		}
	}

	/// Attempts to perform internal sealing (one that does not require work) and handles the result depending on the type of Seal.
	fn seal_and_import_block_internally(&self, chain: &MiningBlockChainClient, block: ClosedBlock) -> bool {
		let mut sealing = self.sealing.lock();
		if block.transactions().is_empty()
			&& !self.forced_sealing()
			&& Instant::now() <= sealing.next_mandatory_reseal
		{
			return false
		}

		trace!(target: "miner", "seal_block_internally: attempting internal seal.");

		let parent_header = match chain.block_header(BlockId::Hash(*block.header().parent_hash())) {
			Some(hdr) => hdr.decode(),
			None => return false,
		};

		match self.engine.generate_seal(block.block(), &parent_header) {
			// Save proposal for later seal submission and broadcast it.
			Seal::Proposal(seal) => {
				trace!(target: "miner", "Received a Proposal seal.");
				sealing.next_mandatory_reseal = Instant::now() + self.options.reseal_max_period;
				sealing.queue.push(block.clone());
				sealing.queue.use_last_ref();

				block
					.lock()
					.seal(&*self.engine, seal)
					.map(|sealed| {
						chain.broadcast_proposal_block(sealed);
						true
					})
					.unwrap_or_else(|e| {
						warn!("ERROR: seal failed when given internally generated seal: {}", e);
						false
					})
			},
			// Directly import a regular sealed block.
			Seal::Regular(seal) => {
				sealing.next_mandatory_reseal = Instant::now() + self.options.reseal_max_period;
				block
					.lock()
					.seal(&*self.engine, seal)
					.map(|sealed| chain.import_sealed_block(sealed).is_ok())
					.unwrap_or_else(|e| {
						warn!("ERROR: seal failed when given internally generated seal: {}", e);
						false
					})
			},
			Seal::None => false,
		}
	}

	/// Prepares work which has to be done to seal.
	fn prepare_work(&self, block: ClosedBlock, original_work_hash: Option<H256>) {
		let (work, is_new) = {
			let block_header = block.block().fields().header.clone();
			let block_hash = block_header.hash();

			let mut sealing = self.sealing.lock();
			let last_work_hash = sealing.queue.peek_last_ref().map(|pb| pb.block().fields().header.hash());

			trace!(
				target: "miner",
				"prepare_work: Checking whether we need to reseal: orig={:?} last={:?}, this={:?}",
				original_work_hash, last_work_hash, block_hash
			);

			let (work, is_new) = if last_work_hash.map_or(true, |h| h != block_hash) {
				trace!(
					target: "miner",
					"prepare_work: Pushing a new, refreshed or borrowed pending {}...",
					block_hash
				);
				let is_new = original_work_hash.map_or(true, |h| h != block_hash);

				sealing.queue.push(block);
				// If push notifications are enabled we assume all work items are used.
				if is_new && !self.listeners.read().is_empty() {
					sealing.queue.use_last_ref();
				}

				(Some((block_hash, *block_header.difficulty(), block_header.number())), is_new)
			} else {
				(None, false)
			};
			trace!(
				target: "miner",
				"prepare_work: leaving (last={:?})",
				sealing.queue.peek_last_ref().map(|b| b.block().fields().header.hash())
			);
			(work, is_new)
		};
		if is_new {
			work.map(|(pow_hash, difficulty, number)| {
				for notifier in self.listeners.read().iter() {
					notifier.notify(pow_hash, difficulty, number)
				}
			});
		}
	}

	fn update_transaction_queue_limits(&self, block_gas_limit: U256) {
		debug!(target: "miner", "minimal_gas_price: recalibrating...");
		let txq = self.transaction_queue.clone();
		let mut options = self.options.pool_verification_options.clone();
		self.gas_pricer.lock().recalibrate(move |gas_price| {
			debug!(target: "miner", "minimal_gas_price: Got gas price! {}", gas_price);
			options.minimal_gas_price = gas_price;
			options.block_gas_limit = block_gas_limit;
			txq.set_verifier_options(options);
		});
	}

	/// Returns true if we had to prepare new pending block.
	fn prepare_work_sealing(&self, client: &MiningBlockChainClient) -> bool {
		trace!(target: "miner", "prepare_work_sealing: entering");
		let prepare_new = {
			let mut sealing = self.sealing.lock();
			let have_work = sealing.queue.peek_last_ref().is_some();
			trace!(target: "miner", "prepare_work_sealing: have_work={}", have_work);
			if !have_work {
				sealing.enabled = true;
				true
			} else {
				false
			}
		};

		if prepare_new {
			// --------------------------------------------------------------------------
			// | NOTE Code below requires transaction_queue and sealing locks.          |
			// | Make sure to release the locks before calling that method.             |
			// --------------------------------------------------------------------------
			let (block, original_work_hash) = self.prepare_block(client);
			self.prepare_work(block, original_work_hash);
		}

		let best_number = client.chain_info().best_block_number;
		let mut sealing = self.sealing.lock();
		if sealing.sealing_block_last_request != best_number {
			trace!(
				target: "miner",
				"prepare_work_sealing: Miner received request (was {}, now {}) - waking up.",
				sealing.sealing_block_last_request, best_number
			);
			sealing.sealing_block_last_request = best_number;
		}

		// Return if we restarted
		prepare_new
	}
}

const SEALING_TIMEOUT_IN_BLOCKS : u64 = 5;

impl MinerService for Miner {

	fn authoring_params(&self) -> AuthoringParams {
		self.params.read().clone()
	}

	fn set_gas_range_target(&self, gas_range_target: (U256, U256)) {
		self.params.write().gas_range_target = gas_range_target;
	}

	fn set_extra_data(&self, extra_data: Bytes) {
		self.params.write().extra_data = extra_data;
	}

	fn set_author(&self, address: Address, password: Option<String>) -> Result<(), AccountError> {
		self.params.write().author = address;

		if self.engine.seals_internally().is_some() {
			if let Some(ref ap) = self.accounts {
				let password = password.unwrap_or_default();
				// Sign test message
				ap.sign(address.clone(), Some(password.clone()), Default::default())?;
				// Enable sealing
				self.sealing.lock().enabled = true;
				// --------------------------------------------------------------------------
				// | NOTE Code below may require author and sealing locks                   |
				// | (some `Engine`s call `EngineClient.update_sealing()`)                  |
				// | Make sure to release the locks before calling that method.             |
				// --------------------------------------------------------------------------
				self.engine.set_signer(ap.clone(), address, password);
				Ok(())
			} else {
				warn!(target: "miner", "No account provider");
				Err(AccountError::NotFound)
			}
		} else {
			Ok(())
		}
	}

	fn sensible_gas_price(&self) -> U256 {
		// 10% above our minimum.
		self.transaction_queue.current_worst_gas_price() * 110u32 / 100.into()
	}

	fn sensible_gas_limit(&self) -> U256 {
		self.params.read().gas_range_target.0 / 5.into()
	}

	fn import_external_transactions(
		&self,
		chain: &MiningBlockChainClient,
		transactions: Vec<UnverifiedTransaction>
	) -> Vec<Result<(), transaction::Error>> {
		trace!(target: "external_tx", "Importing external transactions");
		let client = self.client(chain);
		let results = self.transaction_queue.import(
			client,
			transactions.into_iter().map(pool::verifier::Transaction::Unverified).collect(),
		);

		if !results.is_empty() && self.options.reseal_on_external_tx &&	self.sealing.lock().reseal_allowed() {
			// --------------------------------------------------------------------------
			// | NOTE Code below requires sealing locks.                                |
			// | Make sure to release the locks before calling that method.             |
			// --------------------------------------------------------------------------
			self.update_sealing(chain);
		}

		results
	}

	fn import_own_transaction(
		&self,
		chain: &MiningBlockChainClient,
		pending: PendingTransaction,
	) -> Result<(), transaction::Error> {

		trace!(target: "own_tx", "Importing transaction: {:?}", pending);

		let client = self.client(chain);
		let imported = self.transaction_queue.import(
			client,
			vec![pool::verifier::Transaction::Local(pending)]
		).pop().expect("one result returned per added transaction; one added => one result; qed");

		// --------------------------------------------------------------------------
		// | NOTE Code below requires transaction_queue and sealing locks.          |
		// | Make sure to release the locks before calling that method.             |
		// --------------------------------------------------------------------------
		if imported.is_ok() && self.options.reseal_on_own_tx && self.sealing.lock().reseal_allowed() {
			// Make sure to do it after transaction is imported and lock is droped.
			// We need to create pending block and enable sealing.
			if self.engine.seals_internally().unwrap_or(false) || !self.prepare_work_sealing(chain) {
				// If new block has not been prepared (means we already had one)
				// or Engine might be able to seal internally,
				// we need to update sealing.
				self.update_sealing(chain);
			}
		}

		imported
	}

	// fn local_transactions(&self) -> BTreeMap<H256, LocalTransactionStatus> {
	// 	let queue = self.transaction_queue.read();
	// 	queue.local_transactions()
	// 		.iter()
	// 		.map(|(hash, status)| (*hash, status.clone()))
	// 		.collect()
	// }

	fn future_transactions(&self) -> Vec<Arc<VerifiedTransaction>> {
		unimplemented!()
		// self.transaction_queue.read().future_transactions()
	}

	fn ready_transactions(&self, chain: &MiningBlockChainClient) -> Vec<Arc<VerifiedTransaction>> {
		let chain_info = chain.chain_info();
		match self.options.pending_set {
			PendingSet::AlwaysQueue => {
				let client = self.client(chain);

				self.transaction_queue.pending(
					client,
					chain_info.best_block_number,
					chain_info.best_block_timestamp,
					|transactions| transactions.collect(),
				)
			},
			PendingSet::AlwaysSealing => {
				self.from_pending_block(
					chain_info.best_block_number,
					Vec::new,
					|sealing| sealing.transactions()
						.iter()
						.map(|signed| pool::VerifiedTransaction::from_pending_block_transaction(signed.clone()))
						.map(Arc::new)
						.collect()
				)
			},
		}
	}

	fn transaction(&self, best_block: BlockNumber, hash: &H256) -> Option<PendingTransaction> {
		match self.options.pending_set {
			PendingSet::AlwaysQueue => self.transaction_queue.find(hash).map(|t| t.pending().clone()),
			PendingSet::AlwaysSealing => {
				self.from_pending_block(
					best_block,
					|| None,
					|sealing| sealing.transactions().iter().find(|t| &t.hash() == hash).cloned().map(Into::into)
				)
			},
		}
	}

	fn last_nonce(&self, address: &Address) -> Option<U256> {
		// TODO [ToDr] missing!
		unimplemented!()
	}

	fn pending_transactions(&self, best_block: BlockNumber) -> Option<Vec<SignedTransaction>> {
		self.from_pending_block(
			best_block,
			|| None,
			|pending| Some(pending.transactions().to_vec()),
		)
	}

	// TODO [ToDr] This is pretty inconsistent (you can get a ready_transaction, but no receipt for it)
	fn pending_receipt(&self, best_block: BlockNumber, hash: &H256) -> Option<RichReceipt> {
		self.from_pending_block(
			best_block,
			// TODO [ToDr] Should try to find transaction in best block!
			|| None,
			|pending| {
				let txs = pending.transactions();
				txs.iter()
					.map(|t| t.hash())
					.position(|t| t == *hash)
					.map(|index| {
						let receipts = pending.receipts();
						let prev_gas = if index == 0 { Default::default() } else { receipts[index - 1].gas_used };
						let tx = &txs[index];
						let receipt = &receipts[index];
						RichReceipt {
							transaction_hash: hash.clone(),
							transaction_index: index,
							cumulative_gas_used: receipt.gas_used,
							gas_used: receipt.gas_used - prev_gas,
							contract_address: match tx.action {
								Action::Call(_) => None,
								Action::Create => {
									let sender = tx.sender();
									Some(contract_address(self.engine.create_address_scheme(pending.header().number()), &sender, &tx.nonce, &tx.data).0)
								}
							},
							logs: receipt.logs.clone(),
							log_bloom: receipt.log_bloom,
							outcome: receipt.outcome.clone(),
						}
					})
			}
		)
	}

	fn pending_receipts(&self, best_block: BlockNumber) -> Option<BTreeMap<H256, Receipt>> {
		self.from_pending_block(
			best_block,
			// TODO [ToDr] This is wrong should look in latest block!
			|| None,
			|pending| {
				let hashes = pending.transactions().iter().map(|t| t.hash());
				let receipts = pending.receipts().iter().cloned();

				Some(hashes.zip(receipts).collect())
			}
		)
	}

	fn can_produce_work_package(&self) -> bool {
		self.engine.seals_internally().is_none()
	}


	// TODO [ToDr] Pass sealing lock guard
	/// Update sealing if required.
	/// Prepare the block and work if the Engine does not seal internally.
	fn update_sealing(&self, chain: &MiningBlockChainClient) {
		trace!(target: "miner", "update_sealing");
		const NO_NEW_CHAIN_WITH_FORKS: &str = "Your chain specification contains one or more hard forks which are required to be \
			on by default. Please remove these forks and start your chain again.";

		if self.requires_reseal(chain.chain_info().best_block_number) {
			// --------------------------------------------------------------------------
			// | NOTE Code below requires transaction_queue and sealing locks.          |
			// | Make sure to release the locks before calling that method.             |
			// --------------------------------------------------------------------------
			trace!(target: "miner", "update_sealing: preparing a block");
			let (block, original_work_hash) = self.prepare_block(chain);

			// refuse to seal the first block of the chain if it contains hard forks
			// which should be on by default.
			if block.block().fields().header.number() == 1 && self.engine.params().contains_bugfix_hard_fork() {
				warn!("{}", NO_NEW_CHAIN_WITH_FORKS);
				return;
			}

			match self.engine.seals_internally() {
				Some(true) => {
					trace!(target: "miner", "update_sealing: engine indicates internal sealing");
					if self.seal_and_import_block_internally(chain, block) {
						trace!(target: "miner", "update_sealing: imported internally sealed block");
					}
				},
				Some(false) => trace!(target: "miner", "update_sealing: engine is not keen to seal internally right now"),
				None => {
					trace!(target: "miner", "update_sealing: engine does not seal internally, preparing work");
					self.prepare_work(block, original_work_hash)
				},
			}
		}
	}

	fn is_currently_sealing(&self) -> bool {
		self.sealing.lock().queue.is_in_use()
	}

	fn map_pending_block<F, T>(&self, chain: &MiningBlockChainClient, f: F) -> Option<T> where F: FnOnce(&ClosedBlock) -> T {
		self.prepare_work_sealing(chain);
		self.map_existing_pending_block(f, chain.chain_info().best_block_number)
	}

	fn submit_seal(&self, chain: &MiningBlockChainClient, block_hash: H256, seal: Vec<Bytes>) -> Result<(), Error> {
		let result =
			if let Some(b) = self.sealing.lock().queue.get_used_if(
				if self.options.enable_resubmission {
					GetAction::Clone
				} else {
					GetAction::Take
				},
				|b| &b.hash() == &block_hash
			) {
				trace!(target: "miner", "Submitted block {}={}={} with seal {:?}", block_hash, b.hash(), b.header().bare_hash(), seal);
				b.lock().try_seal(&*self.engine, seal).or_else(|(e, _)| {
					warn!(target: "miner", "Mined solution rejected: {}", e);
					Err(Error::PowInvalid)
				})
			} else {
				warn!(target: "miner", "Submitted solution rejected: Block unknown or out of date.");
				Err(Error::PowHashInvalid)
			};
		result.and_then(|sealed| {
			let n = sealed.header().number();
			let h = sealed.header().hash();
			chain.import_sealed_block(sealed)?;
			info!(target: "miner", "Submitted block imported OK. #{}: {}", Colour::White.bold().paint(format!("{}", n)), Colour::White.bold().paint(format!("{:x}", h)));
			Ok(())
		})
	}

	fn chain_new_blocks(&self, chain: &MiningBlockChainClient, imported: &[H256], _invalid: &[H256], enacted: &[H256], retracted: &[H256]) {
		trace!(target: "miner", "chain_new_blocks");

		// 1. We ignore blocks that were `imported` unless resealing on new uncles is enabled.
		// 2. We ignore blocks that are `invalid` because it doesn't have any meaning in terms of the transactions that
		//    are in those blocks

		// First update gas limit in transaction queue and minimal gas price.
		let gas_limit = chain.best_block_header().gas_limit();
		self.update_transaction_queue_limits(gas_limit);

		// Then import all transactions...
		let client = self.client(chain);
		{
			// TODO [ToDr] Parallelize
			for hash in retracted {
				let block = chain.block(BlockId::Hash(*hash))
					.expect("Client is sending message after commit to db and inserting to chain; the block is available; qed");
				let txs = block.transactions()
					.into_iter()
					.map(pool::verifier::Transaction::Retracted)
					.collect();
				let _ = self.transaction_queue.import(
					client.clone(),
					txs,
				);
			}
		}

		// ...and at the end remove the old ones
		self.transaction_queue.cull(client);

		if enacted.len() > 0 || (imported.len() > 0 && self.options.reseal_on_uncle) {
			// --------------------------------------------------------------------------
			// | NOTE Code below requires transaction_queue and sealing locks.          |
			// | Make sure to release the locks before calling that method.             |
			// --------------------------------------------------------------------------
			self.update_sealing(chain);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use ethkey::{Generator, Random};
	use hash::keccak;
	use rustc_hex::FromHex;

	use transaction::Transaction;
	use client::{BlockChainClient, TestBlockChainClient, EachBlockWith};
	use tests::helpers::{generate_dummy_client, generate_dummy_client_with_spec_and_accounts};

	#[test]
	fn should_prepare_block_to_seal() {
		// given
		let client = TestBlockChainClient::default();
		let miner = Miner::new_for_tests(&Spec::new_test(), None);

		// when
		let sealing_work = miner.map_pending_block(&client, |_| ());
		assert!(sealing_work.is_some(), "Expected closed block");
	}

	#[test]
	fn should_still_work_after_a_couple_of_blocks() {
		// given
		let client = TestBlockChainClient::default();
		let miner = Miner::new_for_tests(&Spec::new_test(), None);

		let res = miner.map_pending_block(&client, |b| b.block().fields().header.hash());
		assert!(res.is_some());
		assert!(miner.submit_seal(&client, res.unwrap(), vec![]).is_ok());

		// two more blocks mined, work requested.
		client.add_blocks(1, EachBlockWith::Uncle);
		miner.map_pending_block(&client, |b| b.block().fields().header.hash());

		client.add_blocks(1, EachBlockWith::Uncle);
		miner.map_pending_block(&client, |b| b.block().fields().header.hash());

		// solution to original work submitted.
		assert!(miner.submit_seal(&client, res.unwrap(), vec![]).is_ok());
	}

	fn miner() -> Miner {
		Miner::new(
			MinerOptions {
				force_sealing: false,
				reseal_on_external_tx: false,
				reseal_on_own_tx: true,
				reseal_on_uncle: false,
				reseal_min_period: Duration::from_secs(5),
				reseal_max_period: Duration::from_secs(120),
				pending_set: PendingSet::AlwaysSealing,
				work_queue_size: 5,
				enable_resubmission: true,
				infinite_pending_block: false,
				refuse_service_transactions: false,
				pool_limits: Default::default(),
				pool_verification_options: pool::verifier::Options {
					minimal_gas_price: 0.into(),
					block_gas_limit: U256::max_value(),
					tx_gas_limit: U256::max_value(),
				},
			},
			GasPricer::new_fixed(0u64.into()),
			&Spec::new_test(),
			None, // accounts provider
		)
	}

	fn transaction() -> SignedTransaction {
		transaction_with_chain_id(2)
	}

	fn transaction_with_chain_id(chain_id: u64) -> SignedTransaction {
		let keypair = Random.generate().unwrap();
		Transaction {
			action: Action::Create,
			value: U256::zero(),
			data: "3331600055".from_hex().unwrap(),
			gas: U256::from(100_000),
			gas_price: U256::zero(),
			nonce: U256::zero(),
		}.sign(keypair.secret(), Some(chain_id))
	}

	#[test]
	fn should_make_pending_block_when_importing_own_transaction() {
		// given
		let client = TestBlockChainClient::default();
		let miner = miner();
		let transaction = transaction();
		let best_block = 0;
		// when
		let res = miner.import_own_transaction(&client, PendingTransaction::new(transaction, None));

		// then
		assert_eq!(res.unwrap(), ());
		assert_eq!(miner.pending_transactions(best_block).unwrap().len(), 1);
		assert_eq!(miner.pending_receipts(best_block).unwrap().len(), 1);
		assert_eq!(miner.ready_transactions(&client).len(), 1);
		// This method will let us know if pending block was created (before calling that method)
		assert!(!miner.prepare_work_sealing(&client));
	}

	#[test]
	fn should_not_use_pending_block_if_best_block_is_higher() {
		// given
		let client = TestBlockChainClient::default();
		let miner = miner();
		let transaction = transaction();
		let best_block = 10;
		// when
		let res = miner.import_own_transaction(&client, PendingTransaction::new(transaction, None));

		// then
		assert_eq!(res.unwrap(), ());
		assert_eq!(miner.pending_transactions(best_block).unwrap().len(), 1);
		assert_eq!(miner.pending_receipts(best_block).unwrap().len(), 0);
		assert_eq!(miner.ready_transactions(&client).len(), 0);
	}

	#[test]
	fn should_import_external_transaction() {
		// given
		let client = TestBlockChainClient::default();
		let miner = miner();
		let transaction = transaction().into();
		let best_block = 0;
		// when
		let res = miner.import_external_transactions(&client, vec![transaction]).pop().unwrap();

		// then
		assert_eq!(res.unwrap(), ());
		assert_eq!(miner.pending_transactions(best_block).unwrap().len(), 0);
		assert_eq!(miner.pending_receipts(best_block).unwrap().len(), 0);
		assert_eq!(miner.ready_transactions(&client).len(), 1);
		// This method will let us know if pending block was created (before calling that method)
		assert!(miner.prepare_work_sealing(&client));
	}

	#[test]
	fn should_not_seal_unless_enabled() {
		let miner = miner();
		let client = TestBlockChainClient::default();
		// By default resealing is not required.
		assert!(!miner.requires_reseal(1u8.into()));

		miner.import_external_transactions(&client, vec![transaction().into()]).pop().unwrap().unwrap();
		assert!(miner.prepare_work_sealing(&client));
		// Unless asked to prepare work.
		assert!(miner.requires_reseal(1u8.into()));
	}

	#[test]
	fn internal_seals_without_work() {
		let spec = Spec::new_instant();
		let miner = Miner::new_for_tests(&spec, None);

		let client = generate_dummy_client(2);

		let import = miner.import_external_transactions(&*client, vec![transaction_with_chain_id(spec.chain_id()).into()]).pop().unwrap();
		assert_eq!(import.unwrap(), ());

		miner.update_sealing(&*client);
		client.flush_queue();
		assert!(miner.pending_block(0).is_none());
		assert_eq!(client.chain_info().best_block_number, 3 as BlockNumber);

		assert!(miner.import_own_transaction(&*client, PendingTransaction::new(transaction_with_chain_id(spec.chain_id()).into(), None)).is_ok());

		miner.update_sealing(&*client);
		client.flush_queue();
		assert!(miner.pending_block(0).is_none());
		assert_eq!(client.chain_info().best_block_number, 4 as BlockNumber);
	}

	#[test]
	fn should_fail_setting_engine_signer_without_account_provider() {
		let spec = Spec::new_instant;
		let tap = Arc::new(AccountProvider::transient_provider());
		let addr = tap.insert_account(keccak("1").into(), "").unwrap();
		let client = generate_dummy_client_with_spec_and_accounts(spec, None);
		assert!(match client.miner().set_author(addr, Some("".into())) { Err(AccountError::NotFound) => true, _ => false });
	}
}
