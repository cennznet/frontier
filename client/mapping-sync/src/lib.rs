// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
// This file is part of Frontier.
//
// Copyright (c) 2020 Parity Technologies (UK) Ltd.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

mod worker;

pub use worker::{MappingSyncWorker, SyncStrategy};

use fp_consensus::FindLogError;
use fp_rpc::EthereumRuntimeRPCApi;
use log::{debug, warn};
use sc_client_api::BlockOf;
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_blockchain::HeaderBackend;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Header as HeaderT, Zero},
};

pub fn sync_block<Block: BlockT>(
	backend: &fc_db::Backend<Block>,
	header: &Block::Header,
) -> Result<(), String> {
	match fp_consensus::find_log(header.digest()) {
		Ok(log) => {
			let post_hashes = log.into_hashes();

			let mapping_commitment = fc_db::MappingCommitment {
				block_hash: header.hash(),
				ethereum_block_hash: post_hashes.block_hash,
				ethereum_transaction_hashes: post_hashes.transaction_hashes,
			};
			backend.mapping().write_hashes(mapping_commitment)?;

			Ok(())
		}
		Err(FindLogError::NotFound) => {
			backend.mapping().write_none(header.hash())?;

			Ok(())
		}
		Err(FindLogError::MultipleLogs) => Err("Multiple logs found".to_string()),
	}
}

pub fn sync_genesis_block<Block: BlockT, C>(
	client: &C,
	backend: &fc_db::Backend<Block>,
	header: &Block::Header,
) -> Result<(), String>
where
	C: ProvideRuntimeApi<Block> + Send + Sync + HeaderBackend<Block> + BlockOf,
	C::Api: EthereumRuntimeRPCApi<Block>,
{
	let id = BlockId::Hash(header.hash());

	let has_api = client
		.runtime_api()
		.has_api::<dyn EthereumRuntimeRPCApi<Block>>(&id)
		.map_err(|e| format!("{:?}", e))?;

	if has_api {
		let block = client
			.runtime_api()
			.current_block(&id)
			.map_err(|e| format!("{:?}", e))?;
		let block_hash = block
			.ok_or("Ethereum genesis block not found".to_string())?
			.header
			.hash();
		let mapping_commitment = fc_db::MappingCommitment::<Block> {
			block_hash: header.hash(),
			ethereum_block_hash: block_hash,
			ethereum_transaction_hashes: Vec::new(),
		};
		backend.mapping().write_hashes(mapping_commitment)?;
	} else {
		backend.mapping().write_none(header.hash())?;
	}

	Ok(())
}

pub fn sync_one_block<Block: BlockT, C, B>(
	client: &C,
	substrate_backend: &B,
	frontier_backend: &fc_db::Backend<Block>,
	sync_from: <Block::Header as HeaderT>::Number,
	strategy: SyncStrategy,
) -> Result<bool, String>
where
	C: ProvideRuntimeApi<Block> + Send + Sync + HeaderBackend<Block> + BlockOf,
	C::Api: EthereumRuntimeRPCApi<Block>,
	B: sp_blockchain::HeaderBackend<Block> + sp_blockchain::Backend<Block>,
{
	let mut current_syncing_tips = frontier_backend.meta().current_syncing_tips()?;

	if current_syncing_tips.is_empty() {
		let mut leaves = substrate_backend.leaves().map_err(|e| format!("{:?}", e))?;
		if leaves.is_empty() {
			return Ok(false);
		}
		current_syncing_tips.append(&mut leaves);
	}

	let mut operating_header = None;
	while let Some(checking_tip) = current_syncing_tips.pop() {
		if let Some(checking_header) =
			fetch_header(substrate_backend, frontier_backend, checking_tip, sync_from)?
		{
			operating_header = Some(checking_header);
			break;
		}
	}
	let operating_header = match operating_header {
		Some(operating_header) => operating_header,
		None => {
			debug!(target: "mapping-sync", "already synced tip");
			frontier_backend
				.meta()
				.write_current_syncing_tips(current_syncing_tips)?;
			return Ok(false);
		}
	};

	if operating_header.number() == &Zero::zero() {
		warn!(target: "mapping-sync", "sync from 0");
		sync_genesis_block(client, frontier_backend, &operating_header)?;

		frontier_backend
			.meta()
			.write_current_syncing_tips(current_syncing_tips)?;
		Ok(true)
	} else {
		if SyncStrategy::Parachain == strategy
			&& operating_header.number() > &client.info().best_number
		{
			return Ok(false);
		}
		debug!(target: "mapping-sync", "sync block. number:{:?}, hash:{:?}", operating_header.number(), operating_header.hash());
		sync_block(frontier_backend, &operating_header)?;

		current_syncing_tips.push(*operating_header.parent_hash());
		frontier_backend
			.meta()
			.write_current_syncing_tips(current_syncing_tips)?;
		Ok(true)
	}
}

pub fn sync_blocks<Block: BlockT, C, B>(
	client: &C,
	substrate_backend: &B,
	frontier_backend: &fc_db::Backend<Block>,
	limit: usize,
	sync_from: <Block::Header as HeaderT>::Number,
	strategy: SyncStrategy,
) -> Result<bool, String>
where
	C: ProvideRuntimeApi<Block> + Send + Sync + HeaderBackend<Block> + BlockOf,
	C::Api: EthereumRuntimeRPCApi<Block>,
	B: sp_blockchain::HeaderBackend<Block> + sp_blockchain::Backend<Block>,
{
	let mut synced_any = false;

	for _ in 0..limit {
		synced_any = synced_any
			|| sync_one_block(
				client,
				substrate_backend,
				frontier_backend,
				sync_from,
				strategy,
			)?;
	}

	Ok(synced_any)
}

pub fn fetch_header<Block: BlockT, B>(
	substrate_backend: &B,
	frontier_backend: &fc_db::Backend<Block>,
	checking_tip: Block::Hash,
	sync_from: <Block::Header as HeaderT>::Number,
) -> Result<Option<Block::Header>, String>
where
	B: sp_blockchain::HeaderBackend<Block> + sp_blockchain::Backend<Block>,
{
	if frontier_backend.mapping().is_synced(&checking_tip)? {
		return Ok(None);
	}

	match substrate_backend.header(BlockId::Hash(checking_tip)) {
		Ok(Some(checking_header)) if checking_header.number() >= &sync_from => {
			Ok(Some(checking_header))
		}
		Ok(Some(_)) => Ok(None),
		Ok(None) | Err(_) => Err("Header not found".to_string()),
	}
}
