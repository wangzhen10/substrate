// Copyright 2017 Parity Technologies (UK) Ltd.
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
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.?

use std::collections::{HashMap, HashSet, BTreeMap};
use std::{mem, cmp};
use std::sync::Arc;
use std::time;
use parking_lot::{RwLock, Mutex};
use serde_json;
use primitives::block::{HeaderHash, ExtrinsicHash, Number as BlockNumber, Header, Id as BlockId};
use primitives::{Hash, blake2_256};
use runtime_support::Hashable;
use network::{PeerId, NodeId};

use chain::Client;
use config::ProtocolConfig;
use consensus::Consensus;
use error;
use io::SyncIo;
use message::{self, Message};
use service::{Role, TransactionPool, BftMessageStream};
use specialization::Specialization;
use sync::{ChainSync, Status as SyncStatus, SyncState};
use super::header_hash;

const REQUEST_TIMEOUT_SEC: u64 = 40;

/// Current protocol version.
pub (crate) const CURRENT_VERSION: u32 = 1;
/// Current packet count.
pub (crate) const CURRENT_PACKET_COUNT: u8 = 1;


// Maximum allowed entries in `BlockResponse`
const MAX_BLOCK_DATA_RESPONSE: u32 = 128;

/// Syncing status and statistics
#[derive(Clone)]
pub struct ProtocolStatus {
	/// Sync status.
	pub sync: SyncStatus,
	/// Total number of connected peers
	pub num_peers: usize,
	/// Total number of active peers.
	pub num_active_peers: usize,
}

/// Peer information
struct Peer {
	/// Protocol version
	protocol_version: u32,
	/// Roles
	roles: Role,
	/// Peer best block hash
	best_hash: HeaderHash,
	/// Peer best block number
	best_number: BlockNumber,
	/// Pending block request if any
	block_request: Option<message::BlockRequest>,
	/// Request timestamp
	request_timestamp: Option<time::Instant>,
	/// Holds a set of transactions known to this peer.
	known_extrinsics: HashSet<ExtrinsicHash>,
	/// Holds a set of blocks known to this peer.
	known_blocks: HashSet<HeaderHash>,
	/// Request counter,
	next_request_id: message::RequestId,
}

#[derive(Debug)]
pub struct PeerInfo {
	/// Roles
	pub roles: Role,
	/// Protocol version
	pub protocol_version: u32,
	/// Peer best block hash
	pub best_hash: HeaderHash,
	/// Peer best block number
	pub best_number: BlockNumber,
}

/// Transaction stats
#[derive(Debug)]
pub struct TransactionStats {
	/// Block number where this TX was first seen.
	pub first_seen: u64,
	/// Peers it was propagated to.
	pub propagated_to: BTreeMap<NodeId, usize>,
}

enum Action {
	Message(PeerId, Message),
	Disconnect(PeerId),
	Disable(PeerId),
}

/// Protocol context.
pub(crate) struct Context<'a> {
	io: &'a mut SyncIo,
	context_data: &'a ContextData,
	actions: Vec<Action>,
}

impl<'a> Context<'a> {
	fn new(context_data: &'a ContextData, io: &'a mut SyncIo) -> Self {
		Context {
			io,
			context_data,
			actions: Vec::new(),
		}
	}

	fn push(&mut self, action: Action) {
		self.actions.push(action);
	}

	/// Send a message to a peer.
	pub(crate) fn send_message(&mut self, peer_id: PeerId, message: Message) {
		self.push(Action::Message(peer_id, message));
	}

	pub(crate) fn disable_peer(&mut self, peer_id: PeerId) {
		self.push(Action::Disable(peer_id))
	}

	pub(crate) fn disconnect_peer(&mut self, peer_id: PeerId) {
		self.push(Action::Disconnect(peer_id))
	}

	/// Get peer info.
	pub(crate) fn peer_info(&self, peer: PeerId) -> Option<PeerInfo> {
		self.context_data.peers.read().get(&peer).map(|p| {
			PeerInfo {
				roles: p.roles,
				protocol_version: p.protocol_version,
				best_hash: p.best_hash,
				best_number: p.best_number,
			}
		})
	}

	/// Get chain handle.
	pub(crate) fn chain(&self) -> &Client {
		&*self.context_data.chain
	}
}

impl<'a> ::specialization::HandlerContext for Context<'a> {
	fn send(&mut self, peer_id: PeerId, message: Vec<u8>) {
		self.push(Action::Message(peer_id, Message::ChainSpecific(message)));
	}

	fn disable_peer(&mut self, peer_id: PeerId) {
		self.push(Action::Disable(peer_id))
	}

	fn disconnect_peer(&mut self, peer_id: PeerId) {
		self.push(Action::Disconnect(peer_id))
	}

	fn client(&self) -> &Client {
		self.chain()
	}
}

impl<'a> Drop for Context<'a> {
	fn drop(&mut self) {
		for action in self.actions.drain(..) {
			match action {
				Action::Message(id, message) => send_message(&self.context_data.peers, self.io, id, message),
				Action::Disconnect(id) => self.io.disconnect_peer(id),
				Action::Disable(id) => self.io.disable_peer(id),
			}
		}
	}
}

struct ContextData {
	// All connected peers
	peers: RwLock<HashMap<PeerId, Peer>>,
	chain: Arc<Client>,
}

// Lock must always be taken in order declared here.
pub struct Protocol<S: Specialization> {
	config: ProtocolConfig,
	specialization: RwLock<S>,
	genesis_hash: HeaderHash,
	sync: RwLock<ChainSync>,
	consensus: Mutex<Consensus>,
	context_data: ContextData,
	// Connected peers pending Status message.
	handshaking_peers: RwLock<HashMap<PeerId, time::Instant>>,
	transaction_pool: Arc<TransactionPool>,
}

impl<S: Specialization> Protocol<S> {
	/// Create a new instance.
	pub fn new(
		config: ProtocolConfig,
		chain: Arc<Client>,
		transaction_pool: Arc<TransactionPool>,
		specialization: S,
	) -> error::Result<Self>  {
		let info = chain.info()?;
		let best_hash = info.chain.best_hash;
		let protocol = Protocol {
			config: config,
			specialization: RwLock::new(specialization),
			context_data: ContextData {
				peers: RwLock::new(HashMap::new()),
				chain,
			},
			genesis_hash: info.chain.genesis_hash,
			sync: RwLock::new(ChainSync::new(&info)),
			consensus: Mutex::new(Consensus::new(best_hash)),
			handshaking_peers: RwLock::new(HashMap::new()),
			transaction_pool: transaction_pool,
		};
		Ok(protocol)
	}

	/// Returns protocol status
	pub fn status(&self) -> ProtocolStatus {
		let sync = self.sync.read();
		let peers = self.context_data.peers.read();
		ProtocolStatus {
			sync: sync.status(),
			num_peers: peers.values().count(),
			num_active_peers: peers.values().filter(|p| p.block_request.is_some()).count(),
		}
	}

	pub fn handle_packet(&self, io: &mut SyncIo, peer_id: PeerId, data: &[u8]) {
		let message: Message = match serde_json::from_slice(data) {
			Ok(m) => m,
			Err(e) => {
				debug!("Invalid packet from {}: {}", peer_id, e);
				io.disable_peer(peer_id);
				return;
			}
		};

		match message {
			Message::Status(s) => self.on_status_message(io, peer_id, s),
			Message::BlockRequest(r) => self.on_block_request(io, peer_id, r),
			Message::BlockResponse(r) => {
				let request = {
					let mut peers = self.context_data.peers.write();
					if let Some(ref mut peer) = peers.get_mut(&peer_id) {
						peer.request_timestamp = None;
						match mem::replace(&mut peer.block_request, None) {
							Some(r) => r,
							None => {
								debug!("Unexpected response packet from {}", peer_id);
								io.disable_peer(peer_id);
								return;
							}
						}
					} else {
						debug!("Unexpected packet from {}", peer_id);
						io.disable_peer(peer_id);
						return;
					}
				};
				if request.id != r.id {
					trace!(target: "sync", "Ignoring mismatched response packet from {} (expected {} got {})", peer_id, request.id, r.id);
					return;
				}
				self.on_block_response(io, peer_id, request, r);
			},
			Message::BlockAnnounce(announce) => {
				self.on_block_announce(io, peer_id, announce);
			},
			Message::BftMessage(m) => self.on_bft_message(io, peer_id, m, blake2_256(data).into()),
			Message::Extrinsics(m) => self.on_extrinsics(io, peer_id, m),
			Message::ChainSpecific(data) => self.on_chain_specific(io, peer_id, data),
		}
	}

	pub fn send_message(&self, io: &mut SyncIo, peer_id: PeerId, message: Message) {
		send_message(&self.context_data.peers, io, peer_id, message);
	}

	/// Called when a new peer is connected
	pub fn on_peer_connected(&self, io: &mut SyncIo, peer_id: PeerId) {
		trace!(target: "sync", "Connected {}: {}", peer_id, io.peer_info(peer_id));
		self.handshaking_peers.write().insert(peer_id, time::Instant::now());
		self.send_status(io, peer_id);
	}

	/// Called by peer when it is disconnecting
	pub fn on_peer_disconnected(&self, io: &mut SyncIo, peer: PeerId) {
		trace!(target: "sync", "Disconnecting {}: {}", peer, io.peer_info(peer));
		let removed = {
			let mut peers = self.context_data.peers.write();
			let mut handshaking_peers = self.handshaking_peers.write();
			handshaking_peers.remove(&peer);
			peers.remove(&peer).is_some()
		};
		if removed {
			let mut context = Context::new(&self.context_data, io);
			self.consensus.lock().peer_disconnected(&mut context, peer);
			self.sync.write().peer_disconnected(&mut context, peer);
		}
	}

	fn on_block_request(&self, io: &mut SyncIo, peer: PeerId, request: message::BlockRequest) {
		trace!(target: "sync", "BlockRequest {} from {}: from {:?} to {:?} max {:?}", request.id, peer, request.from, request.to, request.max);
		let mut blocks = Vec::new();
		let mut id = match request.from {
			message::FromBlock::Hash(h) => BlockId::Hash(h),
			message::FromBlock::Number(n) => BlockId::Number(n),
		};
		let max = cmp::min(request.max.unwrap_or(u32::max_value()), MAX_BLOCK_DATA_RESPONSE) as usize;
		// TODO: receipts, etc.
		let (mut get_header, mut get_body, mut get_justification) = (false, false, false);
		for a in request.fields {
			match a {
				message::BlockAttribute::Header => get_header = true,
				message::BlockAttribute::Body => get_body = true,
				message::BlockAttribute::Receipt => unimplemented!(),
				message::BlockAttribute::MessageQueue => unimplemented!(),
				message::BlockAttribute::Justification => get_justification = true,
			}
		}
		while let Some(header) = self.context_data.chain.header(&id).unwrap_or(None) {
			if blocks.len() >= max{
				break;
			}
			let number = header.number;
			let hash = header_hash(&header);
			let block_data = message::BlockData {
				hash: hash,
				header: if get_header { Some(header) } else { None },
				body: if get_body { self.context_data.chain.body(&BlockId::Hash(hash)).unwrap_or(None) } else { None },
				receipt: None,
				message_queue: None,
				justification: if get_justification { self.context_data.chain.justification(&BlockId::Hash(hash)).unwrap_or(None) } else { None },
			};
			blocks.push(block_data);
			match request.direction {
				message::Direction::Ascending => id = BlockId::Number(number + 1),
				message::Direction::Descending => {
					if number == 0 {
						break;
					}
					id = BlockId::Number(number - 1)
				}
			}
		}
		let response = message::BlockResponse {
			id: request.id,
			blocks: blocks,
		};
		self.send_message(io, peer, Message::BlockResponse(response))
	}

	fn on_block_response(&self, io: &mut SyncIo, peer: PeerId, request: message::BlockRequest, response: message::BlockResponse) {
		// TODO: validate response
		trace!(target: "sync", "BlockResponse {} from {} with {} blocks", response.id, peer, response.blocks.len());
		self.sync.write().on_block_data(&mut Context::new(&self.context_data, io), peer, request, response);
	}

	fn on_bft_message(&self, io: &mut SyncIo, peer: PeerId, message: message::LocalizedBftMessage, hash: Hash) {
		trace!(target: "sync", "BFT message from {}: {:?}", peer, message);
		self.consensus.lock().on_bft_message(&mut Context::new(&self.context_data, io), peer, message, hash);
	}

	fn on_chain_specific(&self, io: &mut SyncIo, peer: PeerId, message: Vec<u8>) {
		self.specialization.write().on_message(&mut Context::new(&self.context_data, io), peer, message);
	}

	/// See `ConsensusService` trait.
	pub fn send_bft_message(&self, io: &mut SyncIo, message: message::LocalizedBftMessage) {
		self.consensus.lock().send_bft_message(&mut Context::new(&self.context_data, io), message)
	}

	/// See `ConsensusService` trait.
	pub fn bft_messages(&self, parent_hash: Hash) -> BftMessageStream {
		self.consensus.lock().bft_messages(parent_hash)
	}

	/// Perform time based maintenance.
	pub fn tick(&self, io: &mut SyncIo) {
		self.maintain_peers(io);
		self.consensus.lock().collect_garbage(None);
	}

	fn maintain_peers(&self, io: &mut SyncIo) {
		let tick = time::Instant::now();
		let mut aborting = Vec::new();
		{
			let peers = self.context_data.peers.read();
			let handshaking_peers = self.handshaking_peers.read();
			for (peer_id, timestamp) in peers.iter()
				.filter_map(|(id, peer)| peer.request_timestamp.as_ref().map(|r| (id, r)))
				.chain(handshaking_peers.iter()) {
				if (tick - *timestamp).as_secs() > REQUEST_TIMEOUT_SEC {
					trace!(target: "sync", "Timeout {}", peer_id);
					io.disconnect_peer(*peer_id);
					aborting.push(*peer_id);
				}
			}
		}
		for p in aborting {
			self.on_peer_disconnected(io, p);
		}
	}

	pub fn peer_info(&self, peer: PeerId) -> Option<PeerInfo> {
		self.context_data.peers.read().get(&peer).map(|p| {
			PeerInfo {
				roles: p.roles,
				protocol_version: p.protocol_version,
				best_hash: p.best_hash,
				best_number: p.best_number,
			}
		})
	}

	/// Called by peer to report status
	fn on_status_message(&self, io: &mut SyncIo, peer_id: PeerId, status: message::Status) {
		trace!(target: "sync", "New peer {} {:?}", peer_id, status);
		if io.is_expired() {
			trace!(target: "sync", "Status packet from expired session {}:{}", peer_id, io.peer_info(peer_id));
			return;
		}

		let mut sync = self.sync.write();
		let mut consensus = self.consensus.lock();
		{
			let mut peers = self.context_data.peers.write();
			let mut handshaking_peers = self.handshaking_peers.write();
			if peers.contains_key(&peer_id) {
				debug!(target: "sync", "Unexpected status packet from {}:{}", peer_id, io.peer_info(peer_id));
				return;
			}
			if status.genesis_hash != self.genesis_hash {
				io.disable_peer(peer_id);
				trace!(target: "sync", "Peer {} genesis hash mismatch (ours: {}, theirs: {})", peer_id, self.genesis_hash, status.genesis_hash);
				return;
			}
			if status.version != CURRENT_VERSION {
				io.disable_peer(peer_id);
				trace!(target: "sync", "Peer {} unsupported eth protocol ({})", peer_id, status.version);
				return;
			}

			let peer = Peer {
				protocol_version: status.version,
				roles: message::Role::as_flags(&status.roles),
				best_hash: status.best_hash,
				best_number: status.best_number,
				block_request: None,
				request_timestamp: None,
				known_extrinsics: HashSet::new(),
				known_blocks: HashSet::new(),
				next_request_id: 0,
			};
			peers.insert(peer_id.clone(), peer);
			handshaking_peers.remove(&peer_id);
			debug!(target: "sync", "Connected {} {}", peer_id, io.peer_info(peer_id));
		}

		let mut context = Context::new(&self.context_data, io);
		sync.new_peer(&mut context, peer_id);
		consensus.new_peer(&mut context, peer_id, &status.roles);
	}

	/// Called when peer sends us new extrinsics
	fn on_extrinsics(&self, _io: &mut SyncIo, peer_id: PeerId, extrinsics: message::Extrinsics) {
		// Accept extrinsics only when fully synced
		if self.sync.read().status().state != SyncState::Idle {
			trace!(target: "sync", "{} Ignoring extrinsics while syncing", peer_id);
			return;
		}
		trace!(target: "sync", "Received {} extrinsics from {}", extrinsics.len(), peer_id);
		let mut peers = self.context_data.peers.write();
		if let Some(ref mut peer) = peers.get_mut(&peer_id) {
			for t in extrinsics {
				if let Some(hash) = self.transaction_pool.import(&t) {
					peer.known_extrinsics.insert(hash);
				}
			}
		}
	}

	/// Called when we propagate ready extrinsics to peers.
	pub fn propagate_extrinsics(&self, io: &mut SyncIo) {
		debug!(target: "sync", "Propagating extrinsics");

		// Accept transactions only when fully synced
		if self.sync.read().status().state != SyncState::Idle {
			return;
		}

		let extrinsics = self.transaction_pool.transactions();

		let mut peers = self.context_data.peers.write();
		for (peer_id, ref mut peer) in peers.iter_mut() {
			let to_send: Vec<_> = extrinsics.iter().filter_map(|&(hash, ref t)|
				if peer.known_extrinsics.insert(hash.clone()) { Some(t.clone()) } else { None }).collect();
			if !to_send.is_empty() {
				trace!(target: "sync", "Sending {} extrinsics to {}", to_send.len(), peer_id);
				self.send_message(io, *peer_id, Message::Extrinsics(to_send));
			}
		}
	}

	/// Send Status message
	fn send_status(&self, io: &mut SyncIo, peer_id: PeerId) {
		if let Ok(info) = self.context_data.chain.info() {
			let status = message::Status {
				version: CURRENT_VERSION,
				genesis_hash: info.chain.genesis_hash,
				roles: self.config.roles.into(),
				best_number: info.chain.best_number,
				best_hash: info.chain.best_hash,
				authority_signature: None,
				authority_id: None,
				chain_status: self.specialization.read().status(),
			};
			self.send_message(io, peer_id, Message::Status(status))
		}
	}

	pub fn abort(&self) {
		let mut sync = self.sync.write();
		let mut peers = self.context_data.peers.write();
		let mut handshaking_peers = self.handshaking_peers.write();
		sync.clear();
		peers.clear();
		handshaking_peers.clear();
		self.consensus.lock().restart();
	}

	pub fn on_block_announce(&self, io: &mut SyncIo, peer_id: PeerId, announce: message::BlockAnnounce) {
		let header = announce.header;
		let hash: HeaderHash = header.blake2_256().into();
		{
			let mut peers = self.context_data.peers.write();
			if let Some(ref mut peer) = peers.get_mut(&peer_id) {
				peer.known_blocks.insert(hash.clone());
			}
		}
		self.sync.write().on_block_announce(&mut Context::new(&self.context_data, io), peer_id, hash, &header);
	}

	pub fn on_block_imported(&self, io: &mut SyncIo, hash: HeaderHash, header: &Header) {
		self.sync.write().update_chain_info(&header);
		// send out block announcements
		let mut peers = self.context_data.peers.write();

		for (peer_id, ref mut peer) in peers.iter_mut() {
			if peer.known_blocks.insert(hash.clone()) {
				trace!(target: "sync", "Announcing block {:?} to {}", hash, peer_id);
				self.send_message(io, *peer_id, Message::BlockAnnounce(message::BlockAnnounce {
					header: header.clone()
				}));
			}
		}

		self.consensus.lock().collect_garbage(Some((hash, &header)));
	}

	pub fn transactions_stats(&self) -> BTreeMap<ExtrinsicHash, TransactionStats> {
		BTreeMap::new()
	}

	pub fn chain(&self) -> &Client {
		&*self.context_data.chain
	}
}

fn send_message(peers: &RwLock<HashMap<PeerId, Peer>>, io: &mut SyncIo, peer_id: PeerId, mut message: Message) {
	match &mut message {
		&mut Message::BlockRequest(ref mut r) => {
			let mut peers = peers.write();
			if let Some(ref mut peer) = peers.get_mut(&peer_id) {
				r.id = peer.next_request_id;
				peer.next_request_id = peer.next_request_id + 1;
				peer.block_request = Some(r.clone());
				peer.request_timestamp = Some(time::Instant::now());
			}
		},
		_ => (),
	}
	let data = serde_json::to_vec(&message).expect("Serializer is infallible; qed");
	if let Err(e) = io.send(peer_id, data) {
		debug!(target:"sync", "Error sending message: {:?}", e);
		io.disconnect_peer(peer_id);
	}
}

/// Hash a message.
// TODO: remove this. non-unique and insecure.
pub fn hash_message(message: &Message) -> Hash {
	let data = serde_json::to_vec(&message).expect("Serializer is infallible; qed");
	blake2_256(&data).into()
}
