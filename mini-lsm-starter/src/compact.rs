// Copyright (c) 2022-2025 Alex Chi Z
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

#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

mod leveled;
mod simple_leveled;
mod tiered;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Ok, Result};

pub use leveled::{LeveledCompactionController, LeveledCompactionOptions, LeveledCompactionTask};
use serde::{Deserialize, Serialize};
pub use simple_leveled::{
    SimpleLeveledCompactionController, SimpleLeveledCompactionOptions, SimpleLeveledCompactionTask,
};
pub use tiered::{TieredCompactionController, TieredCompactionOptions, TieredCompactionTask};

use crate::iterators::concat_iterator::SstConcatIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::iterators::StorageIterator;

use crate::lsm_storage::{LsmStorageInner, LsmStorageState};
use crate::table::{SsTable, SsTableBuilder, SsTableIterator};

#[derive(Debug, Serialize, Deserialize)]
pub enum CompactionTask {
    Leveled(LeveledCompactionTask),
    Tiered(TieredCompactionTask),
    Simple(SimpleLeveledCompactionTask),
    /// only used when only have l0, l1 levels, so when used, always compact_to_bottom_level = true by default
    ForceFullCompaction {
        l0_sstables: Vec<usize>,
        l1_sstables: Vec<usize>,
    },
}

impl CompactionTask {
    fn compact_to_bottom_level(&self) -> bool {
        match self {
            CompactionTask::ForceFullCompaction { .. } => true,
            CompactionTask::Leveled(task) => task.is_lower_level_bottom_level,
            CompactionTask::Simple(task) => task.is_lower_level_bottom_level,
            CompactionTask::Tiered(task) => task.bottom_tier_included,
        }
    }
}

pub(crate) enum CompactionController {
    Leveled(LeveledCompactionController),
    Tiered(TieredCompactionController),
    Simple(SimpleLeveledCompactionController),
    NoCompaction,
}

impl CompactionController {
    pub fn generate_compaction_task(&self, snapshot: &LsmStorageState) -> Option<CompactionTask> {
        match self {
            CompactionController::Leveled(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Leveled),
            CompactionController::Simple(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Simple),
            CompactionController::Tiered(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Tiered),
            CompactionController::NoCompaction => unreachable!(),
        }
    }

    pub fn apply_compaction_result(
        &self,
        snapshot: &LsmStorageState,
        task: &CompactionTask,
        output: &[usize],
        in_recovery: bool,
    ) -> (LsmStorageState, Vec<usize>) {
        match (self, task) {
            (CompactionController::Leveled(ctrl), CompactionTask::Leveled(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output, in_recovery)
            }
            (CompactionController::Simple(ctrl), CompactionTask::Simple(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            (CompactionController::Tiered(ctrl), CompactionTask::Tiered(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            _ => unreachable!(),
        }
    }
}

impl CompactionController {
    pub fn flush_to_l0(&self) -> bool {
        matches!(
            self,
            Self::Leveled(_) | Self::Simple(_) | Self::NoCompaction
        )
    }
}

#[derive(Debug, Clone)]
pub enum CompactionOptions {
    /// Leveled compaction with partial compaction + dynamic level support (= RocksDB's Leveled
    /// Compaction)
    Leveled(LeveledCompactionOptions),
    /// Tiered compaction (= RocksDB's universal compaction)
    Tiered(TieredCompactionOptions),
    /// Simple leveled compaction
    Simple(SimpleLeveledCompactionOptions),
    /// In no compaction mode (week 1), always flush to L0
    NoCompaction,
}

impl LsmStorageInner {
    fn compact(&self, task: &CompactionTask) -> Result<Vec<Arc<SsTable>>> {
        let mut ret = vec![];

        if let CompactionTask::ForceFullCompaction {
            l0_sstables,
            l1_sstables,
        } = task
        {
            let mut m_it = vec![];
            let state = self.state.read().clone();
            for i in l0_sstables.iter() {
                let t = state.sstables[i].clone();
                let s = SsTableIterator::create_and_seek_to_first(t.clone())?;
                m_it.push(Box::new(s));
            }
            let s_0_it = MergeIterator::create(m_it);

            let mut s_1 = vec![];
            for i in l1_sstables.iter() {
                let t = state.sstables[i].clone();
                s_1.push(t);
            }
            let c_it = SstConcatIterator::create_and_seek_to_first(s_1)?;

            let mut s_it = TwoMergeIterator::create(s_0_it, c_it)?;
            let mut builder = SsTableBuilder::new(self.options.block_size);

            while s_it.is_valid() {
                if builder.estimated_size() >= self.options.target_sst_size {
                    let sst_id = self.next_sst_id();
                    let sst = builder.build(
                        sst_id,
                        Some(self.block_cache.clone()),
                        self.path_of_sst(sst_id),
                    )?;
                    ret.push(Arc::new(sst));
                    builder = SsTableBuilder::new(self.options.block_size);
                }
                // since compact_to_bottom_level always true in ForceFullCompaction, so like in a full tree compaction,
                // just rm the delete tombstone directly
                if !s_it.value().is_empty() {
                    builder.add(s_it.key(), s_it.value());
                }
                s_it.next()?;
            }

            if !builder.is_empty() {
                let sst_id = self.next_sst_id();
                let sst = builder.build(
                    sst_id,
                    Some(self.block_cache.clone()),
                    self.path_of_sst(sst_id),
                )?;
                ret.push(Arc::new(sst));
            }
        }

        Ok(ret)
    }

    pub fn force_full_compaction(&self) -> Result<()> {
        let state = self.state.read().clone();
        let ssts_to_compact = (&state.l0_sstables, &state.levels[0].1);
        let task = CompactionTask::ForceFullCompaction {
            l0_sstables: ssts_to_compact.0.clone(),
            l1_sstables: ssts_to_compact.1.clone(),
        };

        let new_ssts = self.compact(&task)?;

        {
            let _state_lock = self.state_lock.lock();
            let mut guard = self.state.write();
            let mut snashot = guard.as_ref().clone();

            ssts_to_compact
                .0
                .iter()
                .chain(ssts_to_compact.1)
                .for_each(|id| {
                    snashot.sstables.remove(id);
                });
            snashot.levels[0].1 = new_ssts.iter().map(|t| t.sst_id()).collect();
            new_ssts.iter().for_each(|id| {
                snashot.sstables.insert(id.sst_id(), id.clone());
            });
            let l0_rm = ssts_to_compact.0.iter().collect::<HashSet<_>>();
            // might have new l0 insert into snashot.l0_sstables during compact
            snashot.l0_sstables.retain(|id| !l0_rm.contains(id));

            *guard = Arc::new(snashot);
        }

        for id in ssts_to_compact.0.iter().chain(ssts_to_compact.1) {
            std::fs::remove_file(self.path_of_sst(*id))?;
        }

        Ok(())
    }

    fn trigger_compaction(&self) -> Result<()> {
        // self.force_full_compaction()
        Ok(())
    }

    pub(crate) fn spawn_compaction_thread(
        self: &Arc<Self>,
        rx: crossbeam_channel::Receiver<()>,
    ) -> Result<Option<std::thread::JoinHandle<()>>> {
        if let CompactionOptions::Leveled(_)
        | CompactionOptions::Simple(_)
        | CompactionOptions::Tiered(_) = self.options.compaction_options
        {
            let this = self.clone();
            let handle = std::thread::spawn(move || {
                let ticker = crossbeam_channel::tick(Duration::from_millis(50));
                loop {
                    crossbeam_channel::select! {
                        recv(ticker) -> _ => if let Err(e) = this.trigger_compaction() {
                            eprintln!("compaction failed: {}", e);
                        },
                        recv(rx) -> _ => return
                    }
                }
            });
            return Ok(Some(handle));
        }
        Ok(None)
    }

    fn trigger_flush(&self) -> Result<()> {
        let state = self.state.read().clone();
        if state.imm_memtables.len() + 1 > self.options.num_memtable_limit {
            self.force_flush_next_imm_memtable()?;
        }

        Ok(())
    }

    pub(crate) fn spawn_flush_thread(
        self: &Arc<Self>,
        rx: crossbeam_channel::Receiver<()>,
    ) -> Result<Option<std::thread::JoinHandle<()>>> {
        let this = self.clone();
        let handle = std::thread::spawn(move || {
            let ticker = crossbeam_channel::tick(Duration::from_millis(50));
            loop {
                crossbeam_channel::select! {
                    recv(ticker) -> _ => if let Err(e) = this.trigger_flush() {
                        eprintln!("flush failed: {}", e);
                    },
                    recv(rx) -> _ => return
                }
            }
        });
        Ok(Some(handle))
    }
}
