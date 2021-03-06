// Copyright 2015-2018 Parity Technologies (UK) Ltd.
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

//! A mutable state representation suitable to execute transactions.
//! Generic over a `Backend`. Deals with `Account`s.
//! Unconfirmed sub-states are managed with `checkpoint`s which may be canonicalized
//! or rolled back.

use hash::{keccak, KECCAK_EMPTY, KECCAK_NULL_RLP};
use std::cell::{RefCell, RefMut};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use error::{Error, ErrorKind};
use executed::{Executed, ExecutionError};
use executive::{Executive, TransactOptions};
use factory::Factories;
use factory::VmFactory;
use journaldb::overlaydb::OverlayDB;
use machine::EthereumMachine as Machine;
use pod_account::*;
use pod_state::{self, PodState};
use receipt::{Receipt, TransactionOutcome};
use state_db::StateDB;
use trace::{self, FlatTrace, VMTrace};
use trace_ext::ExtTracer;
use transaction::{self, SignedTransaction};
use types::basic_account::BasicAccount;
use types::state_diff::StateDiff;
use vm::{ConfidentialCtx, EnvInfo, OasisContract};

use bytes::Bytes;
use ethereum_types::{Address, H256, U256};
use failure::Fallible;
use hashdb::{AsHashDB, HashDB};
use kvdb::DBValue;

use trie;
use trie::recorder::Recorder;
use trie::{Trie, TrieDB, TrieError};

use crate::mkvs::{PrefixedMKVS, ReadOnlyPrefixedMKVS, MKVS};

mod account;
mod substate;

pub mod backend;

pub use self::account::{Account, MKVS_KEY_CODE, MKVS_KEY_PREFIX_STORAGE};
pub use self::backend::{Backend, Basic as BasicBackend};
pub use self::substate::Substate;

/// Used to return information about an `State::apply` operation.
pub struct ApplyOutcome<T, V> {
	/// The receipt for the applied transaction.
	pub receipt: Receipt,
	/// The output of the applied transaction.
	pub output: Bytes,
	/// The trace for the applied transaction, empty if tracing was not produced.
	pub trace: Vec<T>,
	/// The VM trace for the applied transaction, None if tracing was not produced.
	pub vm_trace: Option<V>,
}

/// Result type for the execution ("application") of a transaction.
pub type ApplyResult<T, V> = Result<ApplyOutcome<T, V>, Error>;

/// Return type of proof validity check.
#[derive(Debug, Clone)]
pub enum ProvedExecution {
	/// Proof wasn't enough to complete execution.
	BadProof,
	/// The transaction failed, but not due to a bad proof.
	Failed(ExecutionError),
	/// The transaction successfully completd with the given proof.
	Complete(Executed),
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
/// Account modification state. Used to check if the account was
/// Modified in between commits and overall.
enum AccountState {
	/// Account was loaded from disk and never modified in this state object.
	CleanFresh,
	/// Account was loaded from the global cache and never modified.
	CleanCached,
	/// Account has been modified and is not committed to the trie yet.
	/// This is set if any of the account data is changed, including
	/// storage and code.
	Dirty,
	/// Account was modified and committed to the trie.
	Committed,
}

#[derive(Debug)]
/// In-memory copy of the account data. Holds the optional account
/// and the modification status.
/// Account entry can contain existing (`Some`) or non-existing
/// account (`None`)
struct AccountEntry {
	/// Account entry. `None` if account known to be non-existant.
	account: Option<Account>,
	/// Unmodified account balance.
	old_balance: Option<U256>,
	/// Entry state.
	state: AccountState,
}

// Account cache item. Contains account data and
// modification state
impl AccountEntry {
	fn is_dirty(&self) -> bool {
		self.state == AccountState::Dirty
	}

	fn exists_and_is_null(&self) -> bool {
		self.account.as_ref().map_or(false, |a| a.is_null())
	}

	/// Clone dirty data into new `AccountEntry`. This includes
	/// basic account data and modified storage keys.
	/// Returns None if clean.
	fn clone_if_dirty(&self) -> Option<AccountEntry> {
		match self.is_dirty() {
			true => Some(self.clone_dirty()),
			false => None,
		}
	}

	/// Clone dirty data into new `AccountEntry`. This includes
	/// basic account data and modified storage keys.
	fn clone_dirty(&self) -> AccountEntry {
		AccountEntry {
			old_balance: self.old_balance,
			account: self.account.as_ref().map(Account::clone_dirty),
			state: self.state,
		}
	}

	// Create a new account entry and mark it as dirty.
	fn new_dirty(account: Option<Account>) -> AccountEntry {
		AccountEntry {
			old_balance: account.as_ref().map(|a| a.balance().clone()),
			account: account,
			state: AccountState::Dirty,
		}
	}

	// Create a new account entry and mark it as clean.
	fn new_clean(account: Option<Account>) -> AccountEntry {
		AccountEntry {
			old_balance: account.as_ref().map(|a| a.balance().clone()),
			account: account,
			state: AccountState::CleanFresh,
		}
	}

	// Create a new account entry and mark it as clean and cached.
	fn new_clean_cached(account: Option<Account>) -> AccountEntry {
		AccountEntry {
			old_balance: account.as_ref().map(|a| a.balance().clone()),
			account: account,
			state: AccountState::CleanCached,
		}
	}

	// Replace data with another entry but preserve storage cache.
	fn overwrite_with(&mut self, other: AccountEntry) {
		self.state = other.state;
		match other.account {
			Some(acc) => {
				if let Some(ref mut ours) = self.account {
					ours.overwrite_with(acc);
				}
			}
			None => self.account = None,
		}
	}
}

/// Check the given proof of execution.
/// `Err(ExecutionError::Internal)` indicates failure, everything else indicates
/// a successful proof (as the transaction itself may be poorly chosen).
// pub fn check_proof(
// 	proof: &[DBValue],
// 	root: H256,
// 	transaction: &SignedTransaction,
// 	machine: &Machine,
// 	env_info: &EnvInfo,
// ) -> ProvedExecution {
// 	let backend = self::backend::ProofCheck::new(proof);
// 	let mut factories = Factories::default();
// 	factories.accountdb = ::account_db::Factory::Plain;
//
// 	let res = State::from_existing(
// 		backend,
// 		root,
// 		machine.account_start_nonce(env_info.number),
// 		factories
// 	);
//
// 	let mut state = match res {
// 		Ok(state) => state,
// 		Err(_) => return ProvedExecution::BadProof,
// 	};
//
// 	let options = TransactOptions::with_no_tracing().save_output_from_contract();
// 	match state.execute(env_info, machine, transaction, options, true) {
// 		Ok(executed) => ProvedExecution::Complete(executed),
// 		Err(ExecutionError::Internal(_)) => ProvedExecution::BadProof,
// 		Err(e) => ProvedExecution::Failed(e),
// 	}
// }

// /// Prove a transaction on the given state.
// /// Returns `None` when the transacion could not be proved,
// /// and a proof otherwise.
// pub fn prove_transaction<H: AsHashDB + Send + Sync>(
// 	db: H,
// 	root: H256,
// 	transaction: &SignedTransaction,
// 	machine: &Machine,
// 	env_info: &EnvInfo,
// 	factories: Factories,
// 	virt: bool,
// ) -> Option<(Bytes, Vec<DBValue>)> {
// 	use self::backend::Proving;
//
// 	let backend = Proving::new(db);
// 	let res = State::from_existing(
// 		backend,
// 		root,
// 		machine.account_start_nonce(env_info.number),
// 		factories,
// 	);
//
// 	let mut state = match res {
// 		Ok(state) => state,
// 		Err(_) => return None,
// 	};
//
// 	let options = TransactOptions::with_no_tracing().dont_check_nonce().save_output_from_contract();
// 	match state.execute(env_info, machine, transaction, options, virt) {
// 		Err(ExecutionError::Internal(_)) => None,
// 		Err(e) => {
// 			trace!(target: "state", "Proved call failed: {}", e);
// 			Some((Vec::new(), state.drop().1.extract_proof()))
// 		}
// 		Ok(res) => Some((res.output, state.drop().1.extract_proof())),
// 	}
// }

/// Representation of the entire state of all accounts in the system.
///
/// `State` can work together with `StateDB` to share account cache.
///
/// Local cache contains changes made locally and changes accumulated
/// locally from previous commits. Global cache reflects the database
/// state and never contains any changes.
///
/// Cache items contains account data, or the flag that account does not exist
/// and modification state (see `AccountState`)
///
/// Account data can be in the following cache states:
/// * In global but not local - something that was queried from the database,
/// but never modified
/// * In local but not global - something that was just added (e.g. new account)
/// * In both with the same value - something that was changed to a new value,
/// but changed back to a previous block in the same block (same State instance)
/// * In both with different values - something that was overwritten with a
/// new value.
///
/// All read-only state queries check local cache/modifications first,
/// then global state cache. If data is not found in any of the caches
/// it is loaded from the DB to the local cache.
///
/// **** IMPORTANT *************************************************************
/// All the modifications to the account data must set the `Dirty` state in the
/// `AccountEntry`. This is done in `require` and `require_or_from`. So just
/// use that.
/// ****************************************************************************
///
/// Upon destruction all the local cache data propagated into the global cache.
/// Propagated items might be rejected if current state is non-canonical.
///
/// State checkpointing.
///
/// A new checkpoint can be created with `checkpoint()`. checkpoints can be
/// created in a hierarchy.
/// When a checkpoint is active all changes are applied directly into
/// `cache` and the original value is copied into an active checkpoint.
/// Reverting a checkpoint with `revert_to_checkpoint` involves copying
/// original values from the latest checkpoint back into `cache`. The code
/// takes care not to overwrite cached storage while doing that.
/// checkpoint can be discarded with `discard_checkpoint`. All of the orignal
/// backed-up values are moved into a parent checkpoint (if any).
///
pub struct State<B: Backend> {
	mkvs: Box<MKVS>,
	db: B,
	cache: RefCell<HashMap<Address, AccountEntry>>,
	// The original account is preserved in
	checkpoints: RefCell<Vec<HashMap<Address, Option<AccountEntry>>>>,
	account_start_nonce: U256,
	factories: Factories,
	// * Option to disable confidentiality entirely (if None).
	// * Box to allow dependency inversion from Parity to the using crate, e.g., Runtime.
	// * Rc to allow two references: `vm::OasisVm` and `State`.
	// * RefCell so that we can allow `vm::OasisVm` to control the ctx (e.g., what
	//	 contract should I be encrypting under?) while the `State` observes control
	//	 updates and encrypts/decrypts storage/logs, as a reult.
	//
	// One alternative to the Rc<RefCell<>> would be to only allow `State` to own the ConfidentialCtx,
	// and `OasisVm` routes all control updates to the ConfidentialCtx owned by `State` through
	// Externalities.
	pub confidential_ctx: Option<Rc<RefCell<Box<ConfidentialCtx>>>>,
}

#[derive(Copy, Clone)]
enum RequireCache {
	None,
	CodeSize,
	Code,
}

/// Mode of dealing with null accounts.
#[derive(PartialEq)]
pub enum CleanupMode<'a> {
	/// Create accounts which would be null.
	ForceCreate,
	/// Don't delete null accounts upon touching, but also don't create them.
	NoEmpty,
	/// Mark all touched accounts.
	TrackTouched(&'a mut HashSet<Address>),
}

/// Provides subset of `State` methods to query state information
pub trait StateInfo {
	/// Get the nonce of account `a`.
	fn nonce(&self, a: &Address) -> trie::Result<U256>;

	/// Get the balance of account `a`.
	fn balance(&self, a: &Address) -> trie::Result<U256>;

	/// Mutate storage of account `address` so that it is `value` for `key`.
	fn storage_at(&self, address: &Address, key: &H256) -> trie::Result<H256>;

	/// Get accounts' code.
	fn code(&self, a: &Address) -> trie::Result<Option<Arc<Bytes>>>;

	/// Get storage expiration timestamp for account 'a'.
	fn storage_expiry(&self, a: &Address) -> trie::Result<u64>;

	/// Mutate storage of account `address` so that it is `value` for `key`.
	fn storage_bytes_at(&self, address: &Address, key: &H256) -> trie::Result<Vec<u8>>;
}

impl<B: Backend> StateInfo for State<B> {
	fn nonce(&self, a: &Address) -> trie::Result<U256> {
		State::nonce(self, a)
	}
	fn balance(&self, a: &Address) -> trie::Result<U256> {
		State::balance(self, a)
	}
	fn storage_at(&self, address: &Address, key: &H256) -> trie::Result<H256> {
		State::storage_at(self, address, key)
	}
	fn code(&self, address: &Address) -> trie::Result<Option<Arc<Bytes>>> {
		State::code(self, address)
	}
	fn storage_expiry(&self, address: &Address) -> trie::Result<u64> {
		State::storage_expiry(self, address)
	}
	fn storage_bytes_at(&self, address: &Address, key: &H256) -> trie::Result<Vec<u8>> {
		State::storage_bytes_at(self, address, key)
	}
}

const SEC_TRIE_DB_UNWRAP_STR: &'static str = "A state can only be created with valid root. Creating a SecTrieDB with a valid root will not fail. \
			 Therefore creating a SecTrieDB with this state's root will not fail.";

impl<B: Backend> State<B> {
	/// Creates new state with empty state root
	/// Used for tests.
	pub fn new(
		mkvs: Box<MKVS>,
		mut db: B,
		account_start_nonce: U256,
		factories: Factories,
	) -> State<B> {
		State {
			mkvs: mkvs,
			db: db,
			cache: RefCell::new(HashMap::new()),
			checkpoints: RefCell::new(Vec::new()),
			account_start_nonce: account_start_nonce,
			factories: factories,
			confidential_ctx: None,
		}
	}

	/// Creates new state with existing state root
	pub fn from_existing(
		mkvs: Box<MKVS>,
		db: B,
		account_start_nonce: U256,
		factories: Factories,
		confidential_ctx: Option<Box<ConfidentialCtx>>,
	) -> Result<State<B>, TrieError> {
		let state = State {
			mkvs: mkvs,
			db: db,
			cache: RefCell::new(HashMap::new()),
			checkpoints: RefCell::new(Vec::new()),
			account_start_nonce: account_start_nonce,
			factories: factories,
			confidential_ctx: confidential_ctx.map(|ctx| Rc::new(RefCell::new(ctx))),
		};

		Ok(state)
	}

	/// Get a VM factory that can execute on this state.
	pub fn vm_factory(&self) -> VmFactory {
		self.factories.vm.clone()
	}

	/// Create a recoverable checkpoint of this state.
	pub fn checkpoint(&mut self) {
		self.checkpoints.get_mut().push(HashMap::new());
	}

	/// Merge last checkpoint with previous.
	pub fn discard_checkpoint(&mut self) {
		// merge with previous checkpoint
		let last = self.checkpoints.get_mut().pop();
		if let Some(mut checkpoint) = last {
			if let Some(ref mut prev) = self.checkpoints.get_mut().last_mut() {
				if prev.is_empty() {
					**prev = checkpoint;
				} else {
					for (k, v) in checkpoint.drain() {
						prev.entry(k).or_insert(v);
					}
				}
			}
		}
	}

	/// Revert to the last checkpoint and discard it.
	pub fn revert_to_checkpoint(&mut self) {
		if let Some(mut checkpoint) = self.checkpoints.get_mut().pop() {
			for (k, v) in checkpoint.drain() {
				match v {
					Some(v) => {
						match self.cache.get_mut().entry(k) {
							Entry::Occupied(mut e) => {
								// Merge checkpointed changes back into the main account
								// storage preserving the cache.
								e.get_mut().overwrite_with(v);
							}
							Entry::Vacant(e) => {
								e.insert(v);
							}
						}
					}
					None => {
						if let Entry::Occupied(e) = self.cache.get_mut().entry(k) {
							if e.get().is_dirty() {
								e.remove();
							}
						}
					}
				}
			}
		}
	}

	fn insert_cache(&self, address: &Address, account: AccountEntry) {
		// Dirty account which is not in the cache means this is a new account.
		// It goes directly into the checkpoint as there's nothing to rever to.
		//
		// In all other cases account is read as clean first, and after that made
		// dirty in and added to the checkpoint with `note_cache`.
		let is_dirty = account.is_dirty();
		let old_value = self.cache.borrow_mut().insert(*address, account);
		if is_dirty {
			if let Some(ref mut checkpoint) = self.checkpoints.borrow_mut().last_mut() {
				checkpoint.entry(*address).or_insert(old_value);
			}
		}
	}

	fn note_cache(&self, address: &Address) {
		if let Some(ref mut checkpoint) = self.checkpoints.borrow_mut().last_mut() {
			checkpoint.entry(*address).or_insert_with(|| {
				self.cache
					.borrow()
					.get(address)
					.map(AccountEntry::clone_dirty)
			});
		}
	}

	/// Destroy the current object and return root and database.
	pub fn drop(mut self) -> (B, Box<MKVS>) {
		self.propagate_to_global_cache();
		(self.db, self.mkvs)
	}

	/// Destroy the current object and return single account data.
	pub fn into_account(
		self,
		account: &Address,
	) -> trie::Result<(Option<Arc<Bytes>>, HashMap<H256, Vec<u8>>)> {
		// TODO: deconstruct without cloning.
		let account = self.require(account, true)?;
		Ok((account.code().clone(), account.storage_changes().clone()))
	}

	/// Create a new contract at address `contract`. If there is already an account at the address
	/// it will have its code reset, ready for `init_code()`.
	pub fn new_contract(
		&mut self,
		contract: &Address,
		balance: U256,
		nonce_offset: U256,
		storage_expiry: u64,
	) {
		self.insert_cache(
			contract,
			AccountEntry::new_dirty(Some(Account::new_contract(
				balance,
				self.account_start_nonce + nonce_offset,
				storage_expiry,
			))),
		);
	}

	/// Remove an existing account.
	pub fn kill_account(&mut self, account: &Address) {
		self.insert_cache(account, AccountEntry::new_dirty(None));
	}

	/// Determine whether an account exists.
	pub fn exists(&self, a: &Address) -> trie::Result<bool> {
		// Bloom filter does not contain empty accounts, so it is important here to
		// check if account exists in the database directly before EIP-161 is in effect.
		self.ensure_cached(a, RequireCache::None, false, |a| a.is_some())
	}

	/// Determine whether an account exists and if not empty.
	pub fn exists_and_not_null(&self, a: &Address) -> trie::Result<bool> {
		self.ensure_cached(a, RequireCache::None, false, |a| {
			a.map_or(false, |a| !a.is_null())
		})
	}

	/// Determine whether an account exists and has code or non-zero nonce.
	pub fn exists_and_has_code_or_nonce(&self, a: &Address) -> trie::Result<bool> {
		self.ensure_cached(a, RequireCache::CodeSize, false, |a| {
			a.map_or(false, |a| {
				a.code_hash() != KECCAK_EMPTY || *a.nonce() != self.account_start_nonce
			})
		})
	}

	/// Get the balance of account `a`.
	pub fn balance(&self, a: &Address) -> trie::Result<U256> {
		self.ensure_cached(a, RequireCache::None, true, |a| {
			a.as_ref()
				.map_or(U256::zero(), |account| *account.balance())
		})
	}

	/// Get the nonce of account `a`.
	pub fn nonce(&self, a: &Address) -> trie::Result<U256> {
		self.ensure_cached(a, RequireCache::None, true, |a| {
			a.as_ref()
				.map_or(self.account_start_nonce, |account| *account.nonce())
		})
	}

	/// Get the storage root of account `a`.
	pub fn storage_root(&self, a: &Address) -> trie::Result<Option<H256>> {
		self.ensure_cached(a, RequireCache::None, true, |a| {
			a.as_ref()
				.and_then(|account| account.storage_root().cloned())
		})
	}

	/// Get the storage expiration timestamp for account `a`.
	pub fn storage_expiry(&self, a: &Address) -> trie::Result<u64> {
		self.ensure_cached(a, RequireCache::None, true, |a| {
			a.as_ref().map_or(0, |account| account.storage_expiry())
		})
	}

	/// Contract storage interface mapping H256 -> H256. If no storage is stored
	/// returns H256::zero(). If bulk storage is accessed, returns an error.
	/// It is assumed bulk storage uses a different keyspace and so such collisions
	/// should never occur.
	pub fn storage_at(&self, address: &Address, key: &H256) -> trie::Result<H256> {
		let storage = self.storage_bytes_at(address, key)?;
		if storage.is_empty() {
			return Ok(H256::zero());
		}
		if storage.len() != 32 {
			error!("Key collision in the patricia trie! Bulk storage should not share a key with H256 storage.");
			return Err(Box::new(trie::TrieError::DecoderError(
				rlp::DecoderError::RlpIsTooBig,
			)));
		}
		Ok(H256::from(storage.as_slice()))
	}

	/// Contract storage interface. The underlying storage may or may not be encrypted.
	/// As a result, we pre-process the key, encrypting it if we're in a
	/// confidential context, and we post-process the value by decrypting it.
	pub fn storage_bytes_at(&self, address: &Address, key: &H256) -> trie::Result<Vec<u8>> {
		let key = self.to_storage_key(key);
		let value = self._storage_at(address, &key)?;
		Ok(self.from_storage_value(value))
	}

	/// Mutate storage of account `address` so that it is `value` for `key`.
	/// Returns None if there is no storage located at the given address for the given key.
	pub fn _storage_at(&self, address: &Address, key: &H256) -> trie::Result<Option<Vec<u8>>> {
		// Storage key search and update works like this:
		// 1. If there's an entry for the account in the local cache check for the key and return it if found.
		// 2. If there's an entry for the account in the global cache check for the key or load it into that account.
		// 3. If account is missing in the global cache load it into the local cache and cache the key there.

		{
			// check local cache first without updating
			let local_cache = self.cache.borrow_mut();
			let mut local_account = None;
			if let Some(maybe_acc) = local_cache.get(address) {
				match maybe_acc.account {
					Some(ref account) => {
						if let Some(value) = account.cached_storage_at(&key) {
							return Ok(Some(value));
						} else {
							local_account = Some(maybe_acc);
						}
					}
					_ => return Ok(None),
				}
			}
			// check the global cache and and cache storage key there if found,
			let trie_res = self.db.get_cached(address, |acc| match acc {
				None => Ok(None),
				Some(a) => {
					let account_mkvs = ReadOnlyPrefixedMKVS::new(&self.mkvs, address);
					Ok(a.storage_at(&account_mkvs, &key))
				}
			});

			if let Some(res) = trie_res {
				return res;
			}

			// otherwise cache the account localy and cache storage key there.
			if let Some(ref mut acc) = local_account {
				if let Some(ref account) = acc.account {
					let account_mkvs = ReadOnlyPrefixedMKVS::new(&self.mkvs, address);
					return Ok(account.storage_at(&account_mkvs, &key));
				} else {
					return Ok(None);
				}
			}
		}

		// account is not found in the global cache, get from the DB and insert into local
		let from_rlp = |b: &[u8]| Account::from_rlp(b).expect("decoding db value failed");
		let maybe_acc = self.mkvs.get(&address).map(|value| from_rlp(&value));
		let r = maybe_acc.as_ref().map_or(Ok(None), |a| {
			let account_mkvs = ReadOnlyPrefixedMKVS::new(&self.mkvs, address);
			Ok(a.storage_at(&account_mkvs, &key))
		});
		self.insert_cache(address, AccountEntry::new_clean(maybe_acc));
		r
	}

	/// Get accounts' code.
	pub fn code(&self, a: &Address) -> trie::Result<Option<Arc<Bytes>>> {
		self.ensure_cached(a, RequireCache::Code, true, |a| {
			a.as_ref().map_or(None, |a| a.code().clone())
		})
	}

	/// Get an account's code hash.
	pub fn code_hash(&self, a: &Address) -> trie::Result<H256> {
		self.ensure_cached(a, RequireCache::None, true, |a| {
			a.as_ref().map_or(KECCAK_EMPTY, |a| a.code_hash())
		})
	}

	/// Get accounts' code size.
	pub fn code_size(&self, a: &Address) -> trie::Result<Option<usize>> {
		self.ensure_cached(a, RequireCache::CodeSize, true, |a| {
			a.as_ref().and_then(|a| a.code_size())
		})
	}

	/// Add `incr` to the balance of account `a`.
	pub fn add_balance(
		&mut self,
		a: &Address,
		incr: &U256,
		cleanup_mode: CleanupMode,
	) -> trie::Result<()> {
		trace!(target: "state", "add_balance({}, {}): {}", a, incr, self.balance(a)?);
		let is_value_transfer = !incr.is_zero();
		if is_value_transfer || (cleanup_mode == CleanupMode::ForceCreate && !self.exists(a)?) {
			self.require(a, false)?.add_balance(incr);
		} else if let CleanupMode::TrackTouched(set) = cleanup_mode {
			if self.exists(a)? {
				set.insert(*a);
				self.touch(a)?;
			}
		}
		Ok(())
	}

	/// Subtract `decr` from the balance of account `a`.
	pub fn sub_balance(
		&mut self,
		a: &Address,
		decr: &U256,
		cleanup_mode: &mut CleanupMode,
	) -> trie::Result<()> {
		trace!(target: "state", "sub_balance({}, {}): {}", a, decr, self.balance(a)?);
		if !decr.is_zero() || !self.exists(a)? {
			self.require(a, false)?.sub_balance(decr);
		}
		if let CleanupMode::TrackTouched(ref mut set) = *cleanup_mode {
			set.insert(*a);
		}
		Ok(())
	}

	/// Subtracts `by` from the balance of `from` and adds it to that of `to`.
	pub fn transfer_balance(
		&mut self,
		from: &Address,
		to: &Address,
		by: &U256,
		mut cleanup_mode: CleanupMode,
	) -> trie::Result<()> {
		self.sub_balance(from, by, &mut cleanup_mode)?;
		self.add_balance(to, by, cleanup_mode)?;
		Ok(())
	}

	/// Increment the nonce of account `a` by 1.
	pub fn inc_nonce(&mut self, a: &Address) -> trie::Result<()> {
		self.require(a, false).map(|mut x| x.inc_nonce())
	}

	/// Analogous to storage_at, encrypts the key, value, if needed, before inserting into
	/// the backing account storage.
	pub fn set_storage(&mut self, a: &Address, key: H256, value: H256) -> trie::Result<()> {
		self.set_storage_bytes(a, key, value.to_vec())
	}

	/// Sets the given key value pair directly into the contract's storage trie. Encrypts
	/// the value if in a confidential ctx.
	pub fn set_storage_bytes(
		&mut self,
		a: &Address,
		key: H256,
		value: Vec<u8>,
	) -> trie::Result<()> {
		trace!(target: "state", "set_storage({}:{:x} to {:?})", a, key, value);
		let key = self.to_storage_key(&key);
		let value = self.to_storage_value(value);
		self._set_storage(a, key, value)
	}

	/// Mutate storage of account `a` so that it is `value` for `key`.
	fn _set_storage(&mut self, a: &Address, key: H256, value: Vec<u8>) -> trie::Result<()> {
		let current_storage = self._storage_at(a, &key)?;
		if current_storage.is_none() || current_storage.unwrap() != value {
			self.require(a, false)?.set_storage(key, value)
		}

		Ok(())
	}

	/// Initialise the code of account `a` so that it is `code`.
	/// NOTE: Account should have been created with `new_contract`.
	pub fn init_code(&mut self, a: &Address, code: Bytes) -> trie::Result<()> {
		self.require_or_from(
			a,
			true,
			|| Account::new_contract(0.into(), self.account_start_nonce, 0),
			|_| {},
		)?
		.init_code(code);
		Ok(())
	}

	/// Reset the code of account `a` so that it is `code`.
	pub fn reset_code(&mut self, a: &Address, code: Bytes) -> trie::Result<()> {
		self.require_or_from(
			a,
			true,
			|| Account::new_contract(0.into(), self.account_start_nonce, 0),
			|_| {},
		)?
		.reset_code(code);
		Ok(())
	}

	/// Execute a given transaction, producing a receipt and an optional trace.
	/// This will change the state accordingly.
	pub fn apply(
		&mut self,
		env_info: &EnvInfo,
		machine: &Machine,
		t: &SignedTransaction,
		tracing: bool,
		should_return_value: bool,
	) -> ApplyResult<FlatTrace, VMTrace> {
		if tracing {
			let options = TransactOptions::with_tracing();
			self.apply_with_tracing(
				env_info,
				machine,
				t,
				options.tracer,
				options.vm_tracer,
				options.ext_tracer,
				should_return_value,
			)
		} else {
			let options = TransactOptions::with_no_tracing();
			self.apply_with_tracing(
				env_info,
				machine,
				t,
				options.tracer,
				options.vm_tracer,
				options.ext_tracer,
				should_return_value,
			)
		}
	}

	fn get_options<T, V, X>(
		tracer: T,
		vm_tracer: V,
		ext_tracer: X,
		benchmarking: bool,
		should_return_value: bool,
	) -> TransactOptions<T, V, X> {
		let mut options = match benchmarking {
			true => TransactOptions::new(tracer, vm_tracer, ext_tracer).dont_check_nonce(),
			false => TransactOptions::new(tracer, vm_tracer, ext_tracer),
		};

		let options = if should_return_value {
			options.save_output_from_contract()
		} else {
			options
		};

		return options;
	}

	/// Execute a given transaction with given tracer and VM tracer producing a receipt and an optional trace.
	/// This will change the state accordingly.
	pub fn apply_with_tracing<V, T, X>(
		&mut self,
		env_info: &EnvInfo,
		machine: &Machine,
		t: &SignedTransaction,
		tracer: T,
		vm_tracer: V,
		ext_tracer: X,
		should_return_value: bool,
	) -> ApplyResult<T::Output, V::Output>
	where
		T: trace::Tracer,
		V: trace::VMTracer,
		X: ExtTracer,
	{
		let options = Self::get_options(
			tracer,
			vm_tracer,
			ext_tracer,
			machine.params().benchmarking,
			should_return_value,
		);

		let e = self.execute(env_info, machine, t, options, false)?;
		let params = machine.params();

		let eip658 = env_info.number >= params.eip658_transition;
		let no_intermediate_commits = eip658
			|| (env_info.number >= params.eip98_transition
				&& env_info.number >= params.validate_receipts_transition);

		let result = if e.exception.is_some() { 0 } else { 1 };
		info!(target: "state", "call_type: TransactionExecuted, \
			sender: {:?}, transaction_hash: {:?}, success: {:?}, error: {:?}",
			t.sender(), &t.hash(), e.exception.is_none(), e.exception);

		let outcome = if no_intermediate_commits {
			if eip658 {
				TransactionOutcome::StatusCode(result)
			} else {
				TransactionOutcome::Unknown
			}
		} else {
			TransactionOutcome::Unknown
		};

		let output = e.output;
		let receipt = Receipt::new(outcome, e.cumulative_gas_used, e.logs);
		trace!(target: "state", "Transaction receipt: {:?}", receipt);

		Ok(ApplyOutcome {
			receipt,
			output,
			trace: e.trace,
			vm_trace: e.vm_trace,
		})
	}

	// Execute a given transaction without committing changes.
	//
	// `virt` signals that we are executing outside of a block set and restrictions like
	// gas limits and gas costs should be lifted.
	fn execute<T, V, X>(
		&mut self,
		env_info: &EnvInfo,
		machine: &Machine,
		t: &SignedTransaction,
		options: TransactOptions<T, V, X>,
		virt: bool,
	) -> Result<Executed<T::Output, V::Output>, ExecutionError>
	where
		T: trace::Tracer,
		V: trace::VMTracer,
		X: ExtTracer,
	{
		let mut e = Executive::new(self, env_info, machine);

		match virt {
			true => e.transact_virtual(t, options),
			false => e.transact(t, options),
		}
	}

	fn touch(&mut self, a: &Address) -> trie::Result<()> {
		self.require(a, false)?;
		Ok(())
	}

	pub fn commit(&mut self) -> Result<(), Error> {
		// first, commit the sub trees.
		let mut accounts = self.cache.borrow_mut();
		for (address, ref mut a) in accounts.iter_mut().filter(|&(_, ref a)| a.is_dirty()) {
			if let Some(ref mut account) = a.account {
				{
					let mut account_mkvs = PrefixedMKVS::new(&mut self.mkvs, address);
					account.commit_storage(&mut account_mkvs);
					account.commit_code(&mut account_mkvs);
				}
			}
		}

		{
			for (address, ref mut a) in accounts.iter_mut().filter(|&(_, ref a)| a.is_dirty()) {
				a.state = AccountState::Committed;
				match a.account {
					Some(ref mut account) => {
						self.mkvs.insert(address.as_ref(), &account.rlp());
					}
					None => {
						self.mkvs.remove(address);
					}
				};
			}
		}

		Ok(())
	}

	/// Propagate local cache into shared canonical state cache.
	fn propagate_to_global_cache(&mut self) {
		let mut addresses = self.cache.borrow_mut();
		trace!("Committing cache {:?} entries", addresses.len());
		for (address, a) in addresses.drain().filter(|&(_, ref a)| {
			a.state == AccountState::Committed || a.state == AccountState::CleanFresh
		}) {
			self.db
				.add_to_account_cache(address, a.account, a.state == AccountState::Committed);
		}
	}

	/// Clear state cache
	pub fn clear(&mut self) {
		self.cache.borrow_mut().clear();
	}

	/// Remove any touched empty or dust accounts.
	pub fn kill_garbage(
		&mut self,
		touched: &HashSet<Address>,
		remove_empty_touched: bool,
		min_balance: &Option<U256>,
		kill_contracts: bool,
	) -> trie::Result<()> {
		let to_kill: HashSet<_> = {
			self.cache.borrow().iter().filter_map(|(address, ref entry)|
			if touched.contains(address) && // Check all touched accounts
				((remove_empty_touched && entry.exists_and_is_null()) // Remove all empty touched accounts.
				|| min_balance.map_or(false, |ref balance| entry.account.as_ref().map_or(false, |account|
					(account.is_basic() || kill_contracts) // Remove all basic and optionally contract accounts where balance has been decreased.
					&& account.balance() < balance && entry.old_balance.as_ref().map_or(false, |b| account.balance() < b)))) {

				Some(address.clone())
			} else { None }).collect()
		};
		for address in to_kill {
			self.kill_account(&address);
		}
		Ok(())
	}

	/// Populate the state from `accounts`.
	/// Used for tests.
	pub fn populate_from(&mut self, accounts: PodState) {
		assert!(self.checkpoints.borrow().is_empty());
		for (add, acc) in accounts.drain().into_iter() {
			self.cache
				.borrow_mut()
				.insert(add, AccountEntry::new_dirty(Some(Account::from_pod(acc))));
		}
	}

	/// Populate a PodAccount map from this state.
	pub fn to_pod(&self) -> PodState {
		assert!(self.checkpoints.borrow().is_empty());
		// TODO: handle database rather than just the cache.
		// will need fat db.
		PodState::from(
			self.cache
				.borrow()
				.iter()
				.fold(BTreeMap::new(), |mut m, (add, opt)| {
					if let Some(ref acc) = opt.account {
						m.insert(add.clone(), PodAccount::from_account(acc));
					}
					m
				}),
		)
	}

	/// Populate a PodAccount map from this state, with another state as the account and storage query.
	pub fn to_pod_diff<X: Backend>(&mut self, query: &State<X>) -> trie::Result<PodState> {
		assert!(self.checkpoints.borrow().is_empty());

		// Merge PodAccount::to_pod for cache of self and `query`.
		let all_addresses = self
			.cache
			.borrow()
			.keys()
			.cloned()
			.chain(query.cache.borrow().keys().cloned())
			.collect::<BTreeSet<_>>();

		Ok(PodState::from(all_addresses.into_iter().fold(
			Ok(BTreeMap::new()),
			|m: trie::Result<_>, address| {
				let mut m = m?;

				let account = self.ensure_cached(&address, RequireCache::Code, true, |acc| {
					acc.map(|acc| {
						// Merge all modified storage keys.
						let all_keys = {
							let self_keys = acc
								.storage_changes()
								.keys()
								.cloned()
								.collect::<BTreeSet<_>>();

							if let Some(ref query_storage) =
								query.cache.borrow().get(&address).and_then(|opt| {
									Some(
										opt.account
											.as_ref()?
											.storage_changes()
											.keys()
											.cloned()
											.collect::<BTreeSet<_>>(),
									)
								}) {
								self_keys.union(&query_storage).cloned().collect::<Vec<_>>()
							} else {
								self_keys.into_iter().collect::<Vec<_>>()
							}
						};

						// Storage must be fetched after ensure_cached to avoid borrow problem.
						(
							*acc.balance(),
							*acc.nonce(),
							all_keys,
							acc.code().map(|x| x.to_vec()),
							acc.storage_expiry(),
						)
					})
				})?;

				if let Some((balance, nonce, storage_keys, code, storage_expiry)) = account {
					let storage = storage_keys.into_iter().fold(
						Ok(BTreeMap::new()),
						|s: trie::Result<_>, key| {
							let mut s = s?;

							s.insert(
								key,
								self._storage_at(&address, &key)?
									.unwrap_or(H256::zero().to_vec()),
							);
							Ok(s)
						},
					)?;

					m.insert(
						address,
						PodAccount {
							balance,
							nonce,
							storage,
							code,
							storage_expiry,
						},
					);
				}

				Ok(m)
			},
		)?))
	}

	/// Returns a `StateDiff` describing the difference from `orig` to `self`.
	/// Consumes self.
	pub fn diff_from<X: Backend>(&self, mut orig: State<X>) -> trie::Result<StateDiff> {
		let pod_state_post = self.to_pod();
		let pod_state_pre = orig.to_pod_diff(self)?;
		Ok(pod_state::diff_pod(&pod_state_pre, &pod_state_post))
	}

	// load required account data from the databases.
	fn update_account_cache(
		require: RequireCache,
		account: &mut Account,
		state_db: &B,
		mkvs: &MKVS,
	) {
		if let RequireCache::None = require {
			return;
		}

		if account.is_cached() {
			return;
		}

		// if there's already code in the global cache, always cache it localy
		let hash = account.code_hash();
		match state_db.get_cached_code(&hash) {
			Some(code) => account.cache_given_code(code),
			None => match require {
				RequireCache::None => {}
				RequireCache::Code => {
					if let Some(code) = account.cache_code(mkvs) {
						// propagate code loaded from the database to
						// the global code cache.
						state_db.cache_code(hash, code)
					}
				}
				RequireCache::CodeSize => {
					account.cache_code_size(mkvs);
				}
			},
		}
	}

	/// Check caches for required data
	/// First searches for account in the local, then the shared cache.
	/// Populates local cache if nothing found.
	fn ensure_cached<F, U>(
		&self,
		a: &Address,
		require: RequireCache,
		_check_null: bool,
		f: F,
	) -> trie::Result<U>
	where
		F: Fn(Option<&Account>) -> U,
	{
		// check local cache first
		if let Some(ref mut maybe_acc) = self.cache.borrow_mut().get_mut(a) {
			if let Some(ref mut account) = maybe_acc.account {
				let account_mkvs = ReadOnlyPrefixedMKVS::new(&self.mkvs, a);
				Self::update_account_cache(require, account, &self.db, &account_mkvs);
				return Ok(f(Some(account)));
			}
			return Ok(f(None));
		}
		// check global cache
		let result = self.db.get_cached(a, |mut acc| {
			if let Some(ref mut account) = acc {
				let account_mkvs = ReadOnlyPrefixedMKVS::new(&self.mkvs, a);
				Self::update_account_cache(require, account, &self.db, &account_mkvs);
			}
			f(acc.map(|a| &*a))
		});
		match result {
			Some(r) => Ok(r),
			None => {
				// not found in the global cache, get from the DB and insert into local
				let from_rlp = |b: &[u8]| Account::from_rlp(b).expect("decoding db value failed");
				let mut maybe_acc = self.mkvs.get(&a).map(|value| from_rlp(&value));
				if let Some(ref mut account) = maybe_acc.as_mut() {
					let account_mkvs = ReadOnlyPrefixedMKVS::new(&self.mkvs, a);
					Self::update_account_cache(require, account, &self.db, &account_mkvs);
				}
				let r = f(maybe_acc.as_ref());
				self.insert_cache(a, AccountEntry::new_clean(maybe_acc));
				Ok(r)
			}
		}
	}

	/// Pull account `a` in our cache from the trie DB. `require_code` requires that the code be cached, too.
	fn require<'a>(&'a self, a: &Address, require_code: bool) -> trie::Result<RefMut<'a, Account>> {
		self.require_or_from(
			a,
			require_code,
			|| Account::new_basic(0u8.into(), self.account_start_nonce),
			|_| {},
		)
	}

	/// Pull account `a` in our cache from the trie DB. `require_code` requires that the code be cached, too.
	/// If it doesn't exist, make account equal the evaluation of `default`.
	fn require_or_from<'a, F, G>(
		&'a self,
		a: &Address,
		require_code: bool,
		default: F,
		not_default: G,
	) -> trie::Result<RefMut<'a, Account>>
	where
		F: FnOnce() -> Account,
		G: FnOnce(&mut Account),
	{
		let contains_key = self.cache.borrow().contains_key(a);
		if !contains_key {
			match self.db.get_cached_account(a) {
				Some(acc) => self.insert_cache(a, AccountEntry::new_clean_cached(acc)),
				None => {
					let from_rlp =
						|b: &[u8]| Account::from_rlp(b).expect("decoding db value failed");
					let maybe_acc = self.mkvs.get(&a).map(|value| from_rlp(&value));
					let maybe_acc = AccountEntry::new_clean(maybe_acc);
					self.insert_cache(a, maybe_acc);
				}
			}
		}
		self.note_cache(a);

		// at this point the entry is guaranteed to be in the cache.
		Ok(RefMut::map(self.cache.borrow_mut(), |c| {
			let entry = c
				.get_mut(a)
				.expect("entry known to exist in the cache; qed");

			match &mut entry.account {
				&mut Some(ref mut acc) => not_default(acc),
				slot => *slot = Some(default()),
			}

			// set the dirty flag after changing account data.
			entry.state = AccountState::Dirty;
			match entry.account {
				Some(ref mut account) => {
					if require_code {
						let account_mkvs = ReadOnlyPrefixedMKVS::new(&self.mkvs, a);
						Self::update_account_cache(
							RequireCache::Code,
							account,
							&self.db,
							&account_mkvs,
						);
					}
					account
				}
				_ => panic!("Required account must always exist; qed"),
			}
		}))
	}

	/// Replace account code and storage. Creates account if it does not exist.
	pub fn patch_account(
		&self,
		a: &Address,
		code: Arc<Bytes>,
		storage: HashMap<H256, Vec<u8>>,
	) -> trie::Result<()> {
		Ok(self
			.require(a, false)?
			.reset_code_and_storage(code, storage))
	}

	/// Returns the Oasis contract associated with this transaction's code (or None, if header not present).
	pub fn oasis_contract(
		&self,
		transaction: &SignedTransaction,
	) -> Result<Option<OasisContract>, String> {
		let code = self.tx_code(transaction)?;
		code.as_ref()
			.map_or(Ok(None), |c| OasisContract::from_code(c.as_slice()))
	}

	pub fn is_confidential_contract(&self, address: &Address) -> Result<bool, String> {
		let code = self.code(address).map_err(|err| err.to_string())?;
		let contract = {
			if let Some(ref code) = code.clone() {
				OasisContract::from_code(code)?
			} else {
				None
			}
		};

		Ok(contract.as_ref().map_or(false, |c| c.confidential))
	}

	pub fn is_encrypting(&self) -> bool {
		self.confidential_ctx.is_some()
			&& self
				.confidential_ctx
				.as_ref()
				.unwrap()
				.borrow()
				.is_encrypting()
	}

	/// Returns the code that will be executed as a result of this transaction. For a create this
	/// is the init code in the transaction's data field. For a call, this is the stored contract
	/// code.
	fn tx_code(&self, transaction: &SignedTransaction) -> Result<Option<Vec<u8>>, String> {
		Ok(Some(match transaction.action {
			transaction::Action::Create => transaction.data.clone(),
			transaction::Action::Call(to_addr) => {
				let mut code = self
					.code(&to_addr)
					.map_err(|_| format!("Failed to get code at address {:?}", to_addr))?;
				if code.is_none() {
					return Ok(None);
				}
				code.unwrap().to_vec()
			}
		}))
	}

	/// Returns the given key in a format that is suitable for storage.
	/// If a confidential context is open, then encrypts the key and hashes it.
	/// Otherwise returns the key as given.
	pub fn to_storage_key(&self, key: &H256) -> H256 {
		if self.is_encrypting() {
			let enc_key = self
				.confidential_ctx
				.as_ref()
				.expect("Cannot encrypt without a confidential context")
				.borrow()
				.encrypt_storage_key(key.to_vec())
				.expect("Should be able to encrypt storage keys");
			keccak(&enc_key)
		} else {
			key.clone()
		}
	}

	/// Returns the given value in a format that is suitable for storage.
	/// If a confidential context is open, then encrypts the value. Otherwise
	/// returns the given value as a Vec.
	fn to_storage_value(&self, value: Vec<u8>) -> Vec<u8> {
		if self.is_encrypting() {
			self.confidential_ctx
				.as_ref()
				.expect("Cannot encrypt without a confidential context")
				.borrow_mut()
				.encrypt_storage_value(value)
				.expect("Should be able to encrypt storage")
		} else {
			value
		}
	}

	/// Transforms the given value--from storage--into its plaintext representation.
	/// If a confidential context is open, then decrypts the value, otherwise returns
	/// the value as given.
	fn from_storage_value(&self, value: Option<Vec<u8>>) -> Vec<u8> {
		if value.is_none() {
			return vec![];
		}
		let value = value.unwrap();
		if self.is_encrypting() {
			self.confidential_ctx
				.as_ref()
				.expect("Cannot decrypt without a confidential context")
				.borrow()
				.decrypt_storage_value(value)
				.expect("Corrupted state")
		} else {
			value
		}
	}
}

impl<B: Backend> fmt::Debug for State<B> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{:?}", self.cache.borrow())
	}
}

// TODO: cloning for `State` shouldn't be possible in general; Remove this and use
// checkpoints where possible.
impl Clone for State<StateDB> {
	fn clone(&self) -> State<StateDB> {
		let cache = {
			let mut cache: HashMap<Address, AccountEntry> = HashMap::new();
			for (key, val) in self.cache.borrow().iter() {
				if let Some(entry) = val.clone_if_dirty() {
					cache.insert(key.clone(), entry);
				}
			}
			cache
		};

		State {
			mkvs: self.mkvs.boxed_clone(),
			db: self.db.boxed_clone(),
			cache: RefCell::new(cache),
			checkpoints: RefCell::new(Vec::new()),
			account_start_nonce: self.account_start_nonce.clone(),
			factories: self.factories.clone(),
			confidential_ctx: None,
		}
	}
}

impl<B: Backend + Clone> Clone for State<B> {
	fn clone(&self) -> State<B> {
		let cache = {
			let mut cache: HashMap<Address, AccountEntry> = HashMap::new();
			for (key, val) in self.cache.borrow().iter() {
				if let Some(entry) = val.clone_if_dirty() {
					cache.insert(key.clone(), entry);
				}
			}
			cache
		};
		State {
			mkvs: self.mkvs.boxed_clone(),
			db: self.db.clone(),
			cache: RefCell::new(cache),
			checkpoints: RefCell::new(Vec::new()),
			account_start_nonce: self.account_start_nonce.clone(),
			factories: self.factories.clone(),
			confidential_ctx: None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use ethereum_types::{Address, H256, U256};
	use ethkey::Secret;
	use hash::keccak;
	use machine::EthereumMachine;
	use rustc_hex::FromHex;
	use spec::*;
	use state::State;
	use state_db::StateDB;
	use std::str::FromStr;
	use std::sync::Arc;
	use test_helpers::{get_temp_state, get_temp_state_db};
	use transaction::*;
	use vm::EnvInfo;
	// use ethcore_logger::init_log;
	use crate::mkvs::MemoryMKVS;
	use evm::CallType;
	use trace::{trace, FlatTrace, TraceError};

	// TODO: do we need to log?
	fn init_log() {}

	fn secret() -> Secret {
		keccak("").into()
	}

	fn make_frontier_machine(max_depth: usize) -> EthereumMachine {
		let mut machine = ::ethereum::new_frontier_test_machine();
		machine.set_schedule_creation_rules(Box::new(move |s, _| s.max_depth = max_depth));
		machine
	}

	#[test]
	fn should_apply_create_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Create,
			value: 100.into(),
			data: FromHex::from_hex("601080600c6000396000f3006000355415600957005b60203560003555")
				.unwrap(),
		}
		.sign(&secret(), None);

		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			subtraces: 0,
			action: trace::Action::Create(trace::Create {
				from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
				value: 100.into(),
				gas: 78603.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
				init: vec![
					96, 16, 128, 96, 12, 96, 0, 57, 96, 0, 243, 0, 96, 0, 53, 84, 21, 96, 9, 87, 0,
					91, 96, 32, 53, 96, 0, 53, 85,
				],
			}),
			result: trace::Res::Create(trace::CreateResult {
				gas_used: U256::from(40),
				address: Address::from_str("8988167e088c87cd314df6d3c2b83da5acb93ace").unwrap(),
				code: vec![96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53],
			}),
		}];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_work_when_cloned() {
		init_log();

		let a = Address::zero();

		let mut state = {
			let mut state = get_temp_state();
			assert_eq!(state.exists(&a).unwrap(), false);
			state.inc_nonce(&a).unwrap();
			state.commit().unwrap();
			state.clone()
		};

		state.inc_nonce(&a).unwrap();
		state.commit().unwrap();
	}

	#[test]
	fn should_trace_failed_create_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Create,
			value: 100.into(),
			data: FromHex::from_hex("5b600056").unwrap(),
		}
		.sign(&secret(), None);

		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			action: trace::Action::Create(trace::Create {
				from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
				value: 100.into(),
				gas: 78948.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
				init: vec![91, 96, 0, 86],
			}),
			result: trace::Res::FailedCreate(TraceError::OutOfGas),
			subtraces: 0,
		}];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_call_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.init_code(&0xa.into(), FromHex::from_hex("6000").unwrap())
			.unwrap();
		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			action: trace::Action::Call(trace::Call {
				from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
				to: 0xa.into(),
				value: 100.into(),
				gas: 79000.into(),
				input: vec![],
				call_type: CallType::Call,
			}),
			result: trace::Res::Call(trace::CallResult {
				gas_used: U256::from(3),
				output: vec![],
			}),
			subtraces: 0,
		}];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_basic_call_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			action: trace::Action::Call(trace::Call {
				from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
				to: 0xa.into(),
				value: 100.into(),
				gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
				input: vec![],
				call_type: CallType::Call,
			}),
			result: trace::Res::Call(trace::CallResult {
				gas_used: U256::from(0),
				output: vec![],
			}),
			subtraces: 0,
		}];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_call_transaction_to_builtin() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = Spec::new_test_machine();

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0x1.into()),
			value: 0.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		let result = state.apply(&info, &machine, &t, true, false).unwrap();

		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			action: trace::Action::Call(trace::Call {
				from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
				to: "0000000000000000000000000000000000000001".into(),
				value: 0.into(),
				gas: 97_900.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
				input: vec![],
				call_type: CallType::Call,
			}),
			result: trace::Res::Call(trace::CallResult {
				gas_used: U256::from(3000),
				output: vec![],
			}),
			subtraces: 0,
		}];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_not_trace_subcall_transaction_to_builtin() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = Spec::new_test_machine();

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 0.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.init_code(
				&0xa.into(),
				FromHex::from_hex("600060006000600060006001610be0f1").unwrap(),
			)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();

		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			action: trace::Action::Call(trace::Call {
				from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
				to: 0xa.into(),
				value: 0.into(),
				gas: 97900.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
				input: vec![],
				call_type: CallType::Call,
			}),
			result: trace::Res::Call(trace::CallResult {
				gas_used: U256::from(3_721), // in post-eip150
				output: vec![],
			}),
			subtraces: 0,
		}];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_failed_call_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.init_code(&0xa.into(), FromHex::from_hex("5b600056").unwrap())
			.unwrap();
		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			action: trace::Action::Call(trace::Call {
				from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
				to: 0xa.into(),
				value: 100.into(),
				gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
				input: vec![],
				call_type: CallType::Call,
			}),
			result: trace::Res::FailedCall(TraceError::OutOfGas),
			subtraces: 0,
		}];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_call_with_subcall_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.init_code(
				&0xa.into(),
				FromHex::from_hex("60006000600060006000600b602b5a03f1").unwrap(),
			)
			.unwrap();
		state
			.init_code(&0xb.into(), FromHex::from_hex("6000").unwrap())
			.unwrap();
		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();

		let expected_trace = vec![
			FlatTrace {
				trace_address: Default::default(),
				subtraces: 1,
				action: trace::Action::Call(trace::Call {
					from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
					to: 0xa.into(),
					value: 100.into(),
					gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(69),
					output: vec![],
				}),
			},
			FlatTrace {
				trace_address: vec![0].into_iter().collect(),
				subtraces: 0,
				action: trace::Action::Call(trace::Call {
					from: 0xa.into(),
					to: 0xb.into(),
					value: 0.into(),
					gas: 78934.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(3),
					output: vec![],
				}),
			},
		];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_call_with_basic_subcall_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.init_code(
				&0xa.into(),
				FromHex::from_hex("60006000600060006045600b6000f1").unwrap(),
			)
			.unwrap();
		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![
			FlatTrace {
				trace_address: Default::default(),
				subtraces: 1,
				action: trace::Action::Call(trace::Call {
					from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
					to: 0xa.into(),
					value: 100.into(),
					gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(31761),
					output: vec![],
				}),
			},
			FlatTrace {
				trace_address: vec![0].into_iter().collect(),
				subtraces: 0,
				action: trace::Action::Call(trace::Call {
					from: 0xa.into(),
					to: 0xb.into(),
					value: 69.into(),
					gas: 2300.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult::default()),
			},
		];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_not_trace_call_with_invalid_basic_subcall_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.init_code(
				&0xa.into(),
				FromHex::from_hex("600060006000600060ff600b6000f1").unwrap(),
			)
			.unwrap(); // not enough funds.
		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			subtraces: 0,
			action: trace::Action::Call(trace::Call {
				from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
				to: 0xa.into(),
				value: 100.into(),
				gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
				input: vec![],
				call_type: CallType::Call,
			}),
			result: trace::Res::Call(trace::CallResult {
				gas_used: U256::from(31761),
				output: vec![],
			}),
		}];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_failed_subcall_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![], //600480600b6000396000f35b600056
		}
		.sign(&secret(), None);

		state
			.init_code(
				&0xa.into(),
				FromHex::from_hex("60006000600060006000600b602b5a03f1").unwrap(),
			)
			.unwrap();
		state
			.init_code(&0xb.into(), FromHex::from_hex("5b600056").unwrap())
			.unwrap();
		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![
			FlatTrace {
				trace_address: Default::default(),
				subtraces: 1,
				action: trace::Action::Call(trace::Call {
					from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
					to: 0xa.into(),
					value: 100.into(),
					gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(79_000),
					output: vec![],
				}),
			},
			FlatTrace {
				trace_address: vec![0].into_iter().collect(),
				subtraces: 0,
				action: trace::Action::Call(trace::Call {
					from: 0xa.into(),
					to: 0xb.into(),
					value: 0.into(),
					gas: 78934.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::FailedCall(TraceError::OutOfGas),
			},
		];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_call_with_subcall_with_subcall_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.init_code(
				&0xa.into(),
				FromHex::from_hex("60006000600060006000600b602b5a03f1").unwrap(),
			)
			.unwrap();
		state
			.init_code(
				&0xb.into(),
				FromHex::from_hex("60006000600060006000600c602b5a03f1").unwrap(),
			)
			.unwrap();
		state
			.init_code(&0xc.into(), FromHex::from_hex("6000").unwrap())
			.unwrap();
		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![
			FlatTrace {
				trace_address: Default::default(),
				subtraces: 1,
				action: trace::Action::Call(trace::Call {
					from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
					to: 0xa.into(),
					value: 100.into(),
					gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(135),
					output: vec![],
				}),
			},
			FlatTrace {
				trace_address: vec![0].into_iter().collect(),
				subtraces: 1,
				action: trace::Action::Call(trace::Call {
					from: 0xa.into(),
					to: 0xb.into(),
					value: 0.into(),
					gas: 78934.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(69),
					output: vec![],
				}),
			},
			FlatTrace {
				trace_address: vec![0, 0].into_iter().collect(),
				subtraces: 0,
				action: trace::Action::Call(trace::Call {
					from: 0xb.into(),
					to: 0xc.into(),
					value: 0.into(),
					gas: 78868.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(3),
					output: vec![],
				}),
			},
		];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_failed_subcall_with_subcall_transaction() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![], //600480600b6000396000f35b600056
		}
		.sign(&secret(), None);

		state
			.init_code(
				&0xa.into(),
				FromHex::from_hex("60006000600060006000600b602b5a03f1").unwrap(),
			)
			.unwrap();
		state
			.init_code(
				&0xb.into(),
				FromHex::from_hex("60006000600060006000600c602b5a03f1505b601256").unwrap(),
			)
			.unwrap();
		state
			.init_code(&0xc.into(), FromHex::from_hex("6000").unwrap())
			.unwrap();
		state
			.add_balance(&t.sender(), &(100.into()), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();

		let expected_trace = vec![
			FlatTrace {
				trace_address: Default::default(),
				subtraces: 1,
				action: trace::Action::Call(trace::Call {
					from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
					to: 0xa.into(),
					value: 100.into(),
					gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(79_000),
					output: vec![],
				}),
			},
			FlatTrace {
				trace_address: vec![0].into_iter().collect(),
				subtraces: 1,
				action: trace::Action::Call(trace::Call {
					from: 0xa.into(),
					to: 0xb.into(),
					value: 0.into(),
					gas: 78934.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::FailedCall(TraceError::OutOfGas),
			},
			FlatTrace {
				trace_address: vec![0, 0].into_iter().collect(),
				subtraces: 0,
				action: trace::Action::Call(trace::Call {
					from: 0xb.into(),
					to: 0xc.into(),
					value: 0.into(),
					gas: 78868.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					call_type: CallType::Call,
					input: vec![],
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: U256::from(3),
					output: vec![],
				}),
			},
		];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn should_trace_suicide() {
		init_log();

		let mut state = get_temp_state();

		let mut info = EnvInfo::default();
		info.gas_limit = 1_000_000.into();
		let machine = make_frontier_machine(5);

		let t = Transaction {
			nonce: 0.into(),
			gas_price: 0.into(),
			gas: 100_000.into(),
			action: Action::Call(0xa.into()),
			value: 100.into(),
			data: vec![],
		}
		.sign(&secret(), None);

		state
			.init_code(
				&0xa.into(),
				FromHex::from_hex("73000000000000000000000000000000000000000bff").unwrap(),
			)
			.unwrap();
		state
			.add_balance(&0xa.into(), &50.into(), CleanupMode::NoEmpty)
			.unwrap();
		state
			.add_balance(&t.sender(), &100.into(), CleanupMode::NoEmpty)
			.unwrap();
		let result = state.apply(&info, &machine, &t, true, false).unwrap();
		let expected_trace = vec![
			FlatTrace {
				trace_address: Default::default(),
				subtraces: 1,
				action: trace::Action::Call(trace::Call {
					from: "9cce34f7ab185c7aba1b7c8140d620b4bda941d6".into(),
					to: 0xa.into(),
					value: 100.into(),
					gas: 79000.into(), // NOTICE: This value will change if the gas model changes, so please update accordingly.
					input: vec![],
					call_type: CallType::Call,
				}),
				result: trace::Res::Call(trace::CallResult {
					gas_used: 3.into(),
					output: vec![],
				}),
			},
			FlatTrace {
				trace_address: vec![0].into_iter().collect(),
				subtraces: 0,
				action: trace::Action::Suicide(trace::Suicide {
					address: 0xa.into(),
					refund_address: 0xb.into(),
					balance: 150.into(),
				}),
				result: trace::Res::None,
			},
		];

		assert_eq!(result.trace, expected_trace);
	}

	#[test]
	fn code_from_database() {
		let a = Address::zero();
		let (db, mkvs) = {
			let mut state = get_temp_state();
			state
				.require_or_from(
					&a,
					false,
					|| Account::new_contract(42.into(), 0.into(), 0),
					|_| {},
				)
				.unwrap();
			state.init_code(&a, vec![1, 2, 3]).unwrap();
			assert_eq!(state.code(&a).unwrap(), Some(Arc::new(vec![1u8, 2, 3])));
			state.commit().unwrap();
			assert_eq!(state.code(&a).unwrap(), Some(Arc::new(vec![1u8, 2, 3])));
			state.drop()
		};

		let state =
			State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		assert_eq!(state.code(&a).unwrap(), Some(Arc::new(vec![1u8, 2, 3])));
	}

	#[test]
	fn storage_at_from_database() {
		let a = Address::zero();
		let (db, mkvs) = {
			let mut state = get_temp_state();
			state
				.set_storage(
					&a,
					H256::from(&U256::from(1u64)),
					H256::from(&U256::from(69u64)),
				)
				.unwrap();
			state.commit().unwrap();
			state.drop()
		};

		let s = State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		assert_eq!(
			s.storage_at(&a, &H256::from(&U256::from(1u64))).unwrap(),
			H256::from(&U256::from(69u64))
		);
	}

	#[test]
	fn get_from_database() {
		let a = Address::zero();
		let (db, mkvs) = {
			let mut state = get_temp_state();
			state.inc_nonce(&a).unwrap();
			state
				.add_balance(&a, &U256::from(69u64), CleanupMode::NoEmpty)
				.unwrap();
			state.commit().unwrap();
			assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
			state.drop()
		};

		let state =
			State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
		assert_eq!(state.nonce(&a).unwrap(), U256::from(1u64));
	}

	#[test]
	fn remove() {
		let a = Address::zero();
		let mut state = get_temp_state();
		assert_eq!(state.exists(&a).unwrap(), false);
		assert_eq!(state.exists_and_not_null(&a).unwrap(), false);
		state.inc_nonce(&a).unwrap();
		assert_eq!(state.exists(&a).unwrap(), true);
		assert_eq!(state.exists_and_not_null(&a).unwrap(), true);
		assert_eq!(state.nonce(&a).unwrap(), U256::from(1u64));
		state.kill_account(&a);
		assert_eq!(state.exists(&a).unwrap(), false);
		assert_eq!(state.exists_and_not_null(&a).unwrap(), false);
		assert_eq!(state.nonce(&a).unwrap(), U256::from(0u64));
	}

	#[test]
	fn empty_account_is_not_created() {
		let a = Address::zero();
		let db = get_temp_state_db();
		let mkvs = Box::new(MemoryMKVS::new());
		let (db, mkvs) = {
			let mut state = State::new(mkvs, db, U256::from(0), Default::default());
			state
				.add_balance(&a, &U256::default(), CleanupMode::NoEmpty)
				.unwrap(); // create an empty account
			state.commit().unwrap();
			state.drop()
		};
		let state =
			State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		assert!(!state.exists(&a).unwrap());
		assert!(!state.exists_and_not_null(&a).unwrap());
	}

	#[test]
	fn empty_account_exists_when_creation_forced() {
		let a = Address::zero();
		let db = get_temp_state_db();
		let mkvs = Box::new(MemoryMKVS::new());
		let (db, mkvs) = {
			let mut state = State::new(mkvs, db, U256::from(0), Default::default());
			state
				.add_balance(&a, &U256::default(), CleanupMode::ForceCreate)
				.unwrap(); // create an empty account
			state.commit().unwrap();
			state.drop()
		};
		let state =
			State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		assert!(state.exists(&a).unwrap());
		assert!(!state.exists_and_not_null(&a).unwrap());
	}

	#[test]
	fn remove_from_database() {
		let a = Address::zero();
		let mkvs = Box::new(MemoryMKVS::new());
		let (db, mkvs) = {
			let mut state = get_temp_state();
			state.inc_nonce(&a).unwrap();
			state.commit().unwrap();
			assert_eq!(state.exists(&a).unwrap(), true);
			assert_eq!(state.nonce(&a).unwrap(), U256::from(1u64));
			state.drop()
		};

		let (db, mkvs) = {
			let mut state =
				State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
			assert_eq!(state.exists(&a).unwrap(), true);
			assert_eq!(state.nonce(&a).unwrap(), U256::from(1u64));
			state.kill_account(&a);
			state.commit().unwrap();
			assert_eq!(state.exists(&a).unwrap(), false);
			assert_eq!(state.nonce(&a).unwrap(), U256::from(0u64));
			state.drop()
		};

		let state =
			State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		assert_eq!(state.exists(&a).unwrap(), false);
		assert_eq!(state.nonce(&a).unwrap(), U256::from(0u64));
	}

	#[test]
	fn alter_balance() {
		let mut state = get_temp_state();
		let a = Address::zero();
		let b = 1u64.into();
		state
			.add_balance(&a, &U256::from(69u64), CleanupMode::NoEmpty)
			.unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
		state.commit().unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
		state
			.sub_balance(&a, &U256::from(42u64), &mut CleanupMode::NoEmpty)
			.unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(27u64));
		state.commit().unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(27u64));
		state
			.transfer_balance(&a, &b, &U256::from(18u64), CleanupMode::NoEmpty)
			.unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(9u64));
		assert_eq!(state.balance(&b).unwrap(), U256::from(18u64));
		state.commit().unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(9u64));
		assert_eq!(state.balance(&b).unwrap(), U256::from(18u64));
	}

	#[test]
	fn alter_nonce() {
		let mut state = get_temp_state();
		let a = Address::zero();
		state.inc_nonce(&a).unwrap();
		assert_eq!(state.nonce(&a).unwrap(), U256::from(1u64));
		state.inc_nonce(&a).unwrap();
		assert_eq!(state.nonce(&a).unwrap(), U256::from(2u64));
		state.commit().unwrap();
		assert_eq!(state.nonce(&a).unwrap(), U256::from(2u64));
		state.inc_nonce(&a).unwrap();
		assert_eq!(state.nonce(&a).unwrap(), U256::from(3u64));
		state.commit().unwrap();
		assert_eq!(state.nonce(&a).unwrap(), U256::from(3u64));
	}

	#[test]
	fn balance_nonce() {
		let mut state = get_temp_state();
		let a = Address::zero();
		assert_eq!(state.balance(&a).unwrap(), U256::from(0u64));
		assert_eq!(state.nonce(&a).unwrap(), U256::from(0u64));
		state.commit().unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(0u64));
		assert_eq!(state.nonce(&a).unwrap(), U256::from(0u64));
	}

	#[test]
	fn ensure_cached() {
		let mut state = get_temp_state();
		let a = Address::zero();
		state.require(&a, false).unwrap();
		state.commit().unwrap();
		//assert_eq!(*state.root(), "911f21ac6c69e16485433cade78bcb6f0f98b6df40ea093e473db3488dacedfe".into());
	}

	#[test]
	fn checkpoint_basic() {
		let mut state = get_temp_state();
		let a = Address::zero();
		state.checkpoint();
		state
			.add_balance(&a, &U256::from(69u64), CleanupMode::NoEmpty)
			.unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
		state.discard_checkpoint();
		assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
		state.checkpoint();
		state
			.add_balance(&a, &U256::from(1u64), CleanupMode::NoEmpty)
			.unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(70u64));
		state.revert_to_checkpoint();
		assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
	}

	#[test]
	fn checkpoint_nested() {
		let mut state = get_temp_state();
		let a = Address::zero();
		state.checkpoint();
		state.checkpoint();
		state
			.add_balance(&a, &U256::from(69u64), CleanupMode::NoEmpty)
			.unwrap();
		assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
		state.discard_checkpoint();
		assert_eq!(state.balance(&a).unwrap(), U256::from(69u64));
		state.revert_to_checkpoint();
		assert_eq!(state.balance(&a).unwrap(), U256::from(0));
	}

	#[test]
	fn create_empty() {
		let mut state = get_temp_state();
		state.commit().unwrap();
		//assert_eq!(*state.root(), "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421".into());
	}

	#[test]
	fn should_not_panic_on_state_diff_with_storage() {
		let mut state = get_temp_state();

		let a: Address = 0xa.into();
		state.init_code(&a, b"abcdefg".to_vec()).unwrap();;
		state
			.add_balance(&a, &256.into(), CleanupMode::NoEmpty)
			.unwrap();
		state.set_storage(&a, 0xb.into(), 0xc.into()).unwrap();

		let mut new_state = state.clone();
		new_state.set_storage(&a, 0xb.into(), 0xd.into()).unwrap();

		new_state.diff_from(state).unwrap();
	}

	#[test]
	fn should_kill_garbage() {
		let a = 10.into();
		let b = 20.into();
		let c = 30.into();
		let d = 40.into();
		let e = 50.into();
		let x = 0.into();
		let db = get_temp_state_db();
		let mkvs = Box::new(MemoryMKVS::new());
		let (db, mkvs) = {
			let mut state = State::new(mkvs, db, U256::from(0), Default::default());
			state
				.add_balance(&a, &U256::default(), CleanupMode::ForceCreate)
				.unwrap(); // create an empty account
			state
				.add_balance(&b, &100.into(), CleanupMode::ForceCreate)
				.unwrap(); // create a dust account
			state
				.add_balance(&c, &101.into(), CleanupMode::ForceCreate)
				.unwrap(); // create a normal account
			state
				.add_balance(&d, &99.into(), CleanupMode::ForceCreate)
				.unwrap(); // create another dust account
			state.new_contract(&e, 100.into(), 1.into(), 0); // create a contract account
			state.init_code(&e, vec![0x00]).unwrap();
			state.commit().unwrap();
			state.drop()
		};

		let mut state =
			State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		let mut touched = HashSet::new();
		state
			.add_balance(
				&a,
				&U256::default(),
				CleanupMode::TrackTouched(&mut touched),
			)
			.unwrap(); // touch an account
		state
			.transfer_balance(&b, &x, &1.into(), CleanupMode::TrackTouched(&mut touched))
			.unwrap(); // touch an account decreasing its balance
		state
			.transfer_balance(&c, &x, &1.into(), CleanupMode::TrackTouched(&mut touched))
			.unwrap(); // touch an account decreasing its balance
		state
			.transfer_balance(&e, &x, &1.into(), CleanupMode::TrackTouched(&mut touched))
			.unwrap(); // touch an account decreasing its balance
		state.kill_garbage(&touched, true, &None, false).unwrap();
		assert!(!state.exists(&a).unwrap());
		assert!(state.exists(&b).unwrap());
		state
			.kill_garbage(&touched, true, &Some(100.into()), false)
			.unwrap();
		assert!(!state.exists(&b).unwrap());
		assert!(state.exists(&c).unwrap());
		assert!(state.exists(&d).unwrap());
		assert!(state.exists(&e).unwrap());
		state
			.kill_garbage(&touched, true, &Some(100.into()), true)
			.unwrap();
		assert!(state.exists(&c).unwrap());
		assert!(state.exists(&d).unwrap());
		assert!(!state.exists(&e).unwrap());
	}

	#[test]
	fn should_trace_diff_suicided_accounts() {
		use pod_account;

		let a = 10.into();
		let db = get_temp_state_db();
		let mkvs = Box::new(MemoryMKVS::new());
		let (db, mkvs) = {
			let mut state = State::new(mkvs, db, U256::from(0), Default::default());
			state
				.add_balance(&a, &100.into(), CleanupMode::ForceCreate)
				.unwrap();
			state.commit().unwrap();
			state.drop()
		};

		let mut state =
			State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		let original = state.clone();
		state.kill_account(&a);

		let diff = state.diff_from(original).unwrap();
		let diff_map = diff.get();
		assert_eq!(diff_map.len(), 1);
		assert!(diff_map.get(&a).is_some());
		assert_eq!(
			diff_map.get(&a),
			pod_account::diff_pod(
				Some(&PodAccount {
					balance: U256::from(100),
					nonce: U256::zero(),
					code: Some(Default::default()),
					storage_expiry: 0,
					storage: Default::default()
				}),
				None
			)
			.as_ref()
		);
	}

	#[test]
	fn should_trace_diff_unmodified_storage() {
		use pod_account;

		let a = 10.into();
		let db = get_temp_state_db();
		let mkvs = Box::new(MemoryMKVS::new());

		let (db, mkvs) = {
			let mut state = State::new(mkvs, db, U256::from(0), Default::default());
			state
				.set_storage(
					&a,
					H256::from(&U256::from(1u64)),
					H256::from(&U256::from(20u64)),
				)
				.unwrap();
			state.commit().unwrap();
			state.drop()
		};

		let mut state =
			State::from_existing(mkvs, db, U256::from(0u8), Default::default(), None).unwrap();
		let original = state.clone();
		state
			.set_storage(
				&a,
				H256::from(&U256::from(1u64)),
				H256::from(&U256::from(100u64)),
			)
			.unwrap();

		let diff = state.diff_from(original).unwrap();
		let diff_map = diff.get();
		assert_eq!(diff_map.len(), 1);
		assert!(diff_map.get(&a).is_some());
		assert_eq!(
			diff_map.get(&a),
			pod_account::diff_pod(
				Some(&PodAccount {
					balance: U256::zero(),
					nonce: U256::zero(),
					code: Some(Default::default()),
					storage: vec![(
						H256::from(&U256::from(1u64)),
						H256::from(&U256::from(20u64)).to_vec()
					)]
					.into_iter()
					.collect(),
					storage_expiry: 0,
				}),
				Some(&PodAccount {
					balance: U256::zero(),
					nonce: U256::zero(),
					code: Some(Default::default()),
					storage: vec![(
						H256::from(&U256::from(1u64)),
						H256::from(&U256::from(100u64)).to_vec()
					)]
					.into_iter()
					.collect(),
					storage_expiry: 0,
				})
			)
			.as_ref()
		);
	}

	#[test]
	fn should_have_output_from_init_contract() {
		let base_options = TransactOptions::with_tracing();
		let options = State::<StateDB>::get_options(
			base_options.tracer,
			base_options.vm_tracer,
			base_options.ext_tracer,
			false,
			true,
		);

		assert_eq!(options.output_from_init_contract, true);
	}

	#[test]
	fn should_not_have_output_from_init_contract() {
		let base_options = TransactOptions::with_tracing();
		let options = State::<StateDB>::get_options(
			base_options.tracer,
			base_options.vm_tracer,
			base_options.ext_tracer,
			false,
			false,
		);

		assert_eq!(options.output_from_init_contract, false);
	}
}
