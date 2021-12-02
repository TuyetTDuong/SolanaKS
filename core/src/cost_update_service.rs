//! this service receives instruction ExecuteTimings from replay_stage,
//! update cost_model which is shared with banking_stage to optimize
//! packing transactions into block; it also triggers persisting cost
//! table to blockstore.

use solana_ledger::blockstore::Blockstore;
use solana_measure::measure::Measure;
use solana_runtime::{bank::Bank, bank::ExecuteTimings, cost_model::CostModel};
use solana_sdk::timing::timestamp;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
        Arc, RwLock,
    },
    thread::{self, Builder, JoinHandle},
    time::Duration,
};

#[derive(Default)]
pub struct CostUpdateServiceTiming {
    last_print: u64,
    update_cost_model_count: u64,
    update_cost_model_elapsed: u64,
    persist_cost_table_elapsed: u64,
}

impl CostUpdateServiceTiming {
    fn update(
        &mut self,
        update_cost_model_count: u64,
        update_cost_model_elapsed: u64,
        persist_cost_table_elapsed: u64,
    ) {
        self.update_cost_model_count += update_cost_model_count;
        self.update_cost_model_elapsed += update_cost_model_elapsed;
        self.persist_cost_table_elapsed += persist_cost_table_elapsed;

        let now = timestamp();
        let elapsed_ms = now - self.last_print;
        if elapsed_ms > 1000 {
            datapoint_info!(
                "cost-update-service-stats",
                ("total_elapsed_us", elapsed_ms * 1000, i64),
                (
                    "update_cost_model_count",
                    self.update_cost_model_count as i64,
                    i64
                ),
                (
                    "update_cost_model_elapsed",
                    self.update_cost_model_elapsed as i64,
                    i64
                ),
                (
                    "persist_cost_table_elapsed",
                    self.persist_cost_table_elapsed as i64,
                    i64
                ),
            );

            *self = CostUpdateServiceTiming::default();
            self.last_print = now;
        }
    }
}

pub enum CostUpdate {
    FrozenBank { bank: Arc<Bank> },
    ExecuteTiming { execute_timings: ExecuteTimings },
}

pub type CostUpdateReceiver = Receiver<CostUpdate>;

pub struct CostUpdateService {
    thread_hdl: JoinHandle<()>,
}

impl CostUpdateService {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        cost_model: Arc<RwLock<CostModel>>,
        cost_update_receiver: CostUpdateReceiver,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solana-cost-update-service".to_string())
            .spawn(move || {
                Self::service_loop(exit, blockstore, cost_model, cost_update_receiver);
            })
            .unwrap();

        Self { thread_hdl }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }

    fn service_loop(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        cost_model: Arc<RwLock<CostModel>>,
        cost_update_receiver: CostUpdateReceiver,
    ) {
        let mut cost_update_service_timing = CostUpdateServiceTiming::default();
        let mut dirty: bool;
        let mut update_count: u64;
        let wait_timer = Duration::from_millis(100);

        loop {
            if exit.load(Ordering::Relaxed) {
                break;
            }

            dirty = false;
            update_count = 0_u64;
            let mut update_cost_model_time = Measure::start("update_cost_model_time");
            for cost_update in cost_update_receiver.try_iter() {
                match cost_update {
                    CostUpdate::FrozenBank { bank } => {
                        bank.read_cost_tracker().unwrap().report_stats(bank.slot());
                    }
                    CostUpdate::ExecuteTiming { execute_timings } => {
                        dirty |= Self::update_cost_model(&cost_model, &execute_timings);
                        update_count += 1;
                    }
                }
            }
            update_cost_model_time.stop();

            let mut persist_cost_table_time = Measure::start("persist_cost_table_time");
            if dirty {
                Self::persist_cost_table(&blockstore, &cost_model);
            }
            persist_cost_table_time.stop();

            cost_update_service_timing.update(
                update_count,
                update_cost_model_time.as_us(),
                persist_cost_table_time.as_us(),
            );

            thread::sleep(wait_timer);
        }
    }

    fn update_cost_model(cost_model: &RwLock<CostModel>, execute_timings: &ExecuteTimings) -> bool {
        let mut dirty = false;
        {
            let mut cost_model_mutable = cost_model.write().unwrap();
            for (program_id, timing) in &execute_timings.details.per_program_timings {
                if timing.count < 1 {
                    continue;
                }
                let units = timing.accumulated_units / timing.count as u64;
                match cost_model_mutable.upsert_instruction_cost(program_id, units) {
                    Ok(c) => {
                        debug!(
                            "after replayed into bank, instruction {:?} has averaged cost {}",
                            program_id, c
                        );
                        dirty = true;
                    }
                    Err(err) => {
                        debug!(
                        "after replayed into bank, instruction {:?} failed to update cost, err: {}",
                        program_id, err
                    );
                    }
                }
            }
        }
        debug!(
           "after replayed into bank, updated cost model instruction cost table, current values: {:?}",
           cost_model.read().unwrap().get_instruction_cost_table()
        );
        dirty
    }

    fn persist_cost_table(blockstore: &Blockstore, cost_model: &RwLock<CostModel>) {
        let cost_model_read = cost_model.read().unwrap();
        let cost_table = cost_model_read.get_instruction_cost_table();
        let db_records = blockstore.read_program_costs().expect("read programs");

        // delete records from blockstore if they are no longer in cost_table
        db_records.iter().for_each(|(pubkey, _)| {
            if cost_table.get(pubkey).is_none() {
                blockstore
                    .delete_program_cost(pubkey)
                    .expect("delete old program");
            }
        });

        for (key, cost) in cost_table.iter() {
            blockstore
                .write_program_cost(key, cost)
                .expect("persist program costs to blockstore");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program_runtime::instruction_processor::ProgramTiming;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn test_update_cost_model_with_empty_execute_timings() {
        let cost_model = Arc::new(RwLock::new(CostModel::default()));
        let empty_execute_timings = ExecuteTimings::default();
        CostUpdateService::update_cost_model(&cost_model, &empty_execute_timings);

        assert_eq!(
            0,
            cost_model
                .read()
                .unwrap()
                .get_instruction_cost_table()
                .len()
        );
    }

    #[test]
    fn test_update_cost_model_with_execute_timings() {
        let cost_model = Arc::new(RwLock::new(CostModel::default()));
        let mut execute_timings = ExecuteTimings::default();

        let program_key_1 = Pubkey::new_unique();
        let mut expected_cost: u64;

        // add new program
        {
            let accumulated_us: u64 = 1000;
            let accumulated_units: u64 = 100;
            let count: u32 = 10;
            expected_cost = accumulated_units / count as u64;

            execute_timings.details.per_program_timings.insert(
                program_key_1,
                ProgramTiming {
                    accumulated_us,
                    accumulated_units,
                    count,
                },
            );
            CostUpdateService::update_cost_model(&cost_model, &execute_timings);
            assert_eq!(
                1,
                cost_model
                    .read()
                    .unwrap()
                    .get_instruction_cost_table()
                    .len()
            );
            assert_eq!(
                Some(&expected_cost),
                cost_model
                    .read()
                    .unwrap()
                    .get_instruction_cost_table()
                    .get(&program_key_1)
            );
        }

        // update program
        {
            let accumulated_us: u64 = 2000;
            let accumulated_units: u64 = 200;
            let count: u32 = 10;
            // to expect new cost is Average(new_value, existing_value)
            expected_cost = ((accumulated_units / count as u64) + expected_cost) / 2;

            execute_timings.details.per_program_timings.insert(
                program_key_1,
                ProgramTiming {
                    accumulated_us,
                    accumulated_units,
                    count,
                },
            );
            CostUpdateService::update_cost_model(&cost_model, &execute_timings);
            assert_eq!(
                1,
                cost_model
                    .read()
                    .unwrap()
                    .get_instruction_cost_table()
                    .len()
            );
            assert_eq!(
                Some(&expected_cost),
                cost_model
                    .read()
                    .unwrap()
                    .get_instruction_cost_table()
                    .get(&program_key_1)
            );
        }
    }
}
