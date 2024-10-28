use crate::common::node_configs::{ContractConfiguration, StoreConfiguration};
use crate::common::node_error::NodeError;
use crate::contract::chain_brief::ChainBrief;
use crate::models::account_audit_row::AccountAuditRow;
use crate::models::brief_model::convert_chain_briefs_to_brief_records;
use crate::models::transaction_model::TransactionRow;
use crate::models::bridge_transaction_model::{BridgeTxRecord, BridgeTxRow};
use crate::repositories::account_audit_repo::AccountAuditRepo;
use crate::repositories::block_repo::BlockRepo;
use crate::repositories::bridge_tx_repo::BridgeTxRepo;
use crate::repositories::brief_repo::BriefRepo;
use crate::repositories::chain_repo::ChainRepo;
use crate::repositories::transaction_repo::TransactionRepo;
use crate::utils::store_util::{create_one, create_pool, PgConnectionPool};
use crate::utils::uuid_util::generate_uuid;
use log::{error, info};
use postgres::Client;
use rocksdb::DB;
use solana_sdk::pubkey::Pubkey;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

pub struct ExecuteService {
    client_pool: PgConnectionPool,
    client_one: Client,
    l2_msg_program_id: String,
    rocksdb: Arc<RwLock<DB>>,
    monitor_rocksdb_slot: Arc<RwLock<DB>>,
    initial_slot: u64,
}

impl ExecuteService {
    pub fn new(config: &StoreConfiguration, contract: &ContractConfiguration, is_filter: bool) -> Result<Self, NodeError> {
        let pool = create_pool(
            config.to_owned(),
            10,
        );

        let one = create_one(config.to_owned());

        let l2_msg_program_id = contract.l2_message_program_id.clone();
        if is_filter {
            let slot_dir = Path::new("./relayer/slot");
            let slot_db = DB::open_default(slot_dir).unwrap();
            let rocksdb = Arc::new(RwLock::new(slot_db));
            let monitor_slot_dir = Path::new("./relayer/monitor-slot-tmp");
            let monitor_slot_db = DB::open_default(monitor_slot_dir).unwrap();
            let monitor_rocksdb_slot = Arc::new(RwLock::new(monitor_slot_db));
            info!("Created PostgresClient.");

            Ok(Self {
                client_pool: pool,
                client_one: one,
                l2_msg_program_id,
                rocksdb,
                monitor_rocksdb_slot,
                initial_slot: 2,
            })
        }else {
            let slot_dir = Path::new("./relayer/slot-tmp");
            let slot_db = DB::open_default(slot_dir).unwrap();
            let rocksdb = Arc::new(RwLock::new(slot_db));

            let monitor_slot_dir = Path::new("./relayer/monitor-slot");
            let monitor_slot_db = DB::open_default(monitor_slot_dir).unwrap();
            let monitor_rocksdb_slot = Arc::new(RwLock::new(monitor_slot_db));
            info!("Created PostgresClient.");

            Ok(Self {
                client_pool: pool,
                client_one: one,
                l2_msg_program_id,
                rocksdb,
                monitor_rocksdb_slot,
                initial_slot: 2,
            })
        }
        
    }

    pub fn get_account_audits(&self, from_slot: i64, to_slot: i64) -> Result<Vec<AccountAuditRow>, NodeError> {
        let repo = AccountAuditRepo { pool: Box::from(self.client_pool.to_owned()) };

        let rows = repo.range(from_slot, to_slot)?;

        Ok(rows)
    }

    pub fn get_transactions(&mut self, from_slot: i64, to_slot: i64) -> Result<Vec<TransactionRow>, NodeError> {
        let mut repo = TransactionRepo { one: &mut self.client_one };

        let rows = repo.range(from_slot, to_slot)?;
        Ok(rows)
    }

    pub fn get_initial_slot(&self) -> Result<i64, NodeError> {
        Ok(self.initial_slot as i64)
    }

    pub fn get_last_slot(&self) -> Result<i64, NodeError> {
        let mut repo = ChainRepo { db: &self.rocksdb };

        let slot = repo.show().unwrap_or(0);

        Ok(slot)
    }

    pub fn get_last_slot_for_monitor(&self) -> Result<i64, NodeError> {
        let mut repo = ChainRepo{ db: &self.monitor_rocksdb_slot };

        let slot = repo.show().unwrap_or(0);
        
        Ok(slot)
    }

    pub fn update_last_slot_for_monitor(&self, slot: i64) {
        let mut repo = ChainRepo { db: &self.monitor_rocksdb_slot };

        repo.upsert(slot);
    }

    pub fn get_max_slot(&mut self) -> Result<i64, NodeError> {
        let mut repo = BlockRepo { one: &mut self.client_one };

        match repo.show() {
            Ok(row) => {
                Ok(row.slot)
            }
            Err(e) => {
                Ok(0)
            }
        }
    }

    pub fn update_last_slot(&self, slot: i64) {
        let mut repo = ChainRepo { db: &self.rocksdb };

        repo.upsert(slot);
    }

    pub fn insert_briefs(&self, chain_briefs: Vec<ChainBrief>) -> Result<u32, NodeError> {
        let repo = BriefRepo { pool: Box::from(self.client_pool.to_owned()) };

        let brief_records = convert_chain_briefs_to_brief_records(chain_briefs);

        let rows = repo.insert(brief_records)?;

        let count = rows.len() as u32;

        Ok(count)
    }

    pub fn filter_bridge_tx(&mut self, start_slot: i64, end_slot: i64) -> Result<Vec<BridgeTxRecord>, NodeError> {
        if end_slot < start_slot {
            error!("end_slot should greater than or equal start_slot  start_slot: {:?},end_slot: {:?}",
                start_slot,end_slot);
            return Err(
                NodeError::new(generate_uuid(),
                               format!("end_slot should greater than or equal start_slot  start_slot: {:?},end_slot: {:?}",
                                       start_slot, end_slot),
                )
            );
        }

        if start_slot < self.initial_slot as i64 || end_slot < self.initial_slot as i64 {
            error!("start_slot and end_slot should greater than initial_slot  start_slot: {:?}, end_slot: {:?}, initial_slot: {:?}",
                start_slot,end_slot,self.initial_slot);
            return Err(
                NodeError::new(generate_uuid(),
                               format!("start_slot and end_slot should greater than initial_slot  start_slot: {:?}, end_slot: {:?}, initial_slot: {:?}",
                                       start_slot, end_slot, self.initial_slot),
                )
            );
        }

        let transactions = self.get_transactions(start_slot, end_slot)?;
        let mut bridge_txs: Vec<BridgeTxRecord> = Vec::new();

        for transaction in transactions.clone() {
            let msg = transaction.clone().legacy_message.unwrap();
            let pks: Vec<Pubkey> = msg.account_keys.iter().map(|ak| Pubkey::try_from(ak.as_slice()).unwrap()).collect();

            if pks.contains(&Pubkey::from_str(&self.l2_msg_program_id).unwrap()) {
                info!("push!!!!!");
                bridge_txs.push(BridgeTxRecord::from(transaction));
            }
        }
        
        info!("tx len: {}", bridge_txs.len());
        Ok(bridge_txs)
    }

    pub fn insert_bridge_txs(&self, bridge_txs: Vec<BridgeTxRecord>) -> Result<u32, NodeError> {
        let repo = BridgeTxRepo{pool: Box::from(self.client_pool.to_owned())};

        let rows = repo.insert(bridge_txs)?;
        let count = rows.len() as u32;

        Ok(count)
    }

    pub fn brige_txs_hashes(&self, from_slot: i64, to_slot: i64) -> Result<Vec<String>, NodeError> {
        let repo = BridgeTxRepo{pool: Box::from(self.client_pool.to_owned())};

        repo.bridge_tx_hashes(from_slot, to_slot)
    }

    pub fn bridge_tx_range(&self, from_slot: i64, to_slot: i64) -> Result<Vec<BridgeTxRecord>, NodeError>{
        let repo = BridgeTxRepo{pool: Box::from(self.client_pool.to_owned())};

        let bridge_tx_rows = repo.range(from_slot, to_slot).unwrap();

        let bridge_tx_records = bridge_tx_rows.into_iter().map(BridgeTxRecord::from).collect();
        
        Ok(bridge_tx_records)

    }

    pub fn bridge_tx_update(&self, brige_tx_record: BridgeTxRecord) -> Result<BridgeTxRow, NodeError>{
        let repo = BridgeTxRepo{pool: Box::from(self.client_pool.to_owned())};

        let row = repo.update(brige_tx_record).unwrap();
        
        Ok(row)
    }
}


