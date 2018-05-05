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

use bytes::Bytes;
use ethcore::client::{BlockId};
use ethcore::header::{BlockNumber};
use ethcore::snapshot::ManifestData;
use ethereum_types::{H256};
use network::{self, PeerId};
use parking_lot::RwLock;
use rlp::{Rlp, RlpStream};
use std::cmp;
use sync_io::SyncIo;

use super::{
	ChainSync,
	PeerInfo,

	RlpResponseResult,
	PacketDecodeError,

	BLOCK_BODIES_PACKET,
	BLOCK_HEADERS_PACKET,

	CONSENSUS_DATA_PACKET,
	GET_BLOCK_BODIES_PACKET,
	GET_BLOCK_HEADERS_PACKET,
	GET_NODE_DATA_PACKET,
	GET_RECEIPTS_PACKET,
	GET_SNAPSHOT_DATA_PACKET,
	GET_SNAPSHOT_MANIFEST_PACKET,
	GET_SNAPSHOT_BITFIELD_PACKET,

	MAX_BODIES_TO_SEND,
	MAX_HEADERS_TO_SEND,
	MAX_NODE_DATA_TO_SEND,
	MAX_RECEIPTS_HEADERS_TO_SEND,
	MAX_RECEIPTS_TO_SEND,
	NODE_DATA_PACKET,
	RECEIPTS_PACKET,
	SNAPSHOT_BITFIELD_PACKET,
	SNAPSHOT_DATA_PACKET,
	SNAPSHOT_MANIFEST_PACKET,
};

type FRlp = Fn(&ChainSync, &SyncIo, &Rlp, PeerId) -> RlpResponseResult;

pub struct SyncSupplier {}

impl SyncSupplier {
	/// Dispatch incoming requests and responses
	pub fn dispatch_packet(sync: &RwLock<ChainSync>, io: &mut SyncIo, peer: PeerId, packet_id: u8, data: &[u8]) {
		let rlp = Rlp::new(data);
		let rlp_func: Option<&FRlp> = match packet_id {
			GET_BLOCK_BODIES_PACKET => Some(&SyncSupplier::return_block_bodies),
			GET_BLOCK_HEADERS_PACKET => Some(&SyncSupplier::return_block_headers),
			GET_RECEIPTS_PACKET => Some(&SyncSupplier::return_receipts),
			GET_NODE_DATA_PACKET => Some(&SyncSupplier::return_node_data),
			GET_SNAPSHOT_BITFIELD_PACKET => Some(&SyncSupplier::return_snapshot_bitfield),
			GET_SNAPSHOT_MANIFEST_PACKET => Some(&SyncSupplier::return_snapshot_manifest),
			GET_SNAPSHOT_DATA_PACKET => Some(&SyncSupplier::return_snapshot_data),
			_ => None,
		};

		let result = match rlp_func {
			Some(rlp_func) => SyncSupplier::return_rlp(
				&sync.read(), io, &rlp, peer, rlp_func,
				|e| format!("Error sending packet {:?}: {:?}", packet_id, e)
			),
			None => {
				match packet_id {
					CONSENSUS_DATA_PACKET => ChainSync::on_consensus_packet(io, peer, &rlp),
					_ => {
						sync.write().on_packet(io, peer, packet_id, data);
						Ok(())
					}
				}
			}
		};

		result.unwrap_or_else(|e| {
			debug!(target:"sync", "{} -> Malformed packet {} : {}", peer, packet_id, e);
		})
	}

	/// Respond to GetBlockHeaders request
	fn return_block_headers(_sync: &ChainSync, io: &SyncIo, r: &Rlp, peer_id: PeerId) -> RlpResponseResult {
		// Packet layout:
		// [ block: { P , B_32 }, maxHeaders: P, skip: P, reverse: P in { 0 , 1 } ]
		let max_headers: usize = r.val_at(1)?;
		let skip: usize = r.val_at(2)?;
		let reverse: bool = r.val_at(3)?;
		let last = io.chain().chain_info().best_block_number;
		let number = if r.at(0)?.size() == 32 {
			// id is a hash
			let hash: H256 = r.val_at(0)?;
			trace!(target: "sync", "{} -> GetBlockHeaders (hash: {}, max: {}, skip: {}, reverse:{})", peer_id, hash, max_headers, skip, reverse);
			match io.chain().block_header(BlockId::Hash(hash)) {
				Some(hdr) => {
					let number = hdr.number().into();
					debug_assert_eq!(hdr.hash(), hash);

					if max_headers == 1 || io.chain().block_hash(BlockId::Number(number)) != Some(hash) {
						// Non canonical header or single header requested
						// TODO: handle single-step reverse hashchains of non-canon hashes
						trace!(target:"sync", "Returning single header: {:?}", hash);
						let mut rlp = RlpStream::new_list(1);
						rlp.append_raw(&hdr.into_inner(), 1);
						return Ok(Some((BLOCK_HEADERS_PACKET, rlp)));
					}
					number
				}
				None => return Ok(Some((BLOCK_HEADERS_PACKET, RlpStream::new_list(0)))) //no such header, return nothing
			}
		} else {
			let number = r.val_at::<BlockNumber>(0)?;
			trace!(target: "sync", "{} -> GetBlockHeaders (number: {}, max: {}, skip: {}, reverse:{})", peer_id, number, max_headers, skip, reverse);
			number
		};

		let mut number = if reverse {
			cmp::min(last, number)
		} else {
			cmp::max(0, number)
		};
		let max_count = cmp::min(MAX_HEADERS_TO_SEND, max_headers);
		let mut count = 0;
		let mut data = Bytes::new();
		let inc = (skip + 1) as BlockNumber;
		let overlay = io.chain_overlay().read();

		while (number <= last || overlay.contains_key(&number)) && count < max_count {
			if let Some(hdr) = overlay.get(&number) {
				trace!(target: "sync", "{}: Returning cached fork header", peer_id);
				data.extend_from_slice(hdr);
				count += 1;
			} else if let Some(hdr) = io.chain().block_header(BlockId::Number(number)) {
				data.append(&mut hdr.into_inner());
				count += 1;
			} else {
				// No required block.
				break;
			}
			if reverse {
				if number <= inc || number == 0 {
					break;
				}
				number -= inc;
			} else {
				number += inc;
			}
		}
		let mut rlp = RlpStream::new_list(count as usize);
		rlp.append_raw(&data, count as usize);
		trace!(target: "sync", "{} -> GetBlockHeaders: returned {} entries", peer_id, count);
		Ok(Some((BLOCK_HEADERS_PACKET, rlp)))
	}

	/// Respond to GetBlockBodies request
	fn return_block_bodies(_sync: &ChainSync, io: &SyncIo, r: &Rlp, peer_id: PeerId) -> RlpResponseResult {
		let mut count = r.item_count().unwrap_or(0);
		if count == 0 {
			debug!(target: "sync", "Empty GetBlockBodies request, ignoring.");
			return Ok(None);
		}
		count = cmp::min(count, MAX_BODIES_TO_SEND);
		let mut added = 0usize;
		let mut data = Bytes::new();
		for i in 0..count {
			if let Some(body) = io.chain().block_body(BlockId::Hash(r.val_at::<H256>(i)?)) {
				data.append(&mut body.into_inner());
				added += 1;
			}
		}
		let mut rlp = RlpStream::new_list(added);
		rlp.append_raw(&data, added);
		trace!(target: "sync", "{} -> GetBlockBodies: returned {} entries", peer_id, added);
		Ok(Some((BLOCK_BODIES_PACKET, rlp)))
	}

	/// Respond to GetNodeData request
	fn return_node_data(_sync: &ChainSync, io: &SyncIo, r: &Rlp, peer_id: PeerId) -> RlpResponseResult {
		let mut count = r.item_count().unwrap_or(0);
		trace!(target: "sync", "{} -> GetNodeData: {} entries", peer_id, count);
		if count == 0 {
			debug!(target: "sync", "Empty GetNodeData request, ignoring.");
			return Ok(None);
		}
		count = cmp::min(count, MAX_NODE_DATA_TO_SEND);
		let mut added = 0usize;
		let mut data = Vec::new();
		for i in 0..count {
			if let Some(node) = io.chain().state_data(&r.val_at::<H256>(i)?) {
				data.push(node);
				added += 1;
			}
		}
		trace!(target: "sync", "{} -> GetNodeData: return {} entries", peer_id, added);
		let mut rlp = RlpStream::new_list(added);
		for d in data {
			rlp.append(&d);
		}
		Ok(Some((NODE_DATA_PACKET, rlp)))
	}

	fn return_receipts(_sync: &ChainSync, io: &SyncIo, rlp: &Rlp, peer_id: PeerId) -> RlpResponseResult {
		let mut count = rlp.item_count().unwrap_or(0);
		trace!(target: "sync", "{} -> GetReceipts: {} entries", peer_id, count);
		if count == 0 {
			debug!(target: "sync", "Empty GetReceipts request, ignoring.");
			return Ok(None);
		}
		count = cmp::min(count, MAX_RECEIPTS_HEADERS_TO_SEND);
		let mut added_headers = 0usize;
		let mut added_receipts = 0usize;
		let mut data = Bytes::new();
		for i in 0..count {
			if let Some(mut receipts_bytes) = io.chain().block_receipts(&rlp.val_at::<H256>(i)?) {
				data.append(&mut receipts_bytes);
				added_receipts += receipts_bytes.len();
				added_headers += 1;
				if added_receipts > MAX_RECEIPTS_TO_SEND { break; }
			}
		}
		let mut rlp_result = RlpStream::new_list(added_headers);
		rlp_result.append_raw(&data, added_headers);
		Ok(Some((RECEIPTS_PACKET, rlp_result)))
	}

	/// Respond to GetSnapshotBitfield request
	fn return_snapshot_bitfield(sync: &ChainSync, _io: &SyncIo, r: &Rlp, peer_id: PeerId) -> RlpResponseResult {
		trace!(target: "warp-sync", "{} -> GetSnapshotBitfield", peer_id);
		if r.item_count().unwrap_or(0) != 0 {
			debug!(target: "warp-sync", "Invalid GetSnapshotBitfield request, ignoring.");
			return Ok(None);
		}
		let rlp = match sync.snapshot.snapshot_hash() {
			Some(_) => {
				trace!(target: "warp-sync", "{} <- SnapshotBitfield", peer_id);
				let bitfield = sync.snapshot.bitfield();
				let mut rlp = RlpStream::new();
				rlp.append_list(&bitfield);
				rlp
			},
			None => {
				trace!(target: "warp-sync", "{}: No partial snapshot manifest to return", peer_id);
				RlpStream::new_list(0)
			},
		};
		Ok(Some((SNAPSHOT_BITFIELD_PACKET, rlp)))
	}

	pub fn get_snapshot_manifest(io: &SyncIo, peer_protocol: u8) -> Option<ManifestData> {
		let supports_partial = PeerInfo::supports_partial_snapshots(peer_protocol);
		let manifest = io.snapshot_service().manifest();

		trace!(target: "warp-sync", "Peer; support_partials={} ; manifest={:?}", supports_partial, manifest);

		manifest
			.or(if supports_partial { io.snapshot_service().partial_manifest() } else { None })
	}

	/// Respond to GetSnapshotManifest request
	fn return_snapshot_manifest(sync: &ChainSync, io: &SyncIo, r: &Rlp, peer_id: PeerId) -> RlpResponseResult {
		let count = r.item_count().unwrap_or(0);
		trace!(target: "warp-sync", "{} -> GetSnapshotManifest", peer_id);
		if count != 0 {
			debug!(target: "warp-sync", "Invalid GetSnapshotManifest request, ignoring.");
			return Ok(None);
		}

		let peer_protocol = sync.peers.get(&peer_id).map_or(0, |ref peer| peer.protocol_version);
		let rlp = match SyncSupplier::get_snapshot_manifest(io, peer_protocol) {
			Some(manifest) => {
				trace!(target: "warp-sync", "{} <- SnapshotManifest", peer_id);
				let mut rlp = RlpStream::new_list(1);
				rlp.append_raw(&manifest.into_rlp(), 1);
				rlp
			},
			None => {
				trace!(target: "warp-sync", "{}: No snapshot manifest to return", peer_id);
				RlpStream::new_list(0)
			}
		};
		Ok(Some((SNAPSHOT_MANIFEST_PACKET, rlp)))
	}

	/// Respond to GetSnapshotData request
	fn return_snapshot_data(_sync: &ChainSync, io: &SyncIo, r: &Rlp, peer_id: PeerId) -> RlpResponseResult {
		let hash: H256 = r.val_at(0)?;
		trace!(target: "warp-sync", "{} -> GetSnapshotData {:?}", peer_id, hash);
		let rlp = match io.snapshot_service().chunk(hash) {
			Some(data) => {
				let mut rlp = RlpStream::new_list(1);
				trace!(target: "warp-sync", "{} <- SnapshotData", peer_id);
				rlp.append(&data);
				rlp
			},
			None => {
				trace!(target: "warp-sync", "{}: No snapshot data to return", peer_id);
				RlpStream::new_list(0)
			}
		};
		Ok(Some((SNAPSHOT_DATA_PACKET, rlp)))
	}

	fn return_rlp<FError>(sync: &ChainSync, io: &mut SyncIo, rlp: &Rlp, peer: PeerId, rlp_func: &FRlp, error_func: FError) -> Result<(), PacketDecodeError>
		where FError : FnOnce(network::Error) -> String
	{
		let response = (*rlp_func)(sync, io, rlp, peer);
		match response {
			Err(e) => Err(e),
			Ok(Some((packet_id, rlp_stream))) => {
				io.respond(packet_id, rlp_stream.out()).unwrap_or_else(
					|e| debug!(target: "sync", "{:?}", error_func(e)));
				Ok(())
			}
			_ => Ok(())
		}
	}
}

#[cfg(test)]
mod test {
	use std::collections::{VecDeque};
	use tests::helpers::{TestIo};
	use tests::snapshot::TestSnapshotService;
	use ethereum_types::{H256};
	use parking_lot::RwLock;
	use bytes::Bytes;
	use rlp::{Rlp, RlpStream};
	use super::{*, super::tests::*};
	use ethcore::client::{BlockChainClient, EachBlockWith, TestBlockChainClient};

	#[test]
	fn return_block_headers() {
		use ethcore::views::HeaderView;
		fn make_hash_req(h: &H256, count: usize, skip: usize, reverse: bool) -> Bytes {
			let mut rlp = RlpStream::new_list(4);
			rlp.append(h);
			rlp.append(&count);
			rlp.append(&skip);
			rlp.append(&if reverse {1u32} else {0u32});
			rlp.out()
		}

		fn make_num_req(n: usize, count: usize, skip: usize, reverse: bool) -> Bytes {
			let mut rlp = RlpStream::new_list(4);
			rlp.append(&n);
			rlp.append(&count);
			rlp.append(&skip);
			rlp.append(&if reverse {1u32} else {0u32});
			rlp.out()
		}
		fn to_header_vec(rlp: ::chain::RlpResponseResult) -> Vec<Bytes> {
			Rlp::new(&rlp.unwrap().unwrap().1.out()).iter().map(|r| r.as_raw().to_vec()).collect()
		}

		let mut client = TestBlockChainClient::new();
		client.add_blocks(100, EachBlockWith::Nothing);
		let blocks: Vec<_> = (0 .. 100)
			.map(|i| (&client as &BlockChainClient).block(BlockId::Number(i as BlockNumber)).map(|b| b.into_inner()).unwrap()).collect();
		let headers: Vec<_> = blocks.iter().map(|b| Rlp::new(b).at(0).unwrap().as_raw().to_vec()).collect();
		let hashes: Vec<_> = headers.iter().map(|h| view!(HeaderView, h).hash()).collect();

		let queue = RwLock::new(VecDeque::new());
		let ss = TestSnapshotService::new();
		let io = TestIo::new(&mut client, &ss, &queue, None);

		let unknown: H256 = H256::new();
		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_hash_req(&unknown, 1, 0, false)), 0);
		assert!(to_header_vec(result).is_empty());
		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_hash_req(&unknown, 1, 0, true)), 0);
		assert!(to_header_vec(result).is_empty());

		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_hash_req(&hashes[2], 1, 0, true)), 0);
		assert_eq!(to_header_vec(result), vec![headers[2].clone()]);

		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_hash_req(&hashes[2], 1, 0, false)), 0);
		assert_eq!(to_header_vec(result), vec![headers[2].clone()]);

		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_hash_req(&hashes[50], 3, 5, false)), 0);
		assert_eq!(to_header_vec(result), vec![headers[50].clone(), headers[56].clone(), headers[62].clone()]);

		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_hash_req(&hashes[50], 3, 5, true)), 0);
		assert_eq!(to_header_vec(result), vec![headers[50].clone(), headers[44].clone(), headers[38].clone()]);

		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_num_req(2, 1, 0, true)), 0);
		assert_eq!(to_header_vec(result), vec![headers[2].clone()]);

		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_num_req(2, 1, 0, false)), 0);
		assert_eq!(to_header_vec(result), vec![headers[2].clone()]);

		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_num_req(50, 3, 5, false)), 0);
		assert_eq!(to_header_vec(result), vec![headers[50].clone(), headers[56].clone(), headers[62].clone()]);

		let result = SyncSupplier::return_block_headers(&io, &Rlp::new(&make_num_req(50, 3, 5, true)), 0);
		assert_eq!(to_header_vec(result), vec![headers[50].clone(), headers[44].clone(), headers[38].clone()]);
	}

	#[test]
	fn return_nodes() {
		let mut client = TestBlockChainClient::new();
		let queue = RwLock::new(VecDeque::new());
		let sync = dummy_sync_with_peer(H256::new(), &client);
		let ss = TestSnapshotService::new();
		let mut io = TestIo::new(&mut client, &ss, &queue, None);

		let mut node_list = RlpStream::new_list(3);
		node_list.append(&H256::from("0000000000000000000000000000000000000000000000005555555555555555"));
		node_list.append(&H256::from("ffffffffffffffffffffffffffffffffffffffffffffaaaaaaaaaaaaaaaaaaaa"));
		node_list.append(&H256::from("aff0000000000000000000000000000000000000000000000000000000000000"));

		let node_request = node_list.out();
		// it returns rlp ONLY for hashes started with "f"
		let result = SyncSupplier::return_node_data(&io, &Rlp::new(&node_request.clone()), 0);

		assert!(result.is_ok());
		let rlp_result = result.unwrap();
		assert!(rlp_result.is_some());

		// the length of one rlp-encoded hashe
		let rlp = rlp_result.unwrap().1.out();
		let rlp = Rlp::new(&rlp);
		assert_eq!(Ok(1), rlp.item_count());

		io.sender = Some(2usize);

		ChainSync::dispatch_packet(&RwLock::new(sync), &mut io, 0usize, GET_NODE_DATA_PACKET, &node_request);
		assert_eq!(1, io.packets.len());
	}

	#[test]
	fn return_receipts_empty() {
		let mut client = TestBlockChainClient::new();
		let queue = RwLock::new(VecDeque::new());
		let ss = TestSnapshotService::new();
		let io = TestIo::new(&mut client, &ss, &queue, None);

		let result = SyncSupplier::return_receipts(&io, &Rlp::new(&[0xc0]), 0);

		assert!(result.is_ok());
	}

	#[test]
	fn return_receipts() {
		let mut client = TestBlockChainClient::new();
		let queue = RwLock::new(VecDeque::new());
		let sync = dummy_sync_with_peer(H256::new(), &client);
		let ss = TestSnapshotService::new();
		let mut io = TestIo::new(&mut client, &ss, &queue, None);

		let mut receipt_list = RlpStream::new_list(4);
		receipt_list.append(&H256::from("0000000000000000000000000000000000000000000000005555555555555555"));
		receipt_list.append(&H256::from("ff00000000000000000000000000000000000000000000000000000000000000"));
		receipt_list.append(&H256::from("fff0000000000000000000000000000000000000000000000000000000000000"));
		receipt_list.append(&H256::from("aff0000000000000000000000000000000000000000000000000000000000000"));

		let receipts_request = receipt_list.out();
		// it returns rlp ONLY for hashes started with "f"
		let result = SyncSupplier::return_receipts(&io, &Rlp::new(&receipts_request.clone()), 0);

		assert!(result.is_ok());
		let rlp_result = result.unwrap();
		assert!(rlp_result.is_some());

		// the length of two rlp-encoded receipts
		assert_eq!(603, rlp_result.unwrap().1.out().len());

		io.sender = Some(2usize);
		ChainSync::dispatch_packet(&RwLock::new(sync), &mut io, 0usize, GET_RECEIPTS_PACKET, &receipts_request);
		assert_eq!(1, io.packets.len());
	}
}
