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

//! Parameters for a block chain.

use std::io::Read;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use rustc_hex::FromHex;
use super::genesis::Genesis;
use super::seal::Generic as GenericSeal;

use builtin::Builtin;
use engines::{Engine, NullEngine, InstantSeal, BasicAuthority, AuthorityRound, Tendermint, DEFAULT_BLOCKHASH_CONTRACT};
use vm::{EnvInfo, CallType, ActionValue, ActionParams};
use error::Error;
use ethereum;
use ethjson;
use executive::Executive;
use factory::Factories;
use header::{BlockNumber, Header};
use pod_state::*;
use rlp::{Rlp, RlpStream};
use state_db::StateDB;
use state::{Backend, State, Substate};
use state::backend::Basic as BasicBackend;
use trace::{NoopTracer, NoopVMTracer};
use util::*;

/// Parameters common to ethereum-like blockchains.
/// NOTE: when adding bugfix hard-fork parameters,
/// add to `contains_bugfix_hard_fork`
///
/// we define a "bugfix" hard fork as any hard fork which
/// you would put on-by-default in a new chain.
#[derive(Debug, PartialEq, Default)]
#[cfg_attr(test, derive(Clone))]
pub struct CommonParams {
	/// Account start nonce.
	pub account_start_nonce: U256,
	/// Maximum size of extra data.
	pub maximum_extra_data_size: usize,
	/// Network id.
	pub network_id: u64,
	/// Chain id.
	pub chain_id: u64,
	/// Main subprotocol name.
	pub subprotocol_name: String,
	/// Minimum gas limit.
	pub min_gas_limit: U256,
	/// Fork block to check.
	pub fork_block: Option<(BlockNumber, H256)>,
	/// Number of first block where EIP-98 rules begin.
	pub eip98_transition: BlockNumber,
	/// Number of first block where EIP-155 rules begin.
	pub eip155_transition: BlockNumber,
	/// Validate block receipts root.
	pub validate_receipts_transition: BlockNumber,
	/// Number of first block where EIP-86 (Metropolis) rules begin.
	pub eip86_transition: BlockNumber,
	/// Number of first block where EIP-140 (Metropolis: REVERT opcode) rules begin.
	pub eip140_transition: BlockNumber,
	/// Number of first block where EIP-210 (Metropolis: BLOCKHASH changes) rules begin.
	pub eip210_transition: BlockNumber,
	/// EIP-210 Blockhash contract address.
	pub eip210_contract_address: Address,
	/// EIP-210 Blockhash contract code.
	pub eip210_contract_code: Bytes,
	/// Gas allocated for EIP-210 blockhash update.
	pub eip210_contract_gas: U256,
	/// Number of first block where EIP-211 (Metropolis: RETURNDATASIZE/RETURNDATACOPY) rules begin.
	pub eip211_transition: BlockNumber,
	/// Number of first block where EIP-214 rules begin.
	pub eip214_transition: BlockNumber,
	/// Number of first block where dust cleanup rules (EIP-168 and EIP169) begin.
	pub dust_protection_transition: BlockNumber,
	/// Nonce cap increase per block. Nonce cap is only checked if dust protection is enabled.
	pub nonce_cap_increment: u64,
	/// Enable dust cleanup for contracts.
	pub remove_dust_contracts: bool,
	/// Wasm support
	pub wasm: bool,
	/// Gas limit bound divisor (how much gas limit can change per block)
	pub gas_limit_bound_divisor: U256,
	/// Block reward in wei.
	pub block_reward: U256,
	/// Registrar contract address.
	pub registrar: Address,
}

impl CommonParams {
	/// Schedule for an EVM in the post-EIP-150-era of the Ethereum main net.
	pub fn schedule(&self, block_number: u64) -> ::vm::Schedule {
		let mut schedule = ::vm::Schedule::new_post_eip150(usize::max_value(), true, true, true);
		self.update_schedule(block_number, &mut schedule);
		schedule
	}

	/// Apply common spec config parameters to the schedule.
 	pub fn update_schedule(&self, block_number: u64, schedule: &mut ::vm::Schedule) {
		schedule.have_create2 = block_number >= self.eip86_transition;
		schedule.have_revert = block_number >= self.eip140_transition;
		schedule.have_static_call = block_number >= self.eip214_transition;
		schedule.have_return_data = block_number >= self.eip211_transition;
		if block_number >= self.eip210_transition {
			schedule.blockhash_gas = 350;
		}
		if block_number >= self.dust_protection_transition {
			schedule.kill_dust = match self.remove_dust_contracts {
				true => ::vm::CleanDustMode::WithCodeAndStorage,
				false => ::vm::CleanDustMode::BasicOnly,
			};
		}
	}

	/// Whether these params contain any bug-fix hard forks.
	pub fn contains_bugfix_hard_fork(&self) -> bool {
		self.eip98_transition != 0 &&
			self.eip155_transition != 0 &&
			self.validate_receipts_transition != 0 &&
			self.eip86_transition != 0 &&
			self.eip140_transition != 0 &&
			self.eip210_transition != 0 &&
			self.eip211_transition != 0 &&
			self.eip214_transition != 0 &&
			self.dust_protection_transition != 0
	}
}

impl From<ethjson::spec::Params> for CommonParams {
	fn from(p: ethjson::spec::Params) -> Self {
		CommonParams {
			account_start_nonce: p.account_start_nonce.map_or_else(U256::zero, Into::into),
			maximum_extra_data_size: p.maximum_extra_data_size.into(),
			network_id: p.network_id.into(),
			chain_id: if let Some(n) = p.chain_id { n.into() } else { p.network_id.into() },
			subprotocol_name: p.subprotocol_name.unwrap_or_else(|| "eth".to_owned()),
			min_gas_limit: p.min_gas_limit.into(),
			fork_block: if let (Some(n), Some(h)) = (p.fork_block, p.fork_hash) { Some((n.into(), h.into())) } else { None },
			eip98_transition: p.eip98_transition.map_or(0, Into::into),
			eip155_transition: p.eip155_transition.map_or(0, Into::into),
			validate_receipts_transition: p.validate_receipts_transition.map_or(0, Into::into),
			eip86_transition: p.eip86_transition.map_or(BlockNumber::max_value(), Into::into),
			eip140_transition: p.eip140_transition.map_or(BlockNumber::max_value(), Into::into),
			eip210_transition: p.eip210_transition.map_or(BlockNumber::max_value(), Into::into),
			eip210_contract_address: p.eip210_contract_address.map_or(0xf0.into(), Into::into),
			eip210_contract_code: p.eip210_contract_code.map_or_else(
				|| DEFAULT_BLOCKHASH_CONTRACT.from_hex().expect("Default BLOCKHASH contract is valid"),
				Into::into),
			eip210_contract_gas: p.eip210_contract_gas.map_or(1000000.into(), Into::into),
			eip211_transition: p.eip211_transition.map_or(BlockNumber::max_value(), Into::into),
			eip214_transition: p.eip214_transition.map_or(BlockNumber::max_value(), Into::into),
			dust_protection_transition: p.dust_protection_transition.map_or(BlockNumber::max_value(), Into::into),
			nonce_cap_increment: p.nonce_cap_increment.map_or(64, Into::into),
			remove_dust_contracts: p.remove_dust_contracts.unwrap_or(false),
			wasm: p.wasm.unwrap_or(false),
			gas_limit_bound_divisor: p.gas_limit_bound_divisor.into(),
			block_reward: p.block_reward.map_or_else(U256::zero, Into::into),
			registrar: p.registrar.map_or_else(Address::new, Into::into),
		}
	}
}

/// Parameters for a block chain; includes both those intrinsic to the design of the
/// chain and those to be interpreted by the active chain engine.
pub struct Spec {
	/// User friendly spec name
	pub name: String,
	/// What engine are we using for this?
	pub engine: Arc<Engine>,
	/// Name of the subdir inside the main data dir to use for chain data and settings.
	pub data_dir: String,

	/// Known nodes on the network in enode format.
	pub nodes: Vec<String>,

	/// The genesis block's parent hash field.
	pub parent_hash: H256,
	/// The genesis block's author field.
	pub author: Address,
	/// The genesis block's difficulty field.
	pub difficulty: U256,
	/// The genesis block's gas limit field.
	pub gas_limit: U256,
	/// The genesis block's gas used field.
	pub gas_used: U256,
	/// The genesis block's timestamp field.
	pub timestamp: u64,
	/// Transactions root of the genesis block. Should be SHA3_NULL_RLP.
	pub transactions_root: H256,
	/// Receipts root of the genesis block. Should be SHA3_NULL_RLP.
	pub receipts_root: H256,
	/// The genesis block's extra data field.
	pub extra_data: Bytes,
	/// Each seal field, expressed as RLP, concatenated.
	pub seal_rlp: Bytes,

	/// Contract constructors to be executed on genesis.
	constructors: Vec<(Address, Bytes)>,

	/// May be prepopulated if we know this in advance.
	state_root_memo: RwLock<H256>,

	/// Genesis state as plain old data.
	genesis_state: PodState,
}

fn load_from<T: AsRef<Path>>(cache_dir: T, s: ethjson::spec::Spec) -> Result<Spec, Error> {
	let builtins = s.accounts.builtins().into_iter().map(|p| (p.0.into(), From::from(p.1))).collect();
	let g = Genesis::from(s.genesis);
	let GenericSeal(seal_rlp) = g.seal.into();
	let params = CommonParams::from(s.params);

	let mut s = Spec {
		name: s.name.clone().into(),
		engine: Spec::engine(cache_dir, s.engine, params, builtins),
		data_dir: s.data_dir.unwrap_or(s.name).into(),
		nodes: s.nodes.unwrap_or_else(Vec::new),
		parent_hash: g.parent_hash,
		transactions_root: g.transactions_root,
		receipts_root: g.receipts_root,
		author: g.author,
		difficulty: g.difficulty,
		gas_limit: g.gas_limit,
		gas_used: g.gas_used,
		timestamp: g.timestamp,
		extra_data: g.extra_data,
		seal_rlp: seal_rlp,
		constructors: s.accounts.constructors().into_iter().map(|(a, c)| (a.into(), c.into())).collect(),
		state_root_memo: RwLock::new(Default::default()), // will be overwritten right after.
		genesis_state: s.accounts.into(),
	};

	// use memoized state root if provided.
	match g.state_root {
		Some(root) => *s.state_root_memo.get_mut() = root,
		None => { let _ = s.run_constructors(&Default::default(), BasicBackend(MemoryDB::new()))?; },
	}

	Ok(s)
}

macro_rules! load_bundled {
	($e:expr) => {
		Spec::load(
			&::std::env::temp_dir(),
			include_bytes!(concat!("../../res/", $e, ".json")) as &[u8]
		).expect(concat!("Chain spec ", $e, " is invalid."))
	};
}

impl Spec {
	/// Convert engine spec into a arc'd Engine of the right underlying type.
	/// TODO avoid this hard-coded nastiness - use dynamic-linked plugin framework instead.
	fn engine<T: AsRef<Path>>(
		cache_dir: T,
		engine_spec: ethjson::spec::Engine,
		params: CommonParams,
		builtins: BTreeMap<Address, Builtin>,
	) -> Arc<Engine> {
		match engine_spec {
			ethjson::spec::Engine::Null => Arc::new(NullEngine::new(params, builtins)),
			ethjson::spec::Engine::InstantSeal => Arc::new(InstantSeal::new(params, builtins)),
			ethjson::spec::Engine::Ethash(ethash) => Arc::new(ethereum::Ethash::new(cache_dir, params, From::from(ethash.params), builtins)),
			ethjson::spec::Engine::BasicAuthority(basic_authority) => Arc::new(BasicAuthority::new(params, From::from(basic_authority.params), builtins)),
			ethjson::spec::Engine::AuthorityRound(authority_round) => AuthorityRound::new(params, From::from(authority_round.params), builtins).expect("Failed to start AuthorityRound consensus engine."),
			ethjson::spec::Engine::Tendermint(tendermint) => Tendermint::new(params, From::from(tendermint.params), builtins).expect("Failed to start the Tendermint consensus engine."),
		}
	}

	// given a pre-constructor state, run all the given constructors and produce a new state and state root.
	fn run_constructors<T: Backend>(&self, factories: &Factories, mut db: T) -> Result<T, Error> {
		let mut root = SHA3_NULL_RLP;

		// basic accounts in spec.
		{
			let mut t = factories.trie.create(db.as_hashdb_mut(), &mut root);

			for (address, account) in self.genesis_state.get().iter() {
				t.insert(&**address, &account.rlp())?;
			}
		}

		for (address, account) in self.genesis_state.get().iter() {
			db.note_non_null_account(address);
			account.insert_additional(
				&mut *factories.accountdb.create(db.as_hashdb_mut(), address.sha3()),
				&factories.trie
			);
		}

		let start_nonce = self.engine.account_start_nonce(0);

		let (root, db) = {
			let mut state = State::from_existing(
				db,
				root,
				start_nonce,
				factories.clone(),
			)?;

			// Execute contract constructors.
			let env_info = EnvInfo {
				number: 0,
				author: self.author,
				timestamp: self.timestamp,
				difficulty: self.difficulty,
				last_hashes: Default::default(),
				gas_used: U256::zero(),
				gas_limit: U256::max_value(),
			};

			let from = Address::default();
			for &(ref address, ref constructor) in self.constructors.iter() {
				trace!(target: "spec", "run_constructors: Creating a contract at {}.", address);
				trace!(target: "spec", "  .. root before = {}", state.root());
				let params = ActionParams {
					code_address: address.clone(),
					code_hash: Some(constructor.sha3()),
					address: address.clone(),
					sender: from.clone(),
					origin: from.clone(),
					gas: U256::max_value(),
					gas_price: Default::default(),
					value: ActionValue::Transfer(Default::default()),
					code: Some(Arc::new(constructor.clone())),
					data: None,
					call_type: CallType::None,
				};

				let mut substate = Substate::new();
				state.kill_account(&address);

				{
					let mut exec = Executive::new(&mut state, &env_info, self.engine.as_ref());
					if let Err(e) = exec.create(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer) {
						warn!(target: "spec", "Genesis constructor execution at {} failed: {}.", address, e);
					}
				}

				if let Err(e) = state.commit() {
					warn!(target: "spec", "Genesis constructor trie commit at {} failed: {}.", address, e);
				}

				trace!(target: "spec", "  .. root after = {}", state.root());
			}

			state.drop()
		};

		*self.state_root_memo.write() = root;
		Ok(db)
	}

	/// Return the state root for the genesis state, memoising accordingly.
	pub fn state_root(&self) -> H256 {
		self.state_root_memo.read().clone()
	}

	/// Get common blockchain parameters.
	pub fn params(&self) -> &CommonParams { &self.engine.params() }

	/// Get the known knodes of the network in enode format.
	pub fn nodes(&self) -> &[String] { &self.nodes }

	/// Get the configured Network ID.
	pub fn network_id(&self) -> u64 { self.params().network_id }

	/// Get the configured subprotocol name.
	pub fn subprotocol_name(&self) -> String { self.params().subprotocol_name.clone() }

	/// Get the configured network fork block.
	pub fn fork_block(&self) -> Option<(BlockNumber, H256)> { self.params().fork_block }

	/// Get the header of the genesis block.
	pub fn genesis_header(&self) -> Header {
		let mut header: Header = Default::default();
		header.set_parent_hash(self.parent_hash.clone());
		header.set_timestamp(self.timestamp);
		header.set_number(0);
		header.set_author(self.author.clone());
		header.set_transactions_root(self.transactions_root.clone());
		header.set_uncles_hash(RlpStream::new_list(0).out().sha3());
		header.set_extra_data(self.extra_data.clone());
		header.set_state_root(self.state_root());
		header.set_receipts_root(self.receipts_root.clone());
		header.set_log_bloom(H2048::new().clone());
		header.set_gas_used(self.gas_used.clone());
		header.set_gas_limit(self.gas_limit.clone());
		header.set_difficulty(self.difficulty.clone());
		header.set_seal({
			let r = Rlp::new(&self.seal_rlp);
			r.iter().map(|f| f.as_raw().to_vec()).collect()
		});
		trace!(target: "spec", "Header hash is {}", header.hash());
		header
	}

	/// Compose the genesis block for this chain.
	pub fn genesis_block(&self) -> Bytes {
		let empty_list = RlpStream::new_list(0).out();
		let header = self.genesis_header();
		let mut ret = RlpStream::new_list(3);
		ret.append(&header);
		ret.append_raw(&empty_list, 1);
		ret.append_raw(&empty_list, 1);
		ret.out()
	}

	/// Overwrite the genesis components.
	pub fn overwrite_genesis_params(&mut self, g: Genesis) {
		let GenericSeal(seal_rlp) = g.seal.into();
		self.parent_hash = g.parent_hash;
		self.transactions_root = g.transactions_root;
		self.receipts_root = g.receipts_root;
		self.author = g.author;
		self.difficulty = g.difficulty;
		self.gas_limit = g.gas_limit;
		self.gas_used = g.gas_used;
		self.timestamp = g.timestamp;
		self.extra_data = g.extra_data;
		self.seal_rlp = seal_rlp;
	}

	/// Alter the value of the genesis state.
	pub fn set_genesis_state(&mut self, s: PodState) -> Result<(), Error> {
		self.genesis_state = s;
		let _ = self.run_constructors(&Default::default(), BasicBackend(MemoryDB::new()))?;

		Ok(())
	}

	/// Returns `false` if the memoized state root is invalid. `true` otherwise.
	pub fn is_state_root_valid(&self) -> bool {
		// TODO: get rid of this function and ensure state root always is valid.
		// we're mostly there, but `self.genesis_state.root()` doesn't encompass
		// post-constructor state.
		*self.state_root_memo.read() == self.genesis_state.root()
	}

	/// Ensure that the given state DB has the trie nodes in for the genesis state.
	pub fn ensure_db_good(&self, db: StateDB, factories: &Factories) -> Result<StateDB, Error> {
		if db.as_hashdb().contains(&self.state_root()) {
			return Ok(db)
		}

		// TODO: could optimize so we don't re-run, but `ensure_db_good` is barely ever
		// called anyway.
		let db = self.run_constructors(factories, db)?;
		Ok(db)
	}

	/// Loads spec from json file. Provide factories for executing contracts and ensuring
	/// storage goes to the right place.
	pub fn load<T: AsRef<Path>, R>(cache_dir: T, reader: R) -> Result<Self, String> where R: Read {
		fn fmt<F: ::std::fmt::Display>(f: F) -> String {
			format!("Spec json is invalid: {}", f)
		}

		ethjson::spec::Spec::load(reader).map_err(fmt)
			.and_then(|x| load_from(cache_dir, x).map_err(fmt))
	}

	/// Create a new Spec which conforms to the Frontier-era Morden chain except that it's a NullEngine consensus.
	pub fn new_test() -> Spec { load_bundled!("null_morden") }

	/// Create a new Spec which is a NullEngine consensus with a premine of address whose secret is sha3('').
	pub fn new_null() -> Spec { load_bundled!("null") }

	/// Create a new Spec which constructs a contract at address 5 with storage at 0 equal to 1.
	pub fn new_test_constructor() -> Spec { load_bundled!("constructor") }

	/// Create a new Spec with InstantSeal consensus which does internal sealing (not requiring work).
	pub fn new_instant() -> Spec { load_bundled!("instant_seal") }

	/// Create a new Spec with AuthorityRound consensus which does internal sealing (not requiring work).
	/// Accounts with secrets "0".sha3() and "1".sha3() are the validators.
	pub fn new_test_round() -> Self { load_bundled!("authority_round") }

	/// Create a new Spec with Tendermint consensus which does internal sealing (not requiring work).
	/// Account "0".sha3() and "1".sha3() are a authorities.
	pub fn new_test_tendermint() -> Self { load_bundled!("tendermint") }

	/// TestList.sol used in both specs: https://github.com/paritytech/contracts/pull/30/files
	/// Accounts with secrets "0".sha3() and "1".sha3() are initially the validators.
	/// Create a new Spec with BasicAuthority which uses a contract at address 5 to determine the current validators using `getValidators`.
	/// Second validator can be removed with "0xbfc708a000000000000000000000000082a978b3f5962a5b0957d9ee9eef472ee55b42f1" and added back in using "0x4d238c8e00000000000000000000000082a978b3f5962a5b0957d9ee9eef472ee55b42f1".
	pub fn new_validator_safe_contract() -> Self { load_bundled!("validator_safe_contract") }

	/// The same as the `safeContract`, but allows reporting and uses AuthorityRound.
	/// Account is marked with `reportBenign` it can be checked as disliked with "0xd8f2e0bf".
	/// Validator can be removed with `reportMalicious`.
	pub fn new_validator_contract() -> Self { load_bundled!("validator_contract") }

	/// Create a new Spec with BasicAuthority which uses multiple validator sets changing with height.
	/// Account with secrets "0".sha3() is the validator for block 1 and with "1".sha3() onwards.
	pub fn new_validator_multi() -> Self { load_bundled!("validator_multi") }

	/// Create a new spec for a PoW chain
	pub fn new_pow_test_spec() -> Self { load_bundled!("ethereum/olympic") }
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;
	use util::*;
	use views::*;
	use tests::helpers::get_temp_state_db;
	use state::State;
	use super::*;

	// https://github.com/paritytech/parity/issues/1840
	#[test]
	fn test_load_empty() {
		assert!(Spec::load(::std::env::temp_dir(), &[] as &[u8]).is_err());
	}

	#[test]
	fn all_spec_files_valid() {
		Spec::new_test();
		Spec::new_null();
		Spec::new_test_constructor();
		Spec::new_instant();
		Spec::new_test_round();
		Spec::new_test_tendermint();
		Spec::new_validator_safe_contract();
		Spec::new_validator_contract();
		Spec::new_validator_multi();
	}

	#[test]
	fn test_chain() {
		let test_spec = Spec::new_test();

		assert_eq!(test_spec.state_root(), H256::from_str("f3f4696bbf3b3b07775128eb7a3763279a394e382130f27c21e70233e04946a9").unwrap());
		let genesis = test_spec.genesis_block();
		assert_eq!(BlockView::new(&genesis).header_view().sha3(), H256::from_str("0cd786a2425d16f152c658316c423e6ce1181e15c3295826d7c9904cba9ce303").unwrap());
	}

	#[test]
	fn genesis_constructor() {
		::ethcore_logger::init_log();
		let spec = Spec::new_test_constructor();
		let db = spec.ensure_db_good(get_temp_state_db(), &Default::default()).unwrap();
		let state = State::from_existing(db.boxed_clone(), spec.state_root(), spec.engine.account_start_nonce(0), Default::default()).unwrap();
		let expected = H256::from_str("0000000000000000000000000000000000000000000000000000000000000001").unwrap();
		assert_eq!(state.storage_at(&Address::from_str("0000000000000000000000000000000000000005").unwrap(), &H256::zero()).unwrap(), expected);
	}
}