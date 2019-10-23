// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! The overlayed changes to state.

#[cfg(test)]
use std::iter::FromIterator;
use std::collections::{HashMap, BTreeSet};
use codec::Decode;
use crate::changes_trie::{NO_EXTRINSIC_INDEX, Configuration as ChangesTrieConfig};
use primitives::storage::well_known_keys::EXTRINSIC_INDEX;
use historied_data::linear::{States, History, HistoriedValue, TransactionState};
use historied_data::DEFAULT_GC_CONF;

/// The overlayed changes to state to be queried on top of the backend.
///
/// A transaction shares all prospective changes within an inner overlay
/// that can be cleared.
#[derive(Debug, Default, Clone)]
pub struct OverlayedChanges {
	/// Changes with their history.
	pub(crate) changes: OverlayedChangeSet,
	/// Changes trie configuration. None by default, but could be installed by the
	/// runtime if it supports change tries.
	pub(crate) changes_trie_config: Option<ChangesTrieConfig>,
	/// Counter of number of operation between garbage collection.
	/// Changing or deleting a value increment this counter by one.
	/// An additional cost for data added is added for every n bytes of data.
	/// n is currently defined by `DEFAULT_GC_CONF`.
	pub(crate) operation_from_last_gc: usize,
	/// Can be set to false to ensure we keep pre committed state (in case we want to move
	/// back committed cursor).
	pub(crate) not_eager_gc: bool,
}

/// The storage value, used inside OverlayedChanges.
#[derive(Debug, Default, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct OverlayedValue {
	/// Current value. None if value has been deleted.
	pub value: Option<Vec<u8>>,
	/// The set of extinsic indices where the values has been changed.
	/// Is filled only if runtime has announced changes trie support.
	pub extrinsics: Option<BTreeSet<u32>>,
}

/// Overlayed change set, keep history of values.
///
/// This does not work by stacking hashmaps per transaction,
/// but by storing history of a value instead.
/// Value validity is given by a global indexed state history.
/// When dropping or committing a transaction layer,
/// full history for each values is kept until
/// next garbage collection.
/// Note that on mutable access terminal values corresponding
/// to a deleted layer are also dropped.
#[derive(Debug, Clone, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct OverlayedChangeSet {
	/// Indexed state history.
	pub(crate) states: States,
	/// Top level storage changes.
	pub(crate) top: HashMap<Vec<u8>, History<OverlayedValue>>,
	/// Child storage changes.
	pub(crate) children: HashMap<Vec<u8>, (HashMap<Vec<u8>, History<OverlayedValue>>)>,
}

#[cfg(test)]
impl FromIterator<(Vec<u8>, OverlayedValue)> for OverlayedChangeSet {
	fn from_iter<T: IntoIterator<Item = (Vec<u8>, OverlayedValue)>>(iter: T) -> Self {

		let mut result = OverlayedChangeSet::default();
		result.top = iter.into_iter().map(|(k, value)| (k, {
			let mut history = History::default();
			history.push_unchecked(HistoriedValue { value, index: 0 });
			history
		})).collect();
		result
	}
}

/// Variant of `set` value that update extrinsics.
/// It does remove latest dropped values.
fn set_with_extrinsic_overlayed_value(
	h_value: &mut History<OverlayedValue>,
	states: (&[TransactionState], usize),
	value: Option<Vec<u8>>,
	extrinsic_index: Option<u32>,
) {
	if let Some(extrinsic) = extrinsic_index {
		set_with_extrinsic_inner_overlayed_value(h_value, states, value, extrinsic)
	} else {
		h_value.set(states, OverlayedValue {
			value,
			extrinsics: None,
		})
	}
}

fn set_with_extrinsic_inner_overlayed_value(
	h_value: &mut History<OverlayedValue>,
	states: (&[TransactionState], usize),
	value: Option<Vec<u8>>,
	extrinsic_index: u32,
) {
	let state = states.0.len() - 1;
	if let Some(current) = h_value.get_mut(states) {
		if current.index == state {
			current.value.value = value;
			current.value.extrinsics.get_or_insert_with(Default::default)
				.insert(extrinsic_index);
		} else {
			let mut extrinsics = current.value.extrinsics.clone();
			extrinsics.get_or_insert_with(Default::default)
				.insert(extrinsic_index);
			h_value.push_unchecked(HistoriedValue {
				value: OverlayedValue {
					value,
					extrinsics,
				},
				index: state,
			});
		}
	} else {
		let mut extrinsics: Option<BTreeSet<u32>> = None;
		extrinsics.get_or_insert_with(Default::default)
			.insert(extrinsic_index);

		h_value.push_unchecked(HistoriedValue {
			value: OverlayedValue {
				value,
				extrinsics,
			},
			index: state,
		});

	}
}

impl OverlayedChangeSet {
	/// Garbage collect.
	fn gc(&mut self, eager: bool) {
		let committed = if eager {
			Some(self.states.committed())
		} else {
			None
		};
		let states = self.states.as_ref_mut();
		// retain does change values
		self.top.retain(|_, h_value| h_value.get_mut_pruning(states, committed).is_some());
		self.children.retain(|_, m| {
			m.retain(|_, h_value| h_value.get_mut_pruning(states, committed).is_some());
			!m.is_empty()
		});
	}

	/// Whether the change set is empty.
	pub fn is_empty(&self) -> bool {
		self.top.is_empty() && self.children.is_empty()
	}

	/// Discard prospective changes to state.
	pub fn discard_prospective(&mut self) {
		self.states.discard_prospective();
	}

	/// Commit prospective changes to state.
	pub fn commit_prospective(&mut self) {
		self.states.commit_prospective();
	}

	/// Create a new transactional layer.
	pub fn start_transaction(&mut self) {
		self.states.start_transaction();
	}

	/// Discard a transactional layer.
	/// A transaction is always running (states always end with pending).
	pub fn discard_transaction(&mut self) {
		self.states.discard_transaction();
	}

	/// Commit a transactional layer.
	pub fn commit_transaction(&mut self) {
		self.states.commit_transaction();
	}

	/// Iterator over current state of a given overlay, including change trie information.
	pub fn iter_overlay(
		&self,
		storage_key: Option<&[u8]>,
	) -> impl Iterator<Item = (&[u8], &OverlayedValue)> {
		let option_map = if let Some(storage_key) = storage_key.as_ref() {
			self.children.get(*storage_key)
		} else {
			Some(&self.top)
		};
		option_map
			.into_iter()
			.flat_map(move |map| map.iter()
				.filter_map(move |(k, v)|
					v.get(self.states.as_ref()).map(|v| (k.as_slice(), v)))
			)
	}

	/// Iterator over current state of a given overlay, values only.
	pub fn iter_values(
		&self,
		storage_key: Option<&[u8]>,
	) -> impl Iterator<Item = (&[u8], Option<&[u8]>)> {
		self.iter_overlay(storage_key)
			.map(|(k, v)| (k, v.value.as_ref().map(|v| v.as_slice())))
	}

	/// Iterator over current state of all children overlays, values only.
	pub fn children_iter(
		&self,
	) -> impl Iterator<Item=(&[u8], impl Iterator<Item = (&[u8], Option<&[u8]>)>)> {
		self.children.iter()
			.map(move |(keyspace, child)| (keyspace.as_slice(), child.iter()
				.filter_map(move |(k, v)|
					v.get(self.states.as_ref())
						.map(|v| (k.as_slice(), v.value.as_ref().map(|v| v.as_slice()))))
			))
	}

	/// Iterator over current state of all children overlays, values only.
	/// Similar to `children_iter` but with key and value as `Vec<u8>`.
	pub fn owned_children_iter<'a>(
		&'a self,
	) -> impl Iterator<Item=(Vec<u8>, impl Iterator<Item = (Vec<u8>, Option<Vec<u8>>)> + 'a)> + 'a {
		self.children.iter()
			.map(move |(keyspace, child)| (keyspace.to_vec(), child.iter()
				.filter_map(move |(k, v)|
					v.get(self.states.as_ref())
						.map(|v| (k.to_vec(), v.value.as_ref().map(|v| v.to_vec()))))
			))
	}

	/// Iterator over current state of all children overlays, including change trie information.
	pub fn children_iter_overlay(
		&self,
	) -> impl Iterator<Item=(&[u8], impl Iterator<Item = (&[u8], &OverlayedValue)>)> {
		self.children.iter()
			.map(move |(keyspace, child)| (keyspace.as_slice(), child.iter()
				.filter_map(move |(k, v)|
					v.get(self.states.as_ref()).map(|v| (k.as_slice(), v)))
			))
	}

	/// Test only method to access current prospective changes.
	/// It is here to keep old test compatibility and should be
	/// avoid for new tests.
	#[cfg(test)]
	pub(crate) fn top_prospective(&self) -> HashMap<Vec<u8>, OverlayedValue> {
		let mut result = HashMap::new();
		let committed = self.states.committed();
		for (k, v) in self.top.iter() {
			if let Some(v) = v.get_prospective(self.states.as_ref(), committed) {
				result.insert(k.clone(), v.clone());
			}
		}
		result
	}

	/// Test only method to access current commited changes.
	/// It is here to keep old test compatibility and should be
	/// avoid for new tests.
	#[cfg(test)]
	pub(crate) fn top_committed(&self) -> HashMap<Vec<u8>, OverlayedValue> {
		let mut result = HashMap::new();
		let committed = self.states.committed();
		for (k, v) in self.top.iter() {
			if let Some(v) = v.get_committed(self.states.as_ref(), committed) {
				result.insert(k.clone(), v.clone());
			}
		}
		result
	}

}

impl OverlayedChanges {
	/// Whether the overlayed changes are empty.
	pub fn is_empty(&self) -> bool {
		self.changes.is_empty()
	}

	/// Sets the changes trie configuration.
	///
	/// Returns false if configuration has been set already and we now trying
	/// to install different configuration. This isn't supported now.
	pub(crate) fn set_changes_trie_config(&mut self, config: ChangesTrieConfig) -> bool {
		if let Some(ref old_config) = self.changes_trie_config {
			// we do not support changes trie configuration' change now
			if *old_config != config {
				return false;
			}
		}

		self.changes_trie_config = Some(config);
		true
	}

	#[cfg(test)]
	/// To allow value without extrinsic this can be use in test to disable change trie.
	pub(crate) fn remove_changes_trie_config(&mut self) -> Option<ChangesTrieConfig> {
		self.changes_trie_config.take()
	}


	/// Returns a double-Option: None if the key is unknown (i.e. and the query should be refered
	/// to the backend); Some(None) if the key has been deleted. Some(Some(...)) for a key whose
	/// value has been set.
	pub fn storage(&self, key: &[u8]) -> Option<Option<&[u8]>> {
		if let Some(overlay_value) = self.changes.top.get(key) {
			if let Some(o_value) = overlay_value.get(self.changes.states.as_ref()) {
				return Some(o_value.value.as_ref().map(|v| v.as_slice()))
			}
		}
		None
	}

	/// Returns a double-Option: None if the key is unknown (i.e. and the query should be refered
	/// to the backend); Some(None) if the key has been deleted. Some(Some(...)) for a key whose
	/// value has been set.
	pub fn child_storage(&self, storage_key: &[u8], key: &[u8]) -> Option<Option<&[u8]>> {
		if let Some(map) = self.changes.children.get(storage_key) {
			if let Some(overlay_value) = map.get(key) {
				if let Some(o_value) = overlay_value.get(self.changes.states.as_ref()) {
					return Some(o_value.value.as_ref().map(|v| v.as_slice()))
				}
			}
		}
		None
	}

	/// Inserts the given key-value pair into the prospective change set.
	///
	/// `None` can be used to delete a value specified by the given key.
	pub fn set_storage(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) {
		self.operation_from_last_gc += DEFAULT_GC_CONF.operation_cost(value.as_ref());
		let extrinsic_index = self.extrinsic_index();
		let entry = self.changes.top.entry(key).or_default();
		let states = self.changes.states.as_ref_mut();
		set_with_extrinsic_overlayed_value(entry, states, value, extrinsic_index);
	}

	/// Inserts the given key-value pair into the prospective child change set.
	///
	/// `None` can be used to delete a value specified by the given key.
	pub(crate) fn set_child_storage(
		&mut self,
		storage_key: Vec<u8>,
		key: Vec<u8>,
		value: Option<Vec<u8>>,
	) {
		self.operation_from_last_gc += DEFAULT_GC_CONF.operation_cost(value.as_ref());
		let extrinsic_index = self.extrinsic_index();
		let map_entry = self.changes.children.entry(storage_key).or_default();
		let entry = map_entry.entry(key).or_default();
		let states = self.changes.states.as_ref_mut();
		set_with_extrinsic_overlayed_value(
			entry,
			states,
			value,
			extrinsic_index,
		);
	}

	/// Clear child storage of given storage key.
	///
	/// NOTE that this doesn't take place immediately but written into the prospective
	/// change set, and still can be reverted by [`discard_prospective`].
	///
	/// [`discard_prospective`]: #method.discard_prospective
	pub(crate) fn clear_child_storage(&mut self, storage_key: &[u8]) {
		let extrinsic_index = self.extrinsic_index();
		let states = self.changes.states.as_ref_mut();
		let map_entry = self.changes.children.entry(storage_key.to_vec()).or_default();

		self.operation_from_last_gc += map_entry.len();
		map_entry.values_mut()
			.for_each(|e| set_with_extrinsic_overlayed_value(e, states, None, extrinsic_index));
	}

	/// Removes all key-value pairs which keys share the given prefix.
	///
	/// NOTE that this doesn't take place immediately but written into the prospective
	/// change set, and still can be reverted by [`discard_prospective`].
	///
	/// [`discard_prospective`]: #method.discard_prospective
	pub(crate) fn clear_prefix(&mut self, prefix: &[u8]) {
		let extrinsic_index = self.extrinsic_index();

		let mut number_removed = 0;
		let states = self.changes.states.as_ref_mut();
		for (key, entry) in self.changes.top.iter_mut() {
			if key.starts_with(prefix) {
				number_removed += 1;
				set_with_extrinsic_overlayed_value(entry, states, None, extrinsic_index);
			}
		}

		self.operation_from_last_gc += number_removed;
	}

	pub(crate) fn clear_child_prefix(&mut self, storage_key: &[u8], prefix: &[u8]) {
		let extrinsic_index = self.extrinsic_index();
		if let Some(child_change) = self.changes.children.get_mut(storage_key) {
			let mut number_removed = 0;
			let states = self.changes.states.as_ref_mut();
			for (key, entry) in child_change.iter_mut() {
				if key.starts_with(prefix) {
					number_removed += 1;
					set_with_extrinsic_overlayed_value(entry, states, None, extrinsic_index);
				}
			}

			self.operation_from_last_gc += number_removed;
		}
	}

	/// Discard prospective changes to state.
	pub fn discard_prospective(&mut self) {
		self.changes.discard_prospective();
		if self.operation_from_last_gc > DEFAULT_GC_CONF.trigger_commit_gc {
			self.operation_from_last_gc = 0;
			self.gc(!self.not_eager_gc);
		}
	}

	/// Commit prospective changes to state.
	pub fn commit_prospective(&mut self) {
		self.changes.commit_prospective();
		if self.operation_from_last_gc > DEFAULT_GC_CONF.trigger_commit_gc {
			self.operation_from_last_gc = 0;
			self.gc(!self.not_eager_gc);
		}
	}

	/// Create a new transactional layer.
	pub fn start_transaction(&mut self) {
		self.changes.start_transaction();
		if self.operation_from_last_gc > DEFAULT_GC_CONF.trigger_transaction_gc {
			self.operation_from_last_gc = 0;
			self.gc(false);
		}
	}

	/// Discard a transactional layer.
	/// A transaction is always running (history always end with pending).
	pub fn discard_transaction(&mut self) {
		self.changes.discard_transaction();
		if self.operation_from_last_gc > DEFAULT_GC_CONF.trigger_transaction_gc {
			self.operation_from_last_gc = 0;
			self.gc(false);
		}
	}

	/// Commit a transactional layer.
	pub fn commit_transaction(&mut self) {
		self.changes.commit_transaction();
		if self.operation_from_last_gc > DEFAULT_GC_CONF.trigger_transaction_gc {
			self.operation_from_last_gc = 0;
			self.gc(false);
		}
	}
	
	/// Consume `OverlayedChanges` and take committed set.
	pub fn into_committed(self) -> (
		impl Iterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		impl Iterator<Item=(Vec<u8>, impl Iterator<Item=(Vec<u8>, Option<Vec<u8>>)>)>,
	){
		let top = self.changes.top;
		let children = self.changes.children;
		let committed = self.changes.states.committed();
		let states = self.changes.states.clone();
		let states2 = self.changes.states;
		(
			top.into_iter()
				.filter_map(move |(k, v)| v.into_committed(states.as_ref(), committed)
					.map(|v| (k, v.value))),
			children.into_iter().map(move |(sk, v)| {
				let states2 = states2.clone();
				(sk, v.into_iter()
					.filter_map(move |(k, v)| v.into_committed(states2.as_ref(), committed)
						.map(|v| (k, v.value))))
			})
		)
	}

	/// Inserts storage entry responsible for current extrinsic index.
	#[cfg(test)]
	pub(crate) fn set_extrinsic_index(&mut self, extrinsic_index: u32) {
		use codec::Encode;
		self.set_storage(EXTRINSIC_INDEX.to_vec(), Some(extrinsic_index.encode()));
	}

	/// Test only method to build from committed info and prospective.
	/// Create an history of two states.
	#[cfg(test)]
	pub(crate) fn new_from_top(
		committed: Vec<(Vec<u8>, Option<Vec<u8>>)>,
		prospective: Vec<(Vec<u8>, Option<Vec<u8>>)>,
		changes_trie_config: Option<ChangesTrieConfig>,
	) -> Self {
		let changes = OverlayedChangeSet::default();
		let mut result = OverlayedChanges {
			changes,
			changes_trie_config,
			operation_from_last_gc: 0,
			not_eager_gc: false,
		};
		committed.into_iter().for_each(|(k, v)| result.set_storage(k, v));
		result.changes.commit_prospective();
		prospective.into_iter().for_each(|(k, v)| result.set_storage(k, v));
		result
	}

	/// Returns current extrinsic index to use in changes trie construction.
	/// None is returned if it is not set or changes trie config is not set.
	/// Persistent value (from the backend) can be ignored because runtime must
	/// set this index before first and unset after last extrinsic is executied.
	/// Changes that are made outside of extrinsics, are marked with
	/// `NO_EXTRINSIC_INDEX` index.
	fn extrinsic_index(&self) -> Option<u32> {
		match self.changes_trie_config.is_some() {
			true => Some(
				self.storage(EXTRINSIC_INDEX)
					.and_then(|idx| idx.and_then(|idx| Decode::decode(&mut &*idx).ok()))
					.unwrap_or(NO_EXTRINSIC_INDEX)),
			false => None,
		}
	}

	#[cfg(any(test, feature = "test-helpers"))]
	/// Iterator over current state of the overlay.
	pub fn iter_values(
		&self,
		storage_key: Option<&[u8]>,
	) -> impl Iterator<Item = (&[u8], Option<&[u8]>)> {
		self.changes.iter_values(storage_key)
	}

	#[cfg(any(test, feature = "test-helpers"))]
	/// Count (slow) the number of key value, history included.
	/// Only for debugging or testing usage.
	pub fn top_count_keyvalue_pair(&self) -> usize {
		let mut result = 0;
		for (_, v) in self.changes.top.iter() {
			result += v.len()
		}
		result
	}

	/// costy garbage collection of unneeded memory from
	/// key values. Eager set to true will remove more
	/// key value but allows more costy memory changes.
	pub fn gc(&mut self, eager: bool) {
		self.changes.gc(eager);
	}
}

#[cfg(test)]
impl From<Option<Vec<u8>>> for OverlayedValue {
	fn from(value: Option<Vec<u8>>) -> OverlayedValue {
		OverlayedValue { value, ..Default::default() }
	}
}

#[cfg(test)]
mod tests {
	use hex_literal::hex;
	use primitives::{
		Blake2Hasher, H256, traits::Externalities, storage::well_known_keys::EXTRINSIC_INDEX,
	};
	use crate::backend::InMemory;
	use crate::changes_trie::InMemoryStorage as InMemoryChangesTrieStorage;
	use crate::ext::Ext;
	use super::*;

	fn strip_extrinsic_index(mut map: HashMap<Vec<u8>, OverlayedValue>) -> HashMap<Vec<u8>, OverlayedValue> {
		map.remove(&EXTRINSIC_INDEX.to_vec());
		map
	}

	#[test]
	fn overlayed_storage_works() {
		let mut overlayed = OverlayedChanges::default();

		let key = vec![42, 69, 169, 142];

		assert!(overlayed.storage(&key).is_none());

		overlayed.set_storage(key.clone(), Some(vec![1, 2, 3]));
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.commit_prospective();
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.set_storage(key.clone(), Some(vec![]));
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[][..]));

		overlayed.set_storage(key.clone(), None);
		assert!(overlayed.storage(&key).unwrap().is_none());

		overlayed.discard_prospective();
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.set_storage(key.clone(), None);
		overlayed.commit_prospective();
		assert!(overlayed.storage(&key).unwrap().is_none());
	}

	#[test]
	fn overlayed_storage_root_works() {
		let initial: HashMap<_, _> = vec![
			(b"doe".to_vec(), b"reindeer".to_vec()),
			(b"dog".to_vec(), b"puppyXXX".to_vec()),
			(b"dogglesworth".to_vec(), b"catXXX".to_vec()),
			(b"doug".to_vec(), b"notadog".to_vec()),
		].into_iter().collect();
		let backend = InMemory::<Blake2Hasher>::from(initial);
		let mut overlay = OverlayedChanges::new_from_top(
			vec![
				(b"dog".to_vec(), Some(b"puppy".to_vec()).into()),
				(b"dogglesworth".to_vec(), Some(b"catYYY".to_vec()).into()),
				(b"doug".to_vec(), Some(vec![]).into()),
			].into_iter().collect(),
			vec![
				(b"dogglesworth".to_vec(), Some(b"cat".to_vec()).into()),
				(b"doug".to_vec(), None.into()),
			].into_iter().collect(), None);

		let changes_trie_storage = InMemoryChangesTrieStorage::<Blake2Hasher, u64>::new();
		let mut ext = Ext::new(
			&mut overlay,
			&backend,
			Some(&changes_trie_storage),
			None,
		);
		const ROOT: [u8; 32] = hex!("39245109cef3758c2eed2ccba8d9b370a917850af3824bc8348d505df2c298fa");

		assert_eq!(ext.storage_root(), H256::from(ROOT));
	}

	#[test]
	fn changes_trie_configuration_is_saved() {
		let mut overlay = OverlayedChanges::default();
		assert!(overlay.changes_trie_config.is_none());
		assert_eq!(
			overlay.set_changes_trie_config(
				ChangesTrieConfig { digest_interval: 4, digest_levels: 1, },
			),
			true,
		);
		assert!(overlay.changes_trie_config.is_some());
	}

	#[test]
	fn changes_trie_configuration_is_saved_twice() {
		let mut overlay = OverlayedChanges::default();
		assert!(overlay.changes_trie_config.is_none());
		assert_eq!(overlay.set_changes_trie_config(ChangesTrieConfig {
			digest_interval: 4, digest_levels: 1,
		}), true);
		overlay.set_extrinsic_index(0);
		overlay.set_storage(vec![1], Some(vec![2]));
		assert_eq!(overlay.set_changes_trie_config(ChangesTrieConfig {
			digest_interval: 4, digest_levels: 1,
		}), true);
		assert_eq!(
			strip_extrinsic_index(overlay.changes.top_prospective()),
			vec![
				(vec![1], OverlayedValue { value: Some(vec![2]), extrinsics: Some(vec![0].into_iter().collect()) }),
			].into_iter().collect(),
		);
	}

	#[test]
	fn panics_when_trying_to_save_different_changes_trie_configuration() {
		let mut overlay = OverlayedChanges::default();
		assert_eq!(overlay.set_changes_trie_config(ChangesTrieConfig {
			digest_interval: 4, digest_levels: 1,
		}), true);
		assert_eq!(overlay.set_changes_trie_config(ChangesTrieConfig {
			digest_interval: 2, digest_levels: 1,
		}), false);
	}

	#[test]
	fn extrinsic_changes_are_collected() {
		let mut overlay = OverlayedChanges::default();
		let _ = overlay.set_changes_trie_config(ChangesTrieConfig {
			digest_interval: 4, digest_levels: 1,
		});

		overlay.set_storage(vec![100], Some(vec![101]));

		overlay.set_extrinsic_index(0);
		overlay.set_storage(vec![1], Some(vec![2]));

		overlay.set_extrinsic_index(1);
		overlay.set_storage(vec![3], Some(vec![4]));

		overlay.set_extrinsic_index(2);
		overlay.set_storage(vec![1], Some(vec![6]));

		assert_eq!(strip_extrinsic_index(overlay.changes.top_prospective()),
			vec![
				(vec![1], OverlayedValue { value: Some(vec![6]), extrinsics: Some(vec![0, 2].into_iter().collect()) }),
				(vec![3], OverlayedValue { value: Some(vec![4]), extrinsics: Some(vec![1].into_iter().collect()) }),
				(vec![100], OverlayedValue { value: Some(vec![101]), extrinsics: Some(vec![NO_EXTRINSIC_INDEX].into_iter().collect()) }),
			].into_iter().collect());

		overlay.commit_prospective();

		overlay.set_extrinsic_index(3);
		overlay.set_storage(vec![3], Some(vec![7]));

		overlay.set_extrinsic_index(4);
		overlay.set_storage(vec![1], Some(vec![8]));

		assert_eq!(strip_extrinsic_index(overlay.changes.top_committed()),
			vec![
				(vec![1], OverlayedValue { value: Some(vec![6]), extrinsics: Some(vec![0, 2].into_iter().collect()) }),
				(vec![3], OverlayedValue { value: Some(vec![4]), extrinsics: Some(vec![1].into_iter().collect()) }),
				(vec![100], OverlayedValue { value: Some(vec![101]), extrinsics: Some(vec![NO_EXTRINSIC_INDEX].into_iter().collect()) }),
			].into_iter().collect());

		assert_eq!(strip_extrinsic_index(overlay.changes.top_prospective()),
			vec![
				(vec![1], OverlayedValue { value: Some(vec![8]), extrinsics: Some(vec![0, 2, 4].into_iter().collect()) }),
				(vec![3], OverlayedValue { value: Some(vec![7]), extrinsics: Some(vec![1, 3].into_iter().collect()) }),
			].into_iter().collect());

		overlay.commit_prospective();

		assert_eq!(strip_extrinsic_index(overlay.changes.top_committed()),
			vec![
				(vec![1], OverlayedValue { value: Some(vec![8]), extrinsics: Some(vec![0, 2, 4].into_iter().collect()) }),
				(vec![3], OverlayedValue { value: Some(vec![7]), extrinsics: Some(vec![1, 3].into_iter().collect()) }),
				(vec![100], OverlayedValue { value: Some(vec![101]), extrinsics: Some(vec![NO_EXTRINSIC_INDEX].into_iter().collect()) }),
			].into_iter().collect());

		assert_eq!(overlay.changes.top_prospective(),
			Default::default());
	}


	#[test]
	fn overlayed_storage_transactions() {
		let mut overlayed = OverlayedChanges::default();

		let key = vec![42, 69, 169, 142];

		assert!(overlayed.storage(&key).is_none());

		// discard transaction similar to discard prospective if no transaction.
 
		overlayed.set_storage(key.clone(), Some(vec![1, 2, 3]));
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.discard_transaction();
		assert_eq!(overlayed.storage(&key), None);

		overlayed.discard_prospective();
		assert_eq!(overlayed.storage(&key), None);

		overlayed.set_storage(key.clone(), Some(vec![1, 2, 3]));
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.commit_transaction();
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));


		overlayed.discard_transaction();
		assert_eq!(overlayed.storage(&key), None);
		// basic transaction test
		// tx:1
		overlayed.set_storage(key.clone(), Some(vec![1, 2, 3]));
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.start_transaction();
		// tx:2
		overlayed.set_storage(key.clone(), Some(vec![1, 2, 3, 4]));
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3, 4][..]));

		overlayed.start_transaction();
		// tx:3
		overlayed.set_storage(key.clone(), None);
		assert_eq!(overlayed.storage(&key).unwrap(), None);

		overlayed.discard_transaction();
		// tx:2
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3, 4][..]));

		overlayed.start_transaction();
		// tx:3
		overlayed.set_storage(key.clone(), None);
		assert_eq!(overlayed.storage(&key).unwrap(), None);

		overlayed.commit_transaction();
		// tx:2
		assert_eq!(overlayed.storage(&key).unwrap(), None);

		overlayed.discard_transaction();
		// tx:1
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));
		overlayed.discard_prospective();
		assert_eq!(overlayed.storage(&key), None);

		// multiple transaction end up to prospective value
		overlayed.start_transaction();
		overlayed.set_storage(key.clone(), Some(vec![1]));
		overlayed.start_transaction();
		overlayed.set_storage(key.clone(), Some(vec![1, 2]));
		overlayed.start_transaction();
		overlayed.set_storage(key.clone(), Some(vec![1, 2, 3]));

		overlayed.commit_prospective();
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));
	}

}
