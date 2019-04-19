// Copyright 2017, 2018 Parity Technologies
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! In-memory trie representation.

use super::{Result, TrieError, TrieMut, TrieLayOut, TrieHash, CError};
use super::lookup::Lookup;
use super::node::Node as EncodedNode;
use node_codec::NodeCodec;
use super::{DBValue, node::NodeKey};

use hash_db::{HashDB, Hasher, Prefix};
use nibbleslice::{self, NibbleSlice, NibbleOps};
use elastic_array::ElasticArray36;
use ::core_::marker::PhantomData;
use ::core_::mem;
use ::core_::ops::Index;
use ::core_::hash::Hash;

#[cfg(feature = "std")]
use ::std::collections::{HashSet, VecDeque};

#[cfg(not(feature = "std"))]
use ::alloc::collections::vec_deque::VecDeque;

#[cfg(not(feature = "std"))]
use ::hashmap_core::HashSet;

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

// For lookups into the Node storage buffer.
// This is deliberately non-copyable.
#[derive(Debug)]
struct StorageHandle(usize);

// Handles to nodes in the trie.
#[derive(Debug)]
enum NodeHandle<H> {
	/// Loaded into memory.
	InMemory(StorageHandle),
	/// Either a hash or an inline node
	Hash(H),
}

impl<H> From<StorageHandle> for NodeHandle<H> {
	fn from(handle: StorageHandle) -> Self {
		NodeHandle::InMemory(handle)
	}
}

fn empty_children<H>() -> Box<[Option<NodeHandle<H>>; 16]> {
	Box::new([
		None, None, None, None, None, None, None, None,
		None, None, None, None, None, None, None, None,
	])
}
// TODO rem? not sure using a different name makes it explicit we are on a full key.
// therefore on left aligned slice
#[derive(Clone, Debug)]
pub(crate) struct PartialKey<'key, N: NibbleOps> {
	key: NibbleSlice<'key, N>,
}

impl<'key, N: NibbleOps> PartialKey<'key, N> {
	fn new(key: NibbleSlice<N>) -> PartialKey<N> {
		PartialKey {
			key,
		}
	}

	fn advance(&mut self, by: usize) {

    assert!(self.key.len() >= by);
		self.key = self.key.mid(by)
	}

  /// advance of return none if underlying slice is to short
  fn checked_advance(&mut self, by: usize) -> bool {
    if self.key.len() >= by {
		  self.key = self.key.mid(by);
      true
    } else { false }
	}

	fn mid(&self) -> NibbleSlice<'key, N> {
		self.key
	}

	fn encoded_prefix(&self) -> Prefix {
		self.key.left()
	}

	fn encoded_prefix_owned(&self) -> (ElasticArray36<u8>, Option<u8>) {
		let (a, b) = self.encoded_prefix();
		(a.into(), b)
	}

}

#[derive(Debug, Clone)]
pub(crate) struct PartialKeyMut<N: NibbleOps> {
	key: Vec<u8>,
	pad: usize,
	marker: PhantomData<N>,
}
// TODO EMCH almost replaceble with nibblevec
impl<N: NibbleOps> PartialKeyMut<N> {
	pub(crate) fn new() -> PartialKeyMut<N> {
		PartialKeyMut {
			key: Vec::new(),
			pad: 0,
			marker: Default::default(),
		}
	}
  /// warning this function expect the partialkey to be left aligned
	pub(crate) fn from_partial(p: &PartialKey<N>) -> PartialKeyMut<N> {
    let (sl, l) = p.encoded_prefix();
    let key = sl[..].into();
    let mut res = PartialKeyMut {
      key,
      pad: 0,
			marker: Default::default(),
    };
    l.map(|l| res.push(l));
    res
	}
	
	// TODO EMCH better truncate
	pub(crate) fn truncate(&mut self, mov: usize) {
		for _ in 0..mov {
			self.pop();
		}
	}

  /// clear
	pub(crate) fn clear(&mut self) {
    self.pad = 0;
    self.key.clear();
	}


	/// ret slice and nb of padding byte TODO rename this is confusing plus it can only be followed
  /// by end: also rename this struct to prefix_accum or something like that
	pub(crate) fn end(&self) -> NibbleSlice<N> {
		(NibbleSlice::new_offset(&self.key[..], self.key.len() * N::NIBBLE_PER_BYTE - self.pad))
	}

	/// Push a nibble onto the `NibbleVec`. Ignores the high 4 bits.
  /// TODO EMCH make a append nibble slice fn (currently we push iter mostly) -> for aligned
	pub(crate) fn push(&mut self, nibble: u8) {
		// TODO EMCH move to N
		let nibble = nibble & 0x0F;

		if self.pad == 0 {
			self.key.push(nibble << 4);
			self.pad = N::NIBBLE_PER_BYTE - 1;
		} else {
			*self.key.last_mut().expect("len != 0 since len % 2 != 0; inner has a last element; qed") |= nibble;
			self.pad -= 1;
		}
	}

	/// Try to pop a nibble off the `NibbleVec`. Fails if len == 0.
	pub(crate) fn pop(&mut self) -> Option<u8> {
		let len = self.key.len() * N::NIBBLE_PER_BYTE - self.pad;
		if len == 0 {
			return None;
		}
		let byte = self.key.pop().expect("len != 0; inner has last elem; qed");
		let nibble = if self.pad == 0 {
			// TODO EMCH rem pop / push
			self.key.push(byte & 0xF0);
			self.pad = 1;
			byte & 0x0F
		} else {
			self.pad -= 1;
			byte >> 4
		};

		Some(nibble)
	}
}


/// Node types in the Trie.
#[derive(Debug)]
enum Node<H> {
	/// Empty node.
	Empty,
	/// A leaf node contains the end of a key and a value.
	/// This key is encoded from a `NibbleSlice`, meaning it contains
	/// a flag indicating it is a leaf.
	Leaf(NodeKey, DBValue),
	/// An extension contains a shared portion of a key and a child node.
	/// The shared portion is encoded from a `NibbleSlice` meaning it contains
	/// a flag indicating it is an extension.
	/// The child node is always a branch.
	Extension(NodeKey, NodeHandle<H>),
	/// A branch has up to 16 children and an optional value.
	Branch(Box<[Option<NodeHandle<H>>; 16]>, Option<DBValue>),
	/// Branch node with support for a nibble (to avoid extension node)
	NibbledBranch(NodeKey, Box<[Option<NodeHandle<H>>; 16]>, Option<DBValue>),
}

impl<O> Node<O>
where
	O: AsRef<[u8]> + AsMut<[u8]> + Default + crate::MaybeDebug + PartialEq + Eq + Hash + Send + Sync + Clone + Copy
{
	// load an inline node into memory or get the hash to do the lookup later.
	fn inline_or_hash<C, H, N>(
		node: &[u8],
		db: &HashDB<H, DBValue>,
		storage: &mut NodeStorage<H::Out>
	) -> NodeHandle<H::Out>
	where
		N: NibbleOps,
		C: NodeCodec<H, N>,
		H: Hasher<Out = O>,
	{
		C::try_decode_hash(&node)
			.map(NodeHandle::Hash)
			.unwrap_or_else(|| {
				let child = Node::from_encoded::<C, H, N>(node, db, storage);
				NodeHandle::InMemory(storage.alloc(Stored::New(child)))
			})
	}

	// decode a node from encoded bytes without getting its children.
	fn from_encoded<'a, 'b, C, H, N>(data: &'a[u8], db: &HashDB<H, DBValue>, storage: &'b mut NodeStorage<H::Out>) -> Self
	where N: NibbleOps, C: NodeCodec<H, N>, H: Hasher<Out = O>,
	{
		let dec_children = |encoded_children: &[Option<&'a [u8]>; 16], storage: &'b mut NodeStorage<H::Out>| {
			let mut child = |i:usize| {
					encoded_children[i].map(|data|
						Self::inline_or_hash::<C, H, N>(data, db, storage)
					)
				};

				Box::new([
					child(0), child(1), child(2), child(3),
					child(4), child(5), child(6), child(7),
					child(8), child(9), child(10), child(11),
					child(12), child(13), child(14), child(15),
				])
		};

		match C::decode(data).unwrap_or(EncodedNode::Empty) {
			EncodedNode::Empty => Node::Empty,
			EncodedNode::Leaf(k, v) => Node::Leaf(k.into(), DBValue::from_slice(&v)),
			EncodedNode::Extension(key, cb) => {
				Node::Extension(
					key.into(),
					Self::inline_or_hash::<C, H, N>(cb, db, storage))
				},
				EncodedNode::Branch(encoded_children, val) => {
					let children = dec_children(&encoded_children, storage);
					Node::Branch(children, val.map(DBValue::from_slice))
				},
				EncodedNode::NibbledBranch(k, encoded_children, val) => {
					let children = dec_children(&encoded_children, storage);
					Node::NibbledBranch(k.into(), children, val.map(DBValue::from_slice))
				},
		}
	}

	// TODO: parallelize TODO EMCH does not need to consume self: change that
	fn into_encoded<F, C, H, N>(self, mut child_cb: F) -> Vec<u8>
	where
		N: NibbleOps,
		C: NodeCodec<H,N>,
		F: FnMut(NodeHandle<H::Out>, Option<&NibbleSlice<N>>, Option<u8>) -> ChildReference<H::Out>,
		H: Hasher<Out = O>,
	{
		match self {
			Node::Empty => C::empty_node().to_vec(),
			Node::Leaf(partial, value) => C::leaf_node(nibbleslice::into_part(&partial), &value),
			Node::Extension(partial, child) => {
				// warning we know that partial does not use pop backward from this point, child_cb using pop 
				// here will break things TODO pop limited version of TrieDBMut??
				let pr = NibbleSlice::<N>::new_offset(&partial.1[..], partial.0);
				let it = pr.right_iter();
				let c = child_cb(child, Some(&pr), None);
				C::ext_node(
					it,
					pr.len(),
					c,
				)
			},
			Node::Branch(mut children, value) => {
				C::branch_node(
					// map the `NodeHandle`s from the Branch to `ChildReferences`
					children.iter_mut()
						.map(Option::take)
						.enumerate()
						.map(|(i, maybe_child)| {
							maybe_child.map(|child|child_cb(child, None, Some(i as u8)))
						}),
					value.as_ref().map(|v|&v[..])
				)
			},
			Node::NibbledBranch(partial, mut children, value) => {
				let pr = NibbleSlice::<N>::new_offset(&partial.1[..], partial.0);
				let it = pr.right_iter();
				C::branch_node_nibbled(
          it,
					pr.len(),
					// map the `NodeHandle`s from the Branch to `ChildReferences`
					children.iter_mut()
						.map(Option::take)
						.enumerate()
						.map(|(i, maybe_child)|{
							//let branch_ix = [i as u8];
							maybe_child.map(|child| {
								// TODO EMCH this clone should be avoid by having a lower limit to pkm
								// and reseting each time (also good to secure pop!!)
								let pr = NibbleSlice::<N>::new_offset(&partial.1[..], partial.0);
								child_cb(child, Some(&pr), Some(i as u8))
							})
						}),
					value.as_ref().map(|v|&v[..])
				)
			},
		}
	}
}

// post-inspect action.
enum Action<H> {
	// Replace a node with a new one.
	Replace(Node<H>),
	// Restore the original node. This trusts that the node is actually the original.
	Restore(Node<H>),
	// if it is a new node, just clears the storage.
	Delete,
}

// post-insert action. Same as action without delete
enum InsertAction<H> {
	// Replace a node with a new one.
	Replace(Node<H>),
	// Restore the original node.
	Restore(Node<H>),
}

impl<H> InsertAction<H> {
	fn into_action(self) -> Action<H> {
		match self {
			InsertAction::Replace(n) => Action::Replace(n),
			InsertAction::Restore(n) => Action::Restore(n),
		}
	}

	// unwrap the node, disregarding replace or restore state.
	fn unwrap_node(self) -> Node<H> {
		match self {
			InsertAction::Replace(n) | InsertAction::Restore(n) => n,
		}
	}
}

// What kind of node is stored here.
enum Stored<H> {
	// A new node.
	New(Node<H>),
	// A cached node, loaded from the DB.
	Cached(Node<H>, H),
}

/// Used to build a collection of child nodes from a collection of `NodeHandle`s
pub enum ChildReference<HO> { // `HO` is e.g. `H256`, i.e. the output of a `Hasher`
	Hash(HO),
	Inline(HO, usize), // usize is the length of the node data we store in the `H::Out`
}

/// Compact and cache-friendly storage for Trie nodes.
struct NodeStorage<H> {
	nodes: Vec<Stored<H>>,
	free_indices: VecDeque<usize>,
}

impl<H> NodeStorage<H> {
	/// Create a new storage.
	fn empty() -> Self {
		NodeStorage {
			nodes: Vec::new(),
			free_indices: VecDeque::new(),
		}
	}

	/// Allocate a new node in the storage.
	fn alloc(&mut self, stored: Stored<H>) -> StorageHandle {
		if let Some(idx) = self.free_indices.pop_front() {
			self.nodes[idx] = stored;
			StorageHandle(idx)
		} else {
			self.nodes.push(stored);
			StorageHandle(self.nodes.len() - 1)
		}
	}

	/// Remove a node from the storage, consuming the handle and returning the node.
	fn destroy(&mut self, handle: StorageHandle) -> Stored<H> {
		let idx = handle.0;

		self.free_indices.push_back(idx);
		mem::replace(&mut self.nodes[idx], Stored::New(Node::Empty))
	}
}

impl<'a, H> Index<&'a StorageHandle> for NodeStorage<H> {
	type Output = Node<H>;

	fn index(&self, handle: &'a StorageHandle) -> &Node<H> {
		match self.nodes[handle.0] {
			Stored::New(ref node) => node,
			Stored::Cached(ref node, _) => node,
		}
	}
}

/// A `Trie` implementation using a generic `HashDB` backing database.
///
/// Use it as a `TrieMut` trait object. You can use `db()` to get the backing database object.
/// Note that changes are not committed to the database until `commit` is called.
/// Querying the root or dropping the trie will commit automatically.
///
/// # Example
/// ```
/// extern crate trie_db;
/// extern crate reference_trie;
/// extern crate hash_db;
/// extern crate keccak_hasher;
/// extern crate memory_db;
///
/// use hash_db::Hasher;
/// use reference_trie::{RefTrieDBMut, TrieMut};
/// use trie_db::DBValue;
/// use keccak_hasher::KeccakHasher;
/// use memory_db::*;
///
/// fn main() {
///   let mut memdb = MemoryDB::<KeccakHasher, HashKey<_>, DBValue>::default();
///   let mut root = Default::default();
///   let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
///   assert!(t.is_empty());
///   assert_eq!(*t.root(), KeccakHasher::hash(&[0u8][..]));
///   t.insert(b"foo", b"bar").unwrap();
///   assert!(t.contains(b"foo").unwrap());
///   assert_eq!(t.get(b"foo").unwrap().unwrap(), DBValue::from_slice(b"bar"));
///   t.remove(b"foo").unwrap();
///   assert!(!t.contains(b"foo").unwrap());
/// }
/// ```
pub struct TrieDBMut<'a, L>
where
	L: TrieLayOut,
{
	storage: NodeStorage<TrieHash<L>>,
	db: &'a mut HashDB<L::H, DBValue>,
	root: &'a mut TrieHash<L>,
	root_handle: NodeHandle<TrieHash<L>>,
	death_row: HashSet<(TrieHash<L>, (ElasticArray36<u8>, Option<u8>))>,
	/// The number of hash operations this trie has performed.
	/// Note that none are performed until changes are committed.
	hash_count: usize,
}

impl<'a, L> TrieDBMut<'a, L>
where
	L: TrieLayOut,
{
	/// Create a new trie with backing database `db` and empty `root`.
	pub fn new(db: &'a mut HashDB<L::H, DBValue>, root: &'a mut TrieHash<L>) -> Self {
		*root = L::C::hashed_null_node();
		let root_handle = NodeHandle::Hash(L::C::hashed_null_node());

		TrieDBMut {
			storage: NodeStorage::empty(),
			db,
			root,
			root_handle,
			death_row: HashSet::new(),
			hash_count: 0,
		}
	}

	/// Create a new trie with the backing database `db` and `root.
	/// Returns an error if `root` does not exist.
	pub fn from_existing(db: &'a mut HashDB<L::H, DBValue>, root: &'a mut TrieHash<L>) -> Result<Self, TrieHash<L>, CError<L>> {
		if !db.contains(root, nibbleslice::EMPTY_ENCODED) {
			return Err(Box::new(TrieError::InvalidStateRoot(*root)));
		}

		let root_handle = NodeHandle::Hash(*root);
		Ok(TrieDBMut {
			storage: NodeStorage::empty(),
			db,
			root,
			root_handle,
			death_row: HashSet::new(),
			hash_count: 0,
		})
	}
	/// Get the backing database.
	pub fn db(&self) -> &HashDB<L::H, DBValue> {
		self.db
	}

	/// Get the backing database mutably.
	pub fn db_mut(&mut self) -> &mut HashDB<L::H, DBValue> {
		self.db
	}

	// cache a node by hash
	fn cache(&mut self, hash: TrieHash<L>, key: Prefix) -> Result<StorageHandle, TrieHash<L>, CError<L>> {
		let node_encoded = self.db.get(&hash, key).ok_or_else(|| Box::new(TrieError::IncompleteDatabase(hash)))?;
		let node = Node::from_encoded::<L::C, L::H, L::N>(
			&node_encoded,
			&*self.db,
			&mut self.storage
		);
		Ok(self.storage.alloc(Stored::Cached(node, hash)))
	}

	// inspect a node, choosing either to replace, restore, or delete it.
	// if restored or replaced, returns the new node along with a flag of whether it was changed.
	fn inspect<F>(&mut self, stored: Stored<TrieHash<L>>, key: &mut PartialKey<L::N>, inspector: F) -> Result<Option<(Stored<TrieHash<L>>, bool)>, TrieHash<L>, CError<L>>
	where F: FnOnce(&mut Self, Node<TrieHash<L>>, &mut PartialKey<L::N>) -> Result<Action<TrieHash<L>>, TrieHash<L>, CError<L>> {
		Ok(match stored {
			Stored::New(node) => match inspector(self, node, key)? {
				Action::Restore(node) => Some((Stored::New(node), false)),
				Action::Replace(node) => Some((Stored::New(node), true)),
				Action::Delete => None,
			},
			Stored::Cached(node, hash) => match inspector(self, node, key)? {
				Action::Restore(node) => Some((Stored::Cached(node, hash), false)),
				Action::Replace(node) => {
					self.death_row.insert((hash, key.encoded_prefix_owned()));
					Some((Stored::New(node), true))
				}
				Action::Delete => {
					self.death_row.insert((hash, key.encoded_prefix_owned()));
					None
				}
			},
		})
	}

	// walk the trie, attempting to find the key's node.
	fn lookup<'x, 'key>(&'x self, mut partial: NibbleSlice<'key, L::N>, handle: &NodeHandle<TrieHash<L>>) -> Result<Option<DBValue>, TrieHash<L>, CError<L>>
		where 'x: 'key
	{
		let mut handle = handle;
		loop {
			let (mid, child) = match *handle {
				NodeHandle::Hash(ref hash) => return Lookup::<L, _> {
					db: &self.db,
					query: DBValue::from_slice,
					hash: hash.clone(),
				}.look_up(partial),
				NodeHandle::InMemory(ref handle) => match self.storage[handle] {
					Node::Empty => return Ok(None),
					Node::Leaf(ref key, ref value) => {
						if NibbleSlice::from_stored(key) == partial {
							return Ok(Some(DBValue::from_slice(value)));
						} else {
							return Ok(None);
						}
					},
					Node::Extension(ref slice, ref child) => {
						let slice = NibbleSlice::from_stored(slice);
						if partial.starts_with(&slice) {
							(slice.len(), child)
						} else {
							return Ok(None);
						}
					},
					Node::Branch(ref children, ref value) => {
						if partial.is_empty() {
							return Ok(value.as_ref().map(|v| DBValue::from_slice(v)));
						} else {
							let idx = partial.at(0);
							match children[idx as usize].as_ref() {
								Some(child) => (1, child),
								None => return Ok(None),
							}
						}
					},
					Node::NibbledBranch(ref slice, ref children, ref value) => {
						let slice = NibbleSlice::from_stored(slice);
						if partial.is_empty() {
							return Ok(value.as_ref().map(|v| DBValue::from_slice(v)));
						} else if partial.starts_with(&slice) {
							let idx = partial.at(0);
							match children[idx as usize].as_ref() {
								Some(child) => (1 + slice.len(), child),
								None => return Ok(None),
							}
						} else {
							return Ok(None)
						}
					},
				}
			};

			partial = partial.mid(mid);
			handle = child;
		}
	}

	/// insert a key-value pair into the trie, creating new nodes if necessary.
	fn insert_at(&mut self, handle: NodeHandle<TrieHash<L>>, key: &mut PartialKey<L::N>, value: DBValue, old_val: &mut Option<DBValue>) -> Result<(StorageHandle, bool), TrieHash<L>, CError<L>> {
		let h = match handle {
			NodeHandle::InMemory(h) => h,
			NodeHandle::Hash(h) => self.cache(h, key.encoded_prefix())?,
		};
		let stored = self.storage.destroy(h); // cache then destroy for hash handle (handle being root in most case), direct access somehow?
		let (new_stored, changed) = self.inspect(stored, key, move |trie, stored, key| {
			trie.insert_inspector(stored, key, value, old_val).map(|a| a.into_action())
		})?.expect("Insertion never deletes.");

		Ok((self.storage.alloc(new_stored), changed))
	}

	/// the insertion inspector.
	fn insert_inspector(&mut self, node: Node<TrieHash<L>>, key: &mut PartialKey<L::N>, value: DBValue, old_val: &mut Option<DBValue>) -> Result<InsertAction<TrieHash<L>>, TrieHash<L>, CError<L>> {
		let partial = key.mid();
		trace!(target: "trie", "augmented (partial: {:?}, value: {:#x?})", partial, value);

		Ok(match node {
			Node::Empty => {
				trace!(target: "trie", "empty: COMPOSE");
				InsertAction::Replace(Node::Leaf(partial.to_stored(), value))
			},
			Node::Branch(mut children, stored_value) => {
				debug_assert!(L::USE_EXTENSION);
				trace!(target: "trie", "branch: ROUTE,AUGMENT");

				if partial.is_empty() {
					let unchanged = stored_value.as_ref() == Some(&value);
					let branch = Node::Branch(children, Some(value));
					*old_val = stored_value;

					match unchanged {
						true => InsertAction::Restore(branch),
						false => InsertAction::Replace(branch),
					}
				} else {
					let idx = partial.at(0) as usize;
					key.advance(1);
					if let Some(child) = children[idx].take() {
						// original had something there. recurse down into it.
						let (new_child, changed) = self.insert_at(child, key, value, old_val)?;
						children[idx] = Some(new_child.into());
						if !changed {
							// the new node we composed didn't change. that means our branch is untouched too.
							return Ok(InsertAction::Restore(Node::Branch(children, stored_value)));
						}
					} else {
						// original had nothing there. compose a leaf.
						let leaf = self.storage.alloc(Stored::New(Node::Leaf(key.mid().to_stored(), value)));
						children[idx] = Some(leaf.into());
					}

					InsertAction::Replace(Node::Branch(children, stored_value))
				}
			},
			Node::NibbledBranch(encoded, mut children, stored_value) => {
				debug_assert!(!L::USE_EXTENSION);
				trace!(target: "trie", "branch: ROUTE,AUGMENT");
				let existing_key = NibbleSlice::from_stored(&encoded);

				let cp = partial.common_prefix(&existing_key);
				if cp == existing_key.len() && cp == partial.len() {
					let unchanged = stored_value.as_ref() == Some(&value);
					let branch = Node::NibbledBranch(existing_key.to_stored(), children, Some(value));
					*old_val = stored_value;

					match unchanged {
						true => InsertAction::Restore(branch),
						false => InsertAction::Replace(branch),
					}
				} else if cp < existing_key.len() {
					// insert a branch value in between
					trace!(target: "trie", "partially-shared-prefix (exist={:?}; new={:?}; cp={:?}): AUGMENT-AT-END", existing_key.len(), partial.len(), cp);
					let low = Node::NibbledBranch(existing_key.mid(cp + 1).to_stored(), children, stored_value);
					let ix = existing_key.at(cp);
					let mut children = empty_children();
					let alloc_storage = self.storage.alloc(Stored::New(low));


					children[ix as usize] = Some(alloc_storage.into());

					if partial.len() - cp == 0 {
						InsertAction::Replace(Node::NibbledBranch(
							existing_key.to_stored_range(cp),
							children,
							Some(value),
							)
						)
					} else {
						let ix = partial.at(cp);
						let leaf = self.storage.alloc(Stored::New(Node::Leaf(partial.mid(cp + 1).to_stored(), value)));

						children[ix as usize] = Some(leaf.into());
						InsertAction::Replace(Node::NibbledBranch(
							existing_key.to_stored_range(cp),
							children,
							None,
							)
						)

					}

				} else {
					// append after cp == existing_key and partial > cp
					trace!(target: "trie", "branch: ROUTE,AUGMENT");
					let idx = partial.at(cp) as usize;
					key.advance(cp + 1);
					if let Some(child) = children[idx].take() {
						// original had something there. recurse down into it.
						let (new_child, changed) = self.insert_at(child, key, value, old_val)?;
						children[idx] = Some(new_child.into());
						if !changed {
							// the new node we composed didn't change. that means our branch is untouched too.
							return Ok(InsertAction::Restore(Node::NibbledBranch(existing_key.to_stored(), children, stored_value)));
						}
					} else {
						// original had nothing there. compose a leaf.
						let leaf = self.storage.alloc(Stored::New(Node::Leaf(key.mid().to_stored(), value)));
						children[idx] = Some(leaf.into());
					}
					InsertAction::Replace(Node::NibbledBranch(
						existing_key.to_stored(),
						children,
						stored_value,
						))
				}
			},
			Node::Leaf(encoded, stored_value) => {
				let existing_key = NibbleSlice::from_stored(&encoded);
				let cp = partial.common_prefix(&existing_key);
				if cp == existing_key.len() && cp == partial.len() {
					trace!(target: "trie", "equivalent-leaf: REPLACE");
					// equivalent leaf.
					let unchanged = stored_value == value;
					*old_val = Some(stored_value);

					match unchanged {
						// unchanged. restore
						true => InsertAction::Restore(Node::Leaf(encoded.clone(), value)),
						false => InsertAction::Replace(Node::Leaf(encoded.clone(), value)),
					}
				} else if (L::USE_EXTENSION && cp == 0)
					|| (!L::USE_EXTENSION && cp < existing_key.len()) {
					trace!(target: "trie", "lesser-common-prefix, not-both-empty (exist={:?}; new={:?}): TRANSMUTE,AUGMENT", existing_key.len(), partial.len());

					// one of us isn't empty: transmute to branch here
					let mut children = empty_children();
					let branch = if L::USE_EXTENSION && existing_key.is_empty() {
						// always replace since branch isn't leaf.
						Node::Branch(children, Some(stored_value))
					} else {
						let idx = existing_key.at(cp) as usize;
						let new_leaf = Node::Leaf(existing_key.mid(cp + 1).to_stored(), stored_value);
						children[idx] = Some(self.storage.alloc(Stored::New(new_leaf)).into());

						if L::USE_EXTENSION {
							Node::Branch(children, None)
						} else {
							Node::NibbledBranch(partial.to_stored_range(cp), children, None)
						}
					};

					// always replace because whatever we get out here is not the branch we started with.
					let branch_action = self.insert_inspector(branch, key, value, old_val)?.unwrap_node();
					InsertAction::Replace(branch_action)
				} else if !L::USE_EXTENSION {
					trace!(target: "trie", "complete-prefix (cp={:?}): AUGMENT-AT-END", cp);

					// fully-shared prefix for an extension.
					// make a stub branch
					let branch = Node::NibbledBranch(existing_key.to_stored(), empty_children(), Some(stored_value));
					// augment the new branch.
					let branch = self.insert_inspector(branch, key, value, old_val)?.unwrap_node();

					InsertAction::Replace(branch)

				} else if cp == existing_key.len() {
					debug_assert!(L::USE_EXTENSION);
					trace!(target: "trie", "complete-prefix (cp={:?}): AUGMENT-AT-END", cp);

					// fully-shared prefix for an extension.
					// make a stub branch and an extension.
					let branch = Node::Branch(empty_children(), Some(stored_value));
					// augment the new branch.
					key.advance(cp);
					let branch = self.insert_inspector(branch, key, value, old_val)?.unwrap_node();

					// always replace since we took a leaf and made an extension.
					let branch_handle = self.storage.alloc(Stored::New(branch)).into();
					InsertAction::Replace(Node::Extension(existing_key.to_stored(), branch_handle))
				} else {
					debug_assert!(L::USE_EXTENSION);
					trace!(target: "trie", "partially-shared-prefix (exist={:?}; new={:?}; cp={:?}): AUGMENT-AT-END", existing_key.len(), partial.len(), cp);

					// partially-shared prefix for an extension.
					// start by making a leaf.
					let low = Node::Leaf(existing_key.mid(cp).to_stored(), stored_value);

					// augment it. this will result in the Leaf -> cp == 0 routine,
					// which creates a branch.
					key.advance(cp);
					let augmented_low = self.insert_inspector(low, key, value, old_val)?.unwrap_node();
					// make an extension using it. this is a replacement.
					InsertAction::Replace(Node::Extension(
						existing_key.to_stored_range(cp),
						self.storage.alloc(Stored::New(augmented_low)).into()
					))
				}
			},
			Node::Extension(encoded, child_branch) => {
				debug_assert!(L::USE_EXTENSION);
				let existing_key = NibbleSlice::from_stored(&encoded);
				let cp = partial.common_prefix(&existing_key);
				if cp == 0 {
					trace!(target: "trie", "no-common-prefix, not-both-empty (exist={:?}; new={:?}): TRANSMUTE,AUGMENT", existing_key.len(), partial.len());

					// partial isn't empty: make a branch here
					// extensions may not have empty partial keys.
					assert!(!existing_key.is_empty());
					let idx = existing_key.at(0) as usize;

					let mut children = empty_children();
					children[idx] = if existing_key.len() == 1 {
						// direct extension, just replace.
						Some(child_branch)
					} else {
						// more work required after branching.
						let ext = Node::Extension(existing_key.mid(1).to_stored(), child_branch);
						Some(self.storage.alloc(Stored::New(ext)).into())
					};

					// continue inserting.
					let branch_action = self.insert_inspector(Node::Branch(children, None), key, value, old_val)?.unwrap_node();
					InsertAction::Replace(branch_action)
				} else if cp == existing_key.len() {
					trace!(target: "trie", "complete-prefix (cp={:?}): AUGMENT-AT-END", cp);

					// fully-shared prefix.

					// insert into the child node.
					key.advance(cp);
					let (new_child, changed) = self.insert_at(child_branch, key, value, old_val)?;
					let new_ext = Node::Extension(existing_key.to_stored(), new_child.into());

					// if the child branch wasn't changed, meaning this extension remains the same.
					match changed {
						true => InsertAction::Replace(new_ext),
						false => InsertAction::Restore(new_ext),
					}
				} else {
					trace!(target: "trie", "partially-shared-prefix (exist={:?}; new={:?}; cp={:?}): AUGMENT-AT-END", existing_key.len(), partial.len(), cp);

					// partially-shared.
					let low = Node::Extension(existing_key.mid(cp).to_stored(), child_branch);
					// augment the extension. this will take the cp == 0 path, creating a branch.
					key.advance(cp);
					let augmented_low = self.insert_inspector(low, key, value, old_val)?.unwrap_node();

					// always replace, since this extension is not the one we started with.
					// this is known because the partial key is only the common prefix.
					InsertAction::Replace(Node::Extension(
						existing_key.to_stored_range(cp),
						self.storage.alloc(Stored::New(augmented_low)).into()
					))
				}
			},
		})
	}

	/// Remove a node from the trie based on key.
	fn remove_at(&mut self, handle: NodeHandle<TrieHash<L>>, key: &mut PartialKey<L::N>, old_val: &mut Option<DBValue>) -> Result<Option<(StorageHandle, bool)>, TrieHash<L>, CError<L>> {
		let stored = match handle {
			NodeHandle::InMemory(h) => self.storage.destroy(h),
			NodeHandle::Hash(h) => {
				let handle = self.cache(h, key.encoded_prefix())?;
				self.storage.destroy(handle)
			}
		};

		let opt = self.inspect(stored, key, move |trie, node, key| trie.remove_inspector(node, key, old_val))?;

		Ok(opt.map(|(new, changed)| (self.storage.alloc(new), changed)))
	}

	/// the removal inspector
	fn remove_inspector(&mut self, node: Node<TrieHash<L>>, key: &mut PartialKey<L::N>, old_val: &mut Option<DBValue>) -> Result<Action<TrieHash<L>>, TrieHash<L>, CError<L>> {
		let partial = key.mid();
		Ok(match (node, partial.is_empty()) {
			(Node::Empty, _) => Action::Delete,
			(Node::Branch(c, None), true) => Action::Restore(Node::Branch(c, None)),
			(Node::NibbledBranch(n, c, None), true) => Action::Restore(Node::NibbledBranch(n, c, None)),
			(Node::Branch(children, Some(val)), true) => {
        println!("beffix");
				*old_val = Some(val);
				// always replace since we took the value out.
				Action::Replace(self.fix(Node::Branch(children, None), key.clone())?)
			},
			(Node::NibbledBranch(n, children, Some(val)), true) => {
				*old_val = Some(val);
				// always replace since we took the value out.
				Action::Replace(self.fix(Node::NibbledBranch(n, children, None), key.clone())?)
			},
			(Node::Branch(mut children, value), false) => {
				let idx = partial.at(0) as usize;
				if let Some(child) = children[idx].take() {
					trace!(target: "trie", "removing value out of branch child, partial={:?}", partial);
					let prefix = key.clone();
					key.advance(1);
					match self.remove_at(child, key, old_val)? {
						Some((new, changed)) => {
							children[idx] = Some(new.into());
							let branch = Node::Branch(children, value);
							match changed {
								// child was changed, so we were too.
								true => Action::Replace(branch),
								// unchanged, so we are too.
								false => Action::Restore(branch),
							}
						}
						None => {
        println!("beffixrigh");
							// the child we took was deleted.
							// the node may need fixing.
							trace!(target: "trie", "branch child deleted, partial={:?}", partial);
							Action::Replace(self.fix(Node::Branch(children, value), prefix)?)
						}
					}
				} else {
					// no change needed.
					Action::Restore(Node::Branch(children, value))
				}
			},
			(Node::NibbledBranch(encoded, mut children, value), false) => {
				let (cp, existing_len) = {
					let existing_key = NibbleSlice::from_stored(&encoded);
					(existing_key.common_prefix(&partial), existing_key.len())
				};
				if cp == existing_len && cp == partial.len() {

					// replace val
					if let Some(val) = value {
						*old_val = Some(val);

						let f = self.fix(Node::NibbledBranch(encoded, children, None), key.clone(), None);
						Action::Replace(f?)
					} else {
						Action::Restore(Node::NibbledBranch(encoded, children, None))
					}
				} else if cp < existing_len {
					// partway through an extension -- nothing to do here.
					Action::Restore(Node::NibbledBranch(encoded, children, value))
				} else {
					// cp == existing_len && cp < partial.len() : check children
					let idx = partial.at(cp) as usize;

					if let Some(child) = children[idx].take() {
						trace!(target: "trie", "removing value out of branch child, partial={:?}", partial);
						let prefix = key.clone();
						key.advance(cp + 1);
						match self.remove_at(child, key, old_val)? {
							Some((new, changed)) => {
								children[idx] = Some(new.into());
								let branch = Node::NibbledBranch(encoded, children, value);
								match changed {
									// child was changed, so we were too.
									true => Action::Replace(branch),
									// unchanged, so we are too.
									false => Action::Restore(branch),
								}
							},
							None => {
								// the child we took was deleted.
								// the node may need fixing.
								trace!(target: "trie", "branch child deleted, partial={:?}", partial);
								Action::Replace(self.fix(Node::NibbledBranch(encoded, children, value), prefix)?)
							},
						}
					} else {
						// no change needed.
						Action::Restore(Node::NibbledBranch(encoded, children, value))
					}
				}
			},
			(Node::Leaf(encoded, value), _) => {
				if NibbleSlice::from_stored(&encoded) == partial {
					// this is the node we were looking for. Let's delete it.
					*old_val = Some(value);
					Action::Delete
				} else {
					// leaf the node alone.
					trace!(target: "trie", "restoring leaf wrong partial, partial={:?}, existing={:?}", partial, NibbleSlice::<L::N>::from_stored(&encoded));
					Action::Restore(Node::Leaf(encoded, value))
				}
			},
			(Node::Extension(encoded, child_branch), _) => {
				let (cp, existing_len) = {
					let existing_key = NibbleSlice::from_stored(&encoded);
					(existing_key.common_prefix(&partial), existing_key.len())
				};
				if cp == existing_len {
					// try to remove from the child branch.
					trace!(target: "trie", "removing from extension child, partial={:?}", partial);
					let prefix = key.clone();
					key.advance(cp);
					match self.remove_at(child_branch, key, old_val)? {
						Some((new_child, changed)) => {
							let new_child = new_child.into();

							// if the child branch was unchanged, then the extension is too.
							// otherwise, this extension may need fixing.
							match changed {
								true => Action::Replace(self.fix(Node::Extension(encoded, new_child), prefix)?),
								false => Action::Restore(Node::Extension(encoded, new_child)),
							}
						}
						None => {
							// the whole branch got deleted.
							// that means that this extension is useless.
							Action::Delete
						}
					}
				} else {
					// partway through an extension -- nothing to do here.
					Action::Restore(Node::Extension(encoded, child_branch))
				}
			},
		})
	}

	/// Given a node which may be in an _invalid state_, fix it such that it is then in a valid
	/// state.
	///
	/// _invalid state_ means:
	/// - Branch node where there is only a single entry;
	/// - Extension node followed by anything other than a Branch node.
	fn fix(&mut self, node: Node<TrieHash<L>>, key: PartialKey<L::N>) -> Result<Node<TrieHash<L>>, TrieHash<L>, CError<L>> {
		match node {
			Node::Branch(mut children, value) => {
				// if only a single value, transmute to leaf/extension and feed through fixed.
				#[derive(Debug)]
				enum UsedIndex {
					None,
					One(u8),
					Many,
				};
				let mut used_index = UsedIndex::None;
				for i in 0..16 {
					match (children[i].is_none(), &used_index) {
						(false, &UsedIndex::None) => used_index = UsedIndex::One(i as u8),
						(false, &UsedIndex::One(_)) => {
							used_index = UsedIndex::Many;
							break;
						}
						_ => continue,
					}
				}

				match (used_index, value) {
					(UsedIndex::None, None) => panic!("Branch with no subvalues. Something went wrong."),
					(UsedIndex::One(a), None) => {
						// only one onward node. make an extension.
						let new_partial = NibbleSlice::<L::N>::new_offset(&[a], 1).to_stored();
						let child = children[a as usize].take().expect("used_index only set if occupied; qed");
						let new_node = Node::Extension(new_partial, child);
						self.fix(new_node, key)
					}
					(UsedIndex::None, Some(value)) => {
						// make a leaf.
						trace!(target: "trie", "fixing: branch -> leaf");
						Ok(Node::Leaf(NibbleSlice::<L::N>::new(&[]).to_stored(), value))
					}
					(_, value) => {
						// all is well.
						trace!(target: "trie", "fixing: restoring branch");
						Ok(Node::Branch(children, value))
					}
				}
			},
			Node::NibbledBranch(enc_nibble, mut children, value) => {
				// if only a single value, transmute to leaf/extension and feed through fixed.
				#[derive(Debug)]
				enum UsedIndex {
					None,
					One(u8),
					Many,
				};
				let mut used_index = UsedIndex::None;
				for i in 0..16 {
					match (children[i].is_none(), &used_index) {
						(false, &UsedIndex::None) => used_index = UsedIndex::One(i as u8),
						(false, &UsedIndex::One(_)) => {
							used_index = UsedIndex::Many;
							break;
						}
						_ => continue,
					}
				}

				match (used_index, value) {
					(UsedIndex::None, None) => panic!("Branch with no subvalues. Something went wrong."),
					(UsedIndex::One(a), None) => {
            // TODO EMCH can simplify code by transforming to an extension like in branch
            // (extension only temp value before being transformed).
						// only one onward node. use child instead
						let child = children[a as usize].take().expect("used_index only set if occupied; qed");
            let mut kc = key.clone();
            let b_slice_len = (enc_nibble.1.len() * L::N::NIBBLE_PER_BYTE) -	enc_nibble.0;
						kc.advance(b_slice_len);
						let stored = match child {
							NodeHandle::InMemory(h) => self.storage.destroy(h),
							NodeHandle::Hash(h) => {
                // TODO NX !!! from this key advenac we are out of key -> can do a
                // paded_encoded_prefix for one insert - just a nibble ta add (can reuse buff)
								let handle = self.cache(h, buf.end().left())?;
                buf.pop();
								self.storage.destroy(handle)
							}
						};
						let child_node = match stored {
							Stored::New(node) => node,
							Stored::Cached(node, hash) => {
								self.death_row.insert((hash, key.encoded_prefix_owned()));
								node
							},
						};
						match child_node {
							Node::Leaf(sub_partial, value) => {
                // TODO NX!!!
                // TODO a + buf + ck
								Ok(Node::Leaf(buf.to_stored(s + 1 + b_slice_len), value))
							},
							Node::NibbledBranch(sub_partial, ch_children, ch_value) => {
                // TODO NX!!!
                // TODO a + buf + ck
								Ok(Node::NibbledBranch(buf.to_stored(s + 1 + b_slice_len), ch_children, ch_value))
							},
							_ => unreachable!(),
						}
					},
					(UsedIndex::None, Some(value)) => {
						// make a leaf.
						trace!(target: "trie", "fixing: branch -> leaf");
						Ok(Node::Leaf(enc_nibble, value))
					},
					(_, value) => {
						// all is well.
						trace!(target: "trie", "fixing: restoring branch");
						Ok(Node::NibbledBranch(enc_nibble, children, value))
					},
				}
			},
			Node::Extension(partial, child) => {
				let stored = match child {
					NodeHandle::InMemory(h) => self.storage.destroy(h),
					NodeHandle::Hash(h) => {
            // TODO NX!! here to 
						let handle = self.cache(h, kc.encoded_prefix())?;
						self.storage.destroy(handle)
					}
				};

				let (child_node, maybe_hash) = match stored {
					Stored::New(node) => (node, None),
					Stored::Cached(node, hash) => (node, Some(hash))
				};

				match child_node {
					Node::Extension(sub_partial, sub_child) => {
						// combine with node below.
						if let Some(hash) = maybe_hash {
							// delete the cached child since we are going to replace it.
							self.death_row.insert((hash, key.encoded_prefix_owned()));
						}
            // TODO NX!!
            // subpartial
						trace!(target: "trie", "fixing: extension combination. new_partial={:?}", kc.mid().to_stored());
						self.fix(Node::Extension(buf.to_stored(s + ex_slice_len), sub_child), key)
					}
					Node::Leaf(sub_partial, value) => {
            println!("leaf combine {:x?}", key);
						// combine with node below.
						if let Some(hash) = maybe_hash {
							// delete the cached child since we are going to replace it.
							self.death_row.insert((hash, key.encoded_prefix_owned()));
						}
            // TODO NX!!
            // subpartial oly
						trace!(target: "trie", "fixing: extension -> leaf. new_partial={:?}", kc.mid());
						println!("fixing: extension -> leaf. new_partial={:?}", kc.mid());
						Ok(Node::Leaf(buf.to_stored(s + ex_slice_len), value))
					}
					child_node => {
						trace!(target: "trie", "fixing: restoring extension");

						// reallocate the child node.
						let stored = if let Some(hash) = maybe_hash {
							Stored::Cached(child_node, hash)
						} else {
							Stored::New(child_node)
						};

						Ok(Node::Extension(partial, self.storage.alloc(stored).into()))
					}
				}
			},
			other => Ok(other), // only ext and branch need fixing.
		}
	}

	/// Commit the in-memory changes to disk, freeing their storage and
	/// updating the state root.
	pub fn commit(&mut self) {
		trace!(target: "trie", "Committing trie changes to db.");

		// always kill all the nodes on death row.
		trace!(target: "trie", "{:?} nodes to remove from db", self.death_row.len());
		for (hash, prefix) in self.death_row.drain() {
			self.db.remove(&hash, (&prefix.0[..], prefix.1));
		}

		let handle = match self.root_handle() {
			NodeHandle::Hash(_) => return, // no changes necessary.
			NodeHandle::InMemory(h) => h,
		};

		match self.storage.destroy(handle) {
			Stored::New(node) => {
				let mut k = PartialKeyMut::new();
				let encoded_root = node.into_encoded::<_, L::C, L::H, L::N>(|child, o_sl, o_ix| {
					let mov = concat_key(&mut k, o_sl, o_ix);
					let cr = self.commit_child(child, &mut k);
					k.truncate(mov);
					cr
				});
				trace!(target: "trie", "encoded root node: {:#x?}", &encoded_root[..]);
				*self.root = self.db.insert(nibbleslice::EMPTY_ENCODED, &encoded_root[..]);
				self.hash_count += 1;

				self.root_handle = NodeHandle::Hash(*self.root);
			}
			Stored::Cached(node, hash) => {
				// probably won't happen, but update the root and move on.
				*self.root = hash;
				self.root_handle = NodeHandle::InMemory(self.storage.alloc(Stored::Cached(node, hash)));
			}
		}
	}

	/// Commit a node by hashing it and writing it to the db. Returns a
	/// `ChildReference` which in most cases carries a normal hash but for the
	/// case where we can fit the actual data in the `Hasher`s output type, we
	/// store the data inline. This function is used as the callback to the
	/// `into_encoded` method of `Node`.
	fn commit_child(&mut self, handle: NodeHandle<TrieHash<L>>, prefix: &mut PartialKeyMut<L::N>) -> ChildReference<TrieHash<L>> {
		match handle {
			NodeHandle::Hash(hash) => ChildReference::Hash(hash),
			NodeHandle::InMemory(storage_handle) => {
				match self.storage.destroy(storage_handle) {
					Stored::Cached(_, hash) => ChildReference::Hash(hash),
					Stored::New(node) => {
						let encoded = {
							let commit_child = |node_handle, o_sl: Option<&NibbleSlice<L::N>>, o_ix: Option<u8>| {
								let mov = concat_key(prefix, o_sl, o_ix);
								let cr = self.commit_child(node_handle, prefix);
								prefix.truncate(mov);
								cr
							};
							node.into_encoded::<_, L::C, L::H, L::N>(commit_child)
						};
						if encoded.len() >= L::H::LENGTH {
							let hash = self.db.insert(prefix.end().left(), &encoded[..]);
							self.hash_count +=1;
							ChildReference::Hash(hash)
						} else {
							// it's a small value, so we cram it into a `TrieHash<L>` and tag with length
							let mut h = <TrieHash<L>>::default();
							let len = encoded.len();
							h.as_mut()[..len].copy_from_slice(&encoded[..len]);
							ChildReference::Inline(h, len)
						}
					}
				}
			}
		}
	}

	// a hack to get the root node's handle
	fn root_handle(&self) -> NodeHandle<TrieHash<L>> {
		match self.root_handle {
			NodeHandle::Hash(h) => NodeHandle::Hash(h),
			NodeHandle::InMemory(StorageHandle(x)) => NodeHandle::InMemory(StorageHandle(x)),
		}
	}
}


// TODO EMCH change usage here to run on self buffer
// TODO EMCH a with_concat_key function using a closure and truncating correctly
pub(crate) fn concat_key<N: NibbleOps>(prefix: &mut PartialKeyMut<N>, o_sl: Option<&NibbleSlice<N>>, o_ix: Option<u8>) -> usize {
	let mut res = 0;
	if let Some(sl) = o_sl { 
		// TODO EMCH align optim
		for n in sl.iter() {
			prefix.push(n);
		}
		res += sl.len();
	}
	if let Some(ix) = o_ix { 
		prefix.push(ix);
		res += 1;
	}
	res
}

pub(crate) fn concat_key_clone<N: NibbleOps>(prefix: &PartialKeyMut<N>, o_sl: Option<&NibbleSlice<N>>, o_ix: Option<u8>) -> PartialKeyMut<N> {
  let mut p = prefix.clone();
  concat_key(&mut p, o_sl, o_ix);
  p
}

impl<'a, L> TrieMut<L> for TrieDBMut<'a, L>
where
	L: TrieLayOut,
{
	fn root(&mut self) -> &TrieHash<L> {
		self.commit();
		self.root
	}

	fn is_empty(&self) -> bool {
		match self.root_handle {
			NodeHandle::Hash(h) => h == L::C::hashed_null_node(),
			NodeHandle::InMemory(ref h) => match self.storage[h] {
				Node::Empty => true,
				_ => false,
			}
		}
	}

	fn get<'x, 'key>(&'x self, key: &'key [u8]) -> Result<Option<DBValue>, TrieHash<L>, CError<L>>
		where 'x: 'key
	{
		self.lookup(NibbleSlice::new(key), &self.root_handle)
	}

	fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<Option<DBValue>, TrieHash<L>, CError<L>> {
		if value.is_empty() { return self.remove(key) }

		let mut old_val = None;

		trace!(target: "trie", "insert: key={:#x?}, value={:#x?}", key, value);

		let root_handle = self.root_handle();
		let (new_handle, changed) = self.insert_at(
			root_handle,
			&mut PartialKey::new(NibbleSlice::new(key)),
			DBValue::from_slice(value),
			&mut old_val,
		)?;

		trace!(target: "trie", "insert: altered trie={}", changed);
		self.root_handle = NodeHandle::InMemory(new_handle);

		Ok(old_val)
	}

	fn remove(&mut self, key: &[u8]) -> Result<Option<DBValue>, TrieHash<L>, CError<L>> {
		trace!(target: "trie", "remove: key={:#x?}", key);

		let root_handle = self.root_handle();
		let mut key = PartialKey::new(NibbleSlice::new(key));
		let mut old_val = None;

		match self.remove_at(root_handle, &mut key, &mut old_val)? {
			Some((handle, changed)) => {
				trace!(target: "trie", "remove: altered trie={}", changed);
				self.root_handle = NodeHandle::InMemory(handle);
			}
			None => {
				trace!(target: "trie", "remove: obliterated trie");
				self.root_handle = NodeHandle::Hash(L::C::hashed_null_node());
				*self.root = L::C::hashed_null_node();
			}
		}

		Ok(old_val)
	}
}

impl<'a, L> Drop for TrieDBMut<'a, L>
where
	L: TrieLayOut,
{
	fn drop(&mut self) {
		self.commit();
	}
}

#[cfg(test)]
mod tests {
	use env_logger;
	use standardmap::*;
	use DBValue;
	use memory_db::{MemoryDB, PrefixedKey, HashKey};
	use hash_db::{Hasher, HashDB};
	use keccak_hasher::KeccakHasher;
	use reference_trie::{RefTrieDBMutNoExt, RefTrieDBMut, TrieMut, TrieLayOut, NodeCodec,
		ReferenceNodeCodec, ref_trie_root, RefTrieDB, RefTrieDBNoExt, LayoutOri};

	fn populate_trie<'db>(
		db: &'db mut HashDB<KeccakHasher, DBValue>,
		root: &'db mut <KeccakHasher as Hasher>::Out,
		v: &[(Vec<u8>, Vec<u8>)]
	) -> RefTrieDBMut<'db> {
		let mut t = RefTrieDBMut::new(db, root);
		for i in 0..v.len() {
			let key: &[u8]= &v[i].0;
			let val: &[u8] = &v[i].1;
			t.insert(key, val).unwrap();
		}
		t
	}

	fn unpopulate_trie<'db>(t: &mut RefTrieDBMut<'db>, v: &[(Vec<u8>, Vec<u8>)]) {
		for i in v {
			let key: &[u8]= &i.0;
			t.remove(key).unwrap();
		}
	}

	fn populate_trie_no_ext<'db>(
		db: &'db mut HashDB<KeccakHasher, DBValue>,
		root: &'db mut <KeccakHasher as Hasher>::Out,
		v: &[(Vec<u8>, Vec<u8>)]
	) -> RefTrieDBMutNoExt<'db> {
		let mut t = RefTrieDBMutNoExt::new(db, root);
		for i in 0..v.len() {
			let key: &[u8]= &v[i].0;
			let val: &[u8] = &v[i].1;
			t.insert(key, val).unwrap();
		}
		t
	}

	fn unpopulate_trie_no_ext<'db>(t: &mut RefTrieDBMutNoExt<'db>, v: &[(Vec<u8>, Vec<u8>)]) {
		for i in v {
			let key: &[u8]= &i.0;
			t.remove(key).unwrap();
		}
	}


	#[test]
	fn playpen() {
		env_logger::init();
		let mut seed = Default::default();
		for test_i in 0..10 {
			if test_i % 50 == 0 {
				debug!("{:?} of 10000 stress tests done", test_i);
			}
			let x = StandardMap {
				alphabet: Alphabet::Custom(b"@QWERTYUIOPASDFGHJKLZXCVBNM[/]^_".to_vec()),
				min_key: 5,
				journal_key: 0,
				value_mode: ValueMode::Index,
				count: 100,
			}.make_with(&mut seed);

			let real = ref_trie_root(x.clone());
			let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
			let mut root = Default::default();
			let mut memtrie = populate_trie(&mut memdb, &mut root, &x);

			memtrie.commit();
			if *memtrie.root() != real {
				println!("TRIE MISMATCH");
				println!("");
				println!("{:?} vs {:?}", memtrie.root(), real);
				for i in &x {
					println!("{:#x?} -> {:#x?}", i.0, i.1);
				}
			}
			assert_eq!(*memtrie.root(), real);
			unpopulate_trie(&mut memtrie, &x);
			memtrie.commit();
			let hashed_null_node = <ReferenceNodeCodec as NodeCodec<_, <LayoutOri as TrieLayOut>::N>>::hashed_null_node();
			if *memtrie.root() != hashed_null_node {
				println!("- TRIE MISMATCH");
				println!("");
				println!("{:#x?} vs {:#x?}", memtrie.root(), hashed_null_node);
				for i in &x {
					println!("{:#x?} -> {:#x?}", i.0, i.1);
				}
			}
			assert_eq!(*memtrie.root(), hashed_null_node);
		}
	}

	#[test]
	fn init() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		let hashed_null_node = <ReferenceNodeCodec as NodeCodec<_, <LayoutOri as TrieLayOut>::N>>::hashed_null_node();
		assert_eq!(*t.root(), hashed_null_node);
	}

	#[test]
	fn insert_on_empty() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![ (vec![0x01u8, 0x23], vec![0x01u8, 0x23]) ]));
	}

	#[test]
	fn remove_to_empty() {
		let big_value = b"00000000000000000000000000000000";

		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t1 = RefTrieDBMut::new(&mut memdb, &mut root);
		t1.insert(&[0x01, 0x23], big_value).unwrap();
		t1.insert(&[0x01, 0x34], big_value).unwrap();
		let mut memdb2 = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root2 = Default::default();
		let mut t2 = RefTrieDBMut::new(&mut memdb2, &mut root2);

		t2.insert(&[0x01], big_value).unwrap();
		t2.insert(&[0x01, 0x23], big_value).unwrap();
		t2.insert(&[0x01, 0x34], big_value).unwrap();
		t2.remove(&[0x01]).unwrap();
	}

	#[test]
	fn remove_to_empty_no_ext() {
		let big_value = b"00000000000000000000000000000000";
		let big_value2 = b"00000000000000000000000000000002";
		let big_value3 = b"00000000000000000000000000000004";

		let mut memdb2 = MemoryDB::<_,PrefixedKey<_>,_>::default();
		let mut root2 = Default::default();
		{
			let mut memdb = MemoryDB::<_, PrefixedKey<_>, _>::default();
			let mut root = Default::default();
			let mut t1 = RefTrieDBMutNoExt::new(&mut memdb, &mut root);
			//t1.insert(&[0x01, 0x23], big_value).unwrap();
			//t1.insert(&[0x01, 0x34], big_value).unwrap();
			let mut t2 = RefTrieDBMutNoExt::new(&mut memdb2, &mut root2);

			t2.insert(&[0x01, 0x23], big_value3).unwrap();
			t2.insert(&[0x01], big_value2).unwrap();
			t2.insert(&[0x01, 0x34], big_value).unwrap();
			t2.remove(&[0x01]).unwrap();
			// commit on drop
		}
		let t2 = RefTrieDBNoExt::new(& memdb2, &root2); 
		assert_eq!(&root2[..], &reference_trie::calc_root_no_ext(vec![
		 (vec![0x01u8, 0x23], big_value3.to_vec()),
		 (vec![0x01u8, 0x34], big_value.to_vec()),
		])[..]);
	}


	#[test]
	fn insert_replace_root() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		t.insert(&[0x01u8, 0x23], &[0x23u8, 0x45]).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![ (vec![0x01u8, 0x23], vec![0x23u8, 0x45]) ]));
	}

	#[test]
	fn insert_make_branch_root() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		t.insert(&[0x11u8, 0x23], &[0x11u8, 0x23]).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![
			(vec![0x01u8, 0x23], vec![0x01u8, 0x23]),
			(vec![0x11u8, 0x23], vec![0x11u8, 0x23])
		]));
	}

	#[test]
	fn insert_into_branch_root() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		t.insert(&[0xf1u8, 0x23], &[0xf1u8, 0x23]).unwrap();
		t.insert(&[0x81u8, 0x23], &[0x81u8, 0x23]).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![
			(vec![0x01u8, 0x23], vec![0x01u8, 0x23]),
			(vec![0x81u8, 0x23], vec![0x81u8, 0x23]),
			(vec![0xf1u8, 0x23], vec![0xf1u8, 0x23]),
		]));
	}

	#[test]
	fn insert_value_into_branch_root() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		t.insert(&[], &[0x0]).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![
			(vec![], vec![0x0]),
			(vec![0x01u8, 0x23], vec![0x01u8, 0x23]),
		]));
	}

	#[test]
	fn insert_split_leaf() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		t.insert(&[0x01u8, 0x34], &[0x01u8, 0x34]).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![
			(vec![0x01u8, 0x23], vec![0x01u8, 0x23]),
			(vec![0x01u8, 0x34], vec![0x01u8, 0x34]),
		]));
	}

	#[test]
	fn insert_split_extenstion() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01, 0x23, 0x45], &[0x01]).unwrap();
		t.insert(&[0x01, 0xf3, 0x45], &[0x02]).unwrap();
		t.insert(&[0x01, 0xf3, 0xf5], &[0x03]).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![
			(vec![0x01, 0x23, 0x45], vec![0x01]),
			(vec![0x01, 0xf3, 0x45], vec![0x02]),
			(vec![0x01, 0xf3, 0xf5], vec![0x03]),
		]));
	}

	#[test]
	fn insert_big_value() {
		let big_value0 = b"00000000000000000000000000000000";
		let big_value1 = b"11111111111111111111111111111111";

		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], big_value0).unwrap();
		t.insert(&[0x11u8, 0x23], big_value1).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![
			(vec![0x01u8, 0x23], big_value0.to_vec()),
			(vec![0x11u8, 0x23], big_value1.to_vec())
		]));
	}

	#[test]
	fn insert_duplicate_value() {
		let big_value = b"00000000000000000000000000000000";

		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], big_value).unwrap();
		t.insert(&[0x11u8, 0x23], big_value).unwrap();
		assert_eq!(*t.root(), ref_trie_root(vec![
			(vec![0x01u8, 0x23], big_value.to_vec()),
			(vec![0x11u8, 0x23], big_value.to_vec())
		]));
	}

	#[test]
	fn test_at_empty() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let t = RefTrieDBMut::new(&mut memdb, &mut root);
		assert_eq!(t.get(&[0x5]).unwrap(), None);
	}

	#[test]
	fn test_at_one() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		assert_eq!(t.get(&[0x1, 0x23]).unwrap().unwrap(), DBValue::from_slice(&[0x1u8, 0x23]));
		t.commit();
		assert_eq!(t.get(&[0x1, 0x23]).unwrap().unwrap(), DBValue::from_slice(&[0x1u8, 0x23]));
	}

	#[test]
	fn test_at_three() {
		let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
		t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		t.insert(&[0xf1u8, 0x23], &[0xf1u8, 0x23]).unwrap();
		t.insert(&[0x81u8, 0x23], &[0x81u8, 0x23]).unwrap();
		assert_eq!(t.get(&[0x01, 0x23]).unwrap().unwrap(), DBValue::from_slice(&[0x01u8, 0x23]));
		assert_eq!(t.get(&[0xf1, 0x23]).unwrap().unwrap(), DBValue::from_slice(&[0xf1u8, 0x23]));
		assert_eq!(t.get(&[0x81, 0x23]).unwrap().unwrap(), DBValue::from_slice(&[0x81u8, 0x23]));
		assert_eq!(t.get(&[0x82, 0x23]).unwrap(), None);
		t.commit();
		assert_eq!(t.get(&[0x01, 0x23]).unwrap().unwrap(), DBValue::from_slice(&[0x01u8, 0x23]));
		assert_eq!(t.get(&[0xf1, 0x23]).unwrap().unwrap(), DBValue::from_slice(&[0xf1u8, 0x23]));
		assert_eq!(t.get(&[0x81, 0x23]).unwrap().unwrap(), DBValue::from_slice(&[0x81u8, 0x23]));
		assert_eq!(t.get(&[0x82, 0x23]).unwrap(), None);
	}

	#[test]
	fn stress() {
		let mut seed = Default::default();
		for _ in 0..50 {
			let x = StandardMap {
				alphabet: Alphabet::Custom(b"@QWERTYUIOPASDFGHJKLZXCVBNM[/]^_".to_vec()),
				min_key: 5,
				journal_key: 0,
				value_mode: ValueMode::Index,
				count: 4,
			}.make_with(&mut seed);

			let real = ref_trie_root(x.clone());
			let mut memdb = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
			let mut root = Default::default();
			let mut memtrie = populate_trie(&mut memdb, &mut root, &x);
			let mut y = x.clone();
			y.sort_by(|ref a, ref b| a.0.cmp(&b.0));
			let mut memdb2 = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
			let mut root2 = Default::default();
			let mut memtrie_sorted = populate_trie(&mut memdb2, &mut root2, &y);
			if *memtrie.root() != real || *memtrie_sorted.root() != real {
				println!("TRIE MISMATCH");
				println!("");
				println!("ORIGINAL... {:#x?}", memtrie.root());
				for i in &x {
					println!("{:#x?} -> {:#x?}", i.0, i.1);
				}
				println!("SORTED... {:#x?}", memtrie_sorted.root());
				for i in &y {
					println!("{:#x?} -> {:#x?}", i.0, i.1);
				}
			}
			assert_eq!(*memtrie.root(), real);
			assert_eq!(*memtrie_sorted.root(), real);
		}
	}

	#[test]
	fn test_trie_existing() {
		let mut db = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		{
			let mut t = RefTrieDBMut::new(&mut db, &mut root);
			t.insert(&[0x01u8, 0x23], &[0x01u8, 0x23]).unwrap();
		}

		{
			 let _ = RefTrieDBMut::from_existing(&mut db, &mut root);
		}
	}

	#[test]
	fn insert_empty() {
		let mut seed = Default::default();
		let x = StandardMap {
				alphabet: Alphabet::Custom(b"@QWERTYUIOPASDFGHJKLZXCVBNM[/]^_".to_vec()),
				min_key: 5,
				journal_key: 0,
				value_mode: ValueMode::Index,
				count: 4,
		}.make_with(&mut seed);

		let mut db = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
		let mut t = RefTrieDBMut::new(&mut db, &mut root);
		for &(ref key, ref value) in &x {
			t.insert(key, value).unwrap();
		}

		assert_eq!(*t.root(), ref_trie_root(x.clone()));

		for &(ref key, _) in &x {
			t.insert(key, &[]).unwrap();
		}

		assert!(t.is_empty());
		let hashed_null_node = <ReferenceNodeCodec as NodeCodec<_, <LayoutOri as TrieLayOut>::N>>::hashed_null_node();
		assert_eq!(*t.root(), hashed_null_node);
	}

	#[test]
	fn return_old_values() {
		let mut seed = Default::default();
		let x = StandardMap {
				alphabet: Alphabet::Custom(b"@QWERTYUIOPASDFGHJKLZXCVBNM[/]^_".to_vec()),
				min_key: 5,
				journal_key: 0,
				value_mode: ValueMode::Index,
				count: 2,
		}.make_with(&mut seed);

		let mut db = MemoryDB::<KeccakHasher, PrefixedKey<_>, DBValue>::default();
		let mut root = Default::default();
    {
		let mut t = RefTrieDBMut::new(&mut db, &mut root);
		for &(ref key, ref value) in &x {
      println!("k{:x?}",key);
			assert!(t.insert(key, value).unwrap().is_none());
			assert_eq!(t.insert(key, value).unwrap(), Some(DBValue::from_slice(value)));
		}

			t.remove(&x.iter().next().unwrap().0).unwrap();
    }
		let t = RefTrieDB::new(&db, &root);
    println!("{:?}", t);
    panic!("yo");
/*    let mut i = 0;
		for (key, value) in x {
      println!("rem{}",i);
      i+=1;
			assert_eq!(t.remove(&key).unwrap(), Some(DBValue::from_slice(&value)));
			assert!(t.remove(&key).unwrap().is_none());
		}*/
	}
}
