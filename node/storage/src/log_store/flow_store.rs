use crate::config::ShardConfig;
use crate::error::Error;
use crate::log_store::load_chunk::EntryBatch;
use crate::log_store::log_manager::{
    bytes_to_entries, COL_ENTRY_BATCH, COL_FLOW_MPT_NODES, COL_PAD_DATA_LIST,
    COL_PAD_DATA_SYNC_HEIGH, PORA_CHUNK_SIZE,
};
use crate::log_store::seal_task_manager::SealTaskManager;
use crate::log_store::{
    metrics, FlowRead, FlowSeal, FlowWrite, MineLoadChunk, SealAnswer, SealTask,
};
use crate::{try_option, ZgsKeyValueDB};
use any::Any;
use anyhow::{anyhow, bail, Result};
use append_merkle::{MerkleTreeRead, NodeDatabase, NodeTransaction};
use itertools::Itertools;
use kvdb::DBTransaction;
use parking_lot::RwLock;
use shared_types::{ChunkArray, DataRoot, FlowProof};
use ssz::{Decode, Encode};
use ssz_derive::{Decode as DeriveDecode, Encode as DeriveEncode};

use std::fmt::Debug;
use std::sync::Arc;
use std::time::Instant;
use std::{any, cmp};
use tracing::{debug, error, trace};
use zgs_spec::{BYTES_PER_SECTOR, SEALS_PER_LOAD, SECTORS_PER_LOAD, SECTORS_PER_SEAL};

pub struct FlowStore {
    flow_db: Arc<FlowDBStore>,
    data_db: Arc<FlowDBStore>,
    seal_manager: SealTaskManager,
    config: FlowConfig,
}

impl FlowStore {
    pub fn new(flow_db: Arc<FlowDBStore>, data_db: Arc<FlowDBStore>, config: FlowConfig) -> Self {
        Self {
            flow_db,
            data_db,
            seal_manager: Default::default(),
            config,
        }
    }

    pub fn insert_subtree_list_for_batch(
        &self,
        batch_index: usize,
        subtree_list: Vec<(usize, usize, DataRoot)>,
    ) -> Result<()> {
        let start_time = Instant::now();
        let mut batch = self
            .data_db
            .get_entry_batch(batch_index as u64)?
            .unwrap_or_else(|| EntryBatch::new(batch_index as u64));
        batch.set_subtree_list(subtree_list);
        self.data_db
            .put_entry_raw(vec![(batch_index as u64, batch)])?;
        metrics::INSERT_SUBTREE_LIST.update_since(start_time);
        Ok(())
    }

    pub fn gen_proof_in_batch(&self, batch_index: usize, sector_index: usize) -> Result<FlowProof> {
        let batch = self
            .data_db
            .get_entry_batch(batch_index as u64)?
            .ok_or_else(|| anyhow!("batch missing, index={}", batch_index))?;
        let merkle = batch.to_merkle_tree(batch_index == 0)?.ok_or_else(|| {
            anyhow!(
                "batch data incomplete for building a merkle tree, index={}",
                batch_index
            )
        })?;
        merkle.gen_proof(sector_index)
    }

    pub fn delete_batch_list(&self, batch_list: &[u64]) -> Result<()> {
        self.seal_manager.delete_batch_list(batch_list);
        self.data_db.delete_batch_list(batch_list)
    }
}

#[derive(Clone, Debug)]
pub struct FlowConfig {
    pub batch_size: usize,
    pub merkle_node_cache_capacity: usize,
    pub shard_config: Arc<RwLock<ShardConfig>>,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            batch_size: SECTORS_PER_LOAD,
            // Each node takes (8+8+32=)48 Bytes, so the default value is 1.5 GB memory size.
            merkle_node_cache_capacity: 32 * 1024 * 1024,
            shard_config: Default::default(),
        }
    }
}

impl FlowRead for FlowStore {
    /// Return `Ok(None)` if only partial data are available.
    fn get_entries(&self, index_start: u64, index_end: u64) -> Result<Option<ChunkArray>> {
        if index_end <= index_start {
            bail!(
                "invalid entry index: start={} end={}",
                index_start,
                index_end
            );
        }
        let mut data = Vec::with_capacity((index_end - index_start) as usize * BYTES_PER_SECTOR);
        for (start_entry_index, end_entry_index) in
            batch_iter(index_start, index_end, self.config.batch_size)
        {
            let chunk_index = start_entry_index / self.config.batch_size as u64;
            let mut offset = start_entry_index - chunk_index * self.config.batch_size as u64;
            let mut length = end_entry_index - start_entry_index;

            // Tempfix: for first chunk, its offset is always 1
            if chunk_index == 0 && offset == 0 {
                offset = 1;
                length -= 1;
            }

            let entry_batch = try_option!(self.data_db.get_entry_batch(chunk_index)?);
            let mut entry_batch_data =
                try_option!(entry_batch.get_unsealed_data(offset as usize, length as usize));
            data.append(&mut entry_batch_data);
        }
        Ok(Some(ChunkArray {
            data,
            start_index: index_start,
        }))
    }

    fn get_available_entries(&self, index_start: u64, index_end: u64) -> Result<Vec<ChunkArray>> {
        // Both `index_start` and `index_end` are at the batch boundaries, so we do not need
        // to check if the data is within range when we process each batch.
        if index_end <= index_start
            || index_start % self.config.batch_size as u64 != 0
            || index_end % self.config.batch_size as u64 != 0
        {
            bail!(
                "invalid entry index: start={} end={}",
                index_start,
                index_end
            );
        }
        let mut entry_list = Vec::<ChunkArray>::new();
        for (start_entry_index, _) in batch_iter(index_start, index_end, self.config.batch_size) {
            let chunk_index = start_entry_index / self.config.batch_size as u64;

            if let Some(mut data_list) = self
                .data_db
                .get_entry_batch(chunk_index)?
                .map(|b| b.into_data_list(start_entry_index))
            {
                if data_list.is_empty() {
                    continue;
                }
                // This will not happen for now because we only get entries for the last chunk.
                if let Some(last) = entry_list.last_mut() {
                    if last.start_index + bytes_to_entries(last.data.len() as u64)
                        == data_list[0].start_index
                    {
                        // Merge the first element with the previous one.
                        last.data.append(&mut data_list.remove(0).data);
                    }
                }
                for data in data_list {
                    entry_list.push(data);
                }
            }
        }
        Ok(entry_list)
    }

    fn load_sealed_data(&self, chunk_index: u64) -> Result<Option<MineLoadChunk>> {
        let batch = try_option!(self.data_db.get_entry_batch(chunk_index)?);
        let mut mine_chunk = MineLoadChunk::default();
        for (seal_index, (sealed, validity)) in mine_chunk
            .loaded_chunk
            .iter_mut()
            .zip(mine_chunk.availabilities.iter_mut())
            .enumerate()
        {
            if let Some(data) = batch.get_sealed_data(seal_index as u16) {
                *validity = true;
                *sealed = data;
            }
        }
        Ok(Some(mine_chunk))
    }

    fn get_num_entries(&self) -> Result<u64> {
        // This is an over-estimation as it assumes each batch is full.
        self.data_db
            .kvdb
            .num_keys(COL_ENTRY_BATCH)
            .map(|num_batches| num_batches * PORA_CHUNK_SIZE as u64)
            .map_err(Into::into)
    }

    fn get_shard_config(&self) -> ShardConfig {
        *self.config.shard_config.read()
    }

    fn get_pad_data(&self, start_index: u64) -> crate::error::Result<Option<Vec<PadPair>>> {
        self.flow_db.get_pad_data(start_index)
    }

    fn get_pad_data_sync_height(&self) -> Result<Option<u64>> {
        self.data_db.get_pad_data_sync_height()
    }
}

impl FlowWrite for FlowStore {
    /// Return the roots of completed chunks. The order is guaranteed to be increasing
    /// by chunk index.
    fn append_entries(&self, data: ChunkArray) -> Result<Vec<(u64, DataRoot)>> {
        let start_time = Instant::now();
        let mut to_seal_set = self.seal_manager.to_seal_set.write();
        trace!("append_entries: {} {}", data.start_index, data.data.len());
        if data.data.len() % BYTES_PER_SECTOR != 0 {
            bail!("append_entries: invalid data size, len={}", data.data.len());
        }
        let mut batch_list = Vec::new();
        for (start_entry_index, end_entry_index) in batch_iter(
            data.start_index,
            data.start_index + bytes_to_entries(data.data.len() as u64),
            self.config.batch_size,
        ) {
            // TODO: Avoid mem-copy if possible.
            let chunk = data
                .sub_array(start_entry_index, end_entry_index)
                .expect("in range");

            let chunk_index = chunk.start_index / self.config.batch_size as u64;
            if !self.config.shard_config.read().in_range(chunk_index) {
                // The data are in a shard range that we are not storing.
                continue;
            }

            // TODO: Try to avoid loading from db if possible.
            let mut batch = self
                .data_db
                .get_entry_batch(chunk_index)?
                .unwrap_or_else(|| EntryBatch::new(chunk_index));
            let completed_seals = batch.insert_data(
                (chunk.start_index % self.config.batch_size as u64) as usize,
                chunk.data,
            )?;
            if self.seal_manager.seal_worker_available() {
                completed_seals.into_iter().for_each(|x| {
                    to_seal_set.insert(
                        chunk_index as usize * SEALS_PER_LOAD + x as usize,
                        self.seal_manager.to_seal_version(),
                    );
                });
            }

            batch_list.push((chunk_index, batch));
        }

        metrics::APPEND_ENTRIES.update_since(start_time);
        self.data_db.put_entry_batch_list(batch_list)
    }

    fn truncate(&self, start_index: u64) -> crate::error::Result<()> {
        let mut to_seal_set = self.seal_manager.to_seal_set.write();
        let to_reseal = self.data_db.truncate(start_index, self.config.batch_size)?;

        to_seal_set.split_off(&(start_index as usize / SECTORS_PER_SEAL));
        let new_seal_version = self.seal_manager.inc_seal_version();

        to_reseal.into_iter().for_each(|x| {
            to_seal_set.insert(x, new_seal_version);
        });
        Ok(())
    }

    fn update_shard_config(&self, shard_config: ShardConfig) {
        *self.config.shard_config.write() = shard_config;
    }

    fn put_pad_data(&self, data_sizes: &[PadPair], tx_seq: u64) -> crate::error::Result<()> {
        let start_time = Instant::now();
        let res = self.flow_db.put_pad_data(data_sizes, tx_seq);
        metrics::PUT_PAD_DATA.update_since(start_time);
        res
    }

    fn put_pad_data_sync_height(&self, sync_index: u64) -> crate::error::Result<()> {
        self.data_db.put_pad_data_sync_height(sync_index)
    }
}

impl FlowSeal for FlowStore {
    fn pull_seal_chunk(&self, seal_index_max: usize) -> Result<Option<Vec<SealTask>>> {
        let start_time = Instant::now();
        let to_seal_set = self.seal_manager.to_seal_set.read();
        self.seal_manager.update_pull_time();

        let mut to_seal_iter = to_seal_set.iter();
        let (&first_index, &first_version) = try_option!(to_seal_iter.next());
        if first_index >= seal_index_max {
            return Ok(None);
        }

        let mut tasks = Vec::with_capacity(SEALS_PER_LOAD);

        let batch_data = self
            .data_db
            .get_entry_batch((first_index / SEALS_PER_LOAD) as u64)?
            .expect("Lost data chunk in to_seal_set");

        for (&seal_index, &version) in
            std::iter::once((&first_index, &first_version)).chain(to_seal_iter.filter(|(&x, _)| {
                first_index / SEALS_PER_LOAD == x / SEALS_PER_LOAD && x < seal_index_max
            }))
        {
            let seal_index_local = seal_index % SEALS_PER_LOAD;
            let non_sealed_data = batch_data
                .get_non_sealed_data(seal_index_local as u16)
                .expect("Lost seal chunk in to_seal_set");
            tasks.push(SealTask {
                seal_index: seal_index as u64,
                version,
                non_sealed_data,
            })
        }

        metrics::PULL_SEAL_CHUNK.update_since(start_time);

        Ok(Some(tasks))
    }

    fn submit_seal_result(&self, answers: Vec<SealAnswer>) -> Result<()> {
        let mut to_seal_set = self.seal_manager.to_seal_set.write();
        let is_consistent = |answer: &SealAnswer| {
            to_seal_set
                .get(&(answer.seal_index as usize))
                .map_or(false, |cur_ver| cur_ver == &answer.version)
        };

        let mut updated_chunk = vec![];
        let mut removed_seal_index = Vec::new();
        for (load_index, answers_in_chunk) in &answers
            .into_iter()
            .filter(is_consistent)
            .chunk_by(|answer| answer.seal_index / SEALS_PER_LOAD as u64)
        {
            let mut batch_chunk = self
                .data_db
                .get_entry_batch(load_index)?
                .expect("Can not find chunk data");
            for answer in answers_in_chunk {
                removed_seal_index.push(answer.seal_index as usize);
                batch_chunk.submit_seal_result(answer)?;
            }
            updated_chunk.push((load_index, batch_chunk));
        }

        debug!("Seal chunks: indices = {:?}", removed_seal_index);

        for idx in removed_seal_index.into_iter() {
            to_seal_set.remove(&idx);
        }

        self.data_db.put_entry_raw(updated_chunk)?;

        Ok(())
    }
}

#[derive(Debug, PartialEq, DeriveEncode, DeriveDecode)]
pub struct PadPair {
    pub start_index: u64,
    pub data_size: u64,
}

pub struct FlowDBStore {
    kvdb: Arc<dyn ZgsKeyValueDB>,
}

impl FlowDBStore {
    pub fn new(kvdb: Arc<dyn ZgsKeyValueDB>) -> Self {
        Self { kvdb }
    }

    fn put_entry_batch_list(
        &self,
        batch_list: Vec<(u64, EntryBatch)>,
    ) -> Result<Vec<(u64, DataRoot)>> {
        let start_time = Instant::now();
        let mut completed_batches = Vec::new();
        let mut tx = self.kvdb.transaction();
        for (batch_index, batch) in batch_list {
            tx.put(
                COL_ENTRY_BATCH,
                &batch_index.to_be_bytes(),
                &batch.as_ssz_bytes(),
            );
            if let Some(root) = batch.build_root(batch_index == 0)? {
                trace!("complete batch: index={}", batch_index);
                completed_batches.push((batch_index, root));
            }
        }
        self.kvdb.write(tx)?;
        metrics::PUT_ENTRY_BATCH_LIST.update_since(start_time);
        Ok(completed_batches)
    }

    fn put_entry_raw(&self, batch_list: Vec<(u64, EntryBatch)>) -> Result<()> {
        let mut tx = self.kvdb.transaction();
        for (batch_index, batch) in batch_list {
            tx.put(
                COL_ENTRY_BATCH,
                &batch_index.to_be_bytes(),
                &batch.as_ssz_bytes(),
            );
        }
        self.kvdb.write(tx)?;
        Ok(())
    }

    fn get_entry_batch(&self, batch_index: u64) -> Result<Option<EntryBatch>> {
        let raw = try_option!(self.kvdb.get(COL_ENTRY_BATCH, &batch_index.to_be_bytes())?);
        Ok(Some(EntryBatch::from_ssz_bytes(&raw).map_err(Error::from)?))
    }

    fn truncate(&self, start_index: u64, batch_size: usize) -> crate::error::Result<Vec<usize>> {
        let mut tx = self.kvdb.transaction();
        let mut start_batch_index = start_index / batch_size as u64;
        let first_batch_offset = start_index as usize % batch_size;
        let mut index_to_reseal = Vec::new();
        if first_batch_offset != 0 {
            if let Some(mut first_batch) = self.get_entry_batch(start_batch_index)? {
                index_to_reseal = first_batch
                    .truncate(first_batch_offset)
                    .into_iter()
                    .map(|x| start_batch_index as usize * SEALS_PER_LOAD + x as usize)
                    .collect();
                if !first_batch.is_empty() {
                    tx.put(
                        COL_ENTRY_BATCH,
                        &start_batch_index.to_be_bytes(),
                        &first_batch.as_ssz_bytes(),
                    );
                } else {
                    tx.delete(COL_ENTRY_BATCH, &start_batch_index.to_be_bytes());
                }
            }

            start_batch_index += 1;
        }
        // TODO: `kvdb` and `kvdb-rocksdb` does not support `seek_to_last` yet.
        // We'll need to fork it or use another wrapper for a better performance in this.
        let end = match self.kvdb.iter(COL_ENTRY_BATCH).last() {
            Some(Ok((k, _))) => decode_batch_index(k.as_ref())?,
            Some(Err(e)) => {
                error!("truncate db error: e={:?}", e);
                return Err(e.into());
            }
            None => {
                // The db has no data, so we can just return;
                return Ok(index_to_reseal);
            }
        };
        for batch_index in start_batch_index as usize..=end {
            tx.delete(COL_ENTRY_BATCH, &batch_index.to_be_bytes());
        }
        self.kvdb.write(tx)?;
        Ok(index_to_reseal)
    }

    fn delete_batch_list(&self, batch_list: &[u64]) -> Result<()> {
        let mut tx = self.kvdb.transaction();
        for i in batch_list {
            tx.delete(COL_ENTRY_BATCH, &i.to_be_bytes());
        }
        Ok(self.kvdb.write(tx)?)
    }

    fn put_pad_data(&self, data_sizes: &[PadPair], tx_seq: u64) -> Result<()> {
        let mut tx = self.kvdb.transaction();

        let mut buffer = Vec::new();
        for item in data_sizes {
            buffer.extend(item.as_ssz_bytes());
        }

        tx.put(COL_PAD_DATA_LIST, &tx_seq.to_be_bytes(), &buffer);
        self.kvdb.write(tx)?;
        Ok(())
    }

    fn put_pad_data_sync_height(&self, tx_seq: u64) -> Result<()> {
        let mut tx = self.kvdb.transaction();
        tx.put(
            COL_PAD_DATA_SYNC_HEIGH,
            b"sync_height",
            &tx_seq.to_be_bytes(),
        );
        self.kvdb.write(tx)?;
        Ok(())
    }

    fn get_pad_data_sync_height(&self) -> Result<Option<u64>> {
        match self.kvdb.get(COL_PAD_DATA_SYNC_HEIGH, b"sync_height")? {
            Some(v) => Ok(Some(u64::from_be_bytes(
                v.try_into().map_err(|e| anyhow!("{:?}", e))?,
            ))),
            None => Ok(None),
        }
    }

    fn get_pad_data(&self, tx_seq: u64) -> Result<Option<Vec<PadPair>>> {
        match self.kvdb.get(COL_PAD_DATA_LIST, &tx_seq.to_be_bytes())? {
            Some(v) => Ok(Some(
                Vec::<PadPair>::from_ssz_bytes(&v).map_err(Error::from)?,
            )),
            None => Ok(None),
        }
    }
}

#[derive(DeriveEncode, DeriveDecode, Clone, Debug)]
#[ssz(enum_behaviour = "union")]
pub enum BatchRoot {
    Single(DataRoot),
    Multiple((usize, DataRoot)),
}

/// Return the batch boundaries `(batch_start_index, batch_end_index)` given the index range.
pub fn batch_iter(start: u64, end: u64, batch_size: usize) -> Vec<(u64, u64)> {
    let mut list = Vec::new();
    for i in (start / batch_size as u64 * batch_size as u64..end).step_by(batch_size) {
        let batch_start = cmp::max(start, i);
        let batch_end = cmp::min(end, i + batch_size as u64);
        list.push((batch_start, batch_end));
    }
    list
}

pub fn batch_iter_sharded(
    start: u64,
    end: u64,
    batch_size: usize,
    shard_config: ShardConfig,
) -> Vec<(u64, u64)> {
    batch_iter(start, end, batch_size)
        .into_iter()
        .filter(|(start, _)| {
            (start / batch_size as u64) % shard_config.num_shard as u64
                == shard_config.shard_id as u64
        })
        .collect()
}

fn try_decode_usize(data: &[u8]) -> Result<usize> {
    Ok(usize::from_be_bytes(
        data.try_into().map_err(|e| anyhow!("{:?}", e))?,
    ))
}

fn decode_batch_index(data: &[u8]) -> Result<usize> {
    try_decode_usize(data)
}

fn encode_mpt_node_key(layer_index: usize, position: usize) -> Vec<u8> {
    let mut key = layer_index.to_be_bytes().to_vec();
    key.extend_from_slice(&position.to_be_bytes());
    key
}

fn layer_size_key(layer: usize) -> Vec<u8> {
    let mut key = "layer_size".as_bytes().to_vec();
    key.extend_from_slice(&layer.to_be_bytes());
    key
}

pub struct NodeDBTransaction(DBTransaction);

impl NodeDatabase<DataRoot> for FlowDBStore {
    fn get_node(&self, layer: usize, pos: usize) -> Result<Option<DataRoot>> {
        Ok(self
            .kvdb
            .get(COL_FLOW_MPT_NODES, &encode_mpt_node_key(layer, pos))?
            .map(|v| DataRoot::from_slice(&v)))
    }

    fn get_layer_size(&self, layer: usize) -> Result<Option<usize>> {
        match self.kvdb.get(COL_FLOW_MPT_NODES, &layer_size_key(layer))? {
            Some(v) => Ok(Some(try_decode_usize(&v)?)),
            None => Ok(None),
        }
    }

    fn start_transaction(&self) -> Box<dyn NodeTransaction<DataRoot>> {
        Box::new(NodeDBTransaction(self.kvdb.transaction()))
    }

    fn commit(&self, tx: Box<dyn NodeTransaction<DataRoot>>) -> Result<()> {
        let db_tx: Box<NodeDBTransaction> = tx
            .into_any()
            .downcast()
            .map_err(|e| anyhow!("downcast failed, e={:?}", e))?;
        self.kvdb.write(db_tx.0).map_err(Into::into)
    }
}

impl NodeTransaction<DataRoot> for NodeDBTransaction {
    fn save_node(&mut self, layer: usize, pos: usize, node: &DataRoot) {
        self.0.put(
            COL_FLOW_MPT_NODES,
            &encode_mpt_node_key(layer, pos),
            node.as_bytes(),
        );
    }

    fn save_node_list(&mut self, nodes: &[(usize, usize, &DataRoot)]) {
        for (layer_index, position, data) in nodes {
            self.0.put(
                COL_FLOW_MPT_NODES,
                &encode_mpt_node_key(*layer_index, *position),
                data.as_bytes(),
            );
        }
    }

    fn remove_node_list(&mut self, nodes: &[(usize, usize)]) {
        for (layer_index, position) in nodes {
            self.0.delete(
                COL_FLOW_MPT_NODES,
                &encode_mpt_node_key(*layer_index, *position),
            );
        }
    }

    fn save_layer_size(&mut self, layer: usize, size: usize) {
        self.0.put(
            COL_FLOW_MPT_NODES,
            &layer_size_key(layer),
            &size.to_be_bytes(),
        );
    }

    fn remove_layer_size(&mut self, layer: usize) {
        self.0.delete(COL_FLOW_MPT_NODES, &layer_size_key(layer));
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}
