use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use async_raft::NodeId;
use async_raft::async_trait::async_trait;
use async_raft::raft::{Entry, EntryPayload, MembershipConfig};
use async_raft::storage::{CurrentSnapshotData, HardState, InitialState, RaftStorage};
use heed::types::{OwnedType, Str};
use heed::{Database, Env, EnvOpenOptions, PolyDatabase};
use meilisearch_core::{Database as Db, DatabaseOptions};
use indexmap::IndexMap;
use log::{debug, error, info};
use serde_json::Value;
use tokio::fs::File;

use super::raft_service::NodeState;
use super::{snapshot::RaftSnapshot, ClientRequest, ClientResponse, Message};
use crate::Data;

const ERR_INCONSISTENT_LOG: &str =
    "a query was received which was expecting data to be in place which does not exist in the log";

const MEMBERSHIP_CONFIG_KEY: &str = "membership";
const HARD_STATE_KEY: &str = "hard_state";
const LAST_APPLIED_KEY: &str = "last_commited";
const SNAPSHOT_PATH_KEY: &str = "snapshot_path";

const LOG_DB_SIZE: usize = 10 * 1024 * 1024 * 1024; //10GB

macro_rules! derive_heed {
    ($type:ty, $name:ident) => {
        struct $name;

        impl<'a> heed::BytesDecode<'a> for $name {
            type DItem = $type;

            fn bytes_decode(bytes: &'a [u8]) -> Option<Self::DItem> {
                bincode::deserialize(bytes).ok()
            }
        }

        impl<'a> heed::BytesEncode<'a> for $name {
            type EItem = $type;

            fn bytes_encode(item: &Self::EItem) -> Option<std::borrow::Cow<'a, [u8]>> {
                let bytes = bincode::serialize(item).ok()?;
                Some(std::borrow::Cow::Owned(bytes))
            }
        }
    };
}

derive_heed!(MembershipConfig, HeedMembershipConfig);
derive_heed!(HardState, HeedHardState);
derive_heed!(Entry<ClientRequest>, HeedEntry);
derive_heed!(RaftSnapshot, HeedRaftSnapshot);

pub struct RaftStore {
    pub id: NodeId,
    db: PolyDatabase,
    logs: Database<OwnedType<u64>, HeedEntry>,
    env: Env,
    store: Data,
    snapshot_dir: PathBuf,
    next_serial: AtomicU64,
}

impl RaftStore {
    pub fn new(id: NodeId, db_path: PathBuf, store: Data, snapshot_dir: PathBuf) -> Result<Self> {
        let env = EnvOpenOptions::new()
            .max_dbs(10)
            .map_size(LOG_DB_SIZE)
            .open(db_path)?;
        let db = match env.open_poly_database(Some("meta"))? {
            Some(db) => db,
            None => env.create_poly_database(Some("meta"))?,
        };
        let logs = match env.open_database::<OwnedType<u64>, HeedEntry>(Some("logs"))? {
            Some(db) => db,
            None => env.create_database(Some("logs"))?,
        };
        let next_id = AtomicU64::new(0);

        debug!("Opened database");
        Ok(Self {
            id,
            env,
            db,
            logs,
            next_serial: next_id,
            store,
            snapshot_dir,
        })
    }
}

impl RaftStore {
    fn hard_state(&self, txn: &heed::RoTxn) -> Result<Option<HardState>> {
        Ok(self.db.get::<_, Str, HeedHardState>(txn, HARD_STATE_KEY)?)
    }

    fn set_hard_state(&self, txn: &mut heed::RwTxn, hs: &HardState) -> Result<()> {
        Ok(self
            .db
            .put::<_, Str, HeedHardState>(txn, HARD_STATE_KEY, hs)?)
    }

    fn last_applied_log(&self, txn: &heed::RoTxn) -> Result<Option<u64>> {
        Ok(self
            .db
            .get::<_, Str, OwnedType<u64>>(txn, LAST_APPLIED_KEY)?)
    }

    fn set_last_applied_log(&self, txn: &mut heed::RwTxn, last_applied: u64) -> Result<()> {
        self.db
            .put::<_, Str, OwnedType<u64>>(txn, LAST_APPLIED_KEY, &last_applied)?;
        Ok(())
    }

    fn membership_config(&self, txn: &heed::RoTxn) -> Result<Option<MembershipConfig>> {
        Ok(self
            .db
            .get::<_, Str, HeedMembershipConfig>(txn, MEMBERSHIP_CONFIG_KEY)?)
    }

    fn set_membership_config(&self, txn: &mut heed::RwTxn, cfg: &MembershipConfig) -> Result<()> {
        Ok(self
            .db
            .put::<_, Str, HeedMembershipConfig>(txn, MEMBERSHIP_CONFIG_KEY, cfg)?)
    }

    fn current_snapshot(&self, txn: &heed::RoTxn) -> Result<Option<RaftSnapshot>> {
        Ok(self
            .db
            .get::<_, Str, HeedRaftSnapshot>(txn, SNAPSHOT_PATH_KEY)?)
    }

    fn current_snapshot_txn(&self) -> Result<Option<RaftSnapshot>> {
        let txn = self.env.read_txn()?;
        self.current_snapshot(&txn)
    }

    fn set_current_snapshot(&self, txn: &mut heed::RwTxn, snapshot: &RaftSnapshot) -> Result<()> {
        Ok(self
            .db
            .put::<_, Str, HeedRaftSnapshot>(txn, SNAPSHOT_PATH_KEY, snapshot)?)
    }

    fn put_log(
        &self,
        txn: &mut heed::RwTxn,
        index: u64,
        entry: &Entry<ClientRequest>,
    ) -> Result<()> {
        // keep track of the latest membership config
        match entry.payload {
            EntryPayload::ConfigChange(ref cfg) => {
                self.set_membership_config(txn, &cfg.membership)?
            }
            _ => (),
        }
        self.logs.put(txn, &index, entry)?;
        Ok(())
    }

    fn generate_snapshot_id(&self) -> String {
        let id = self.next_serial.fetch_add(1, Ordering::Relaxed);
        format!("snapshot-{}", id)
    }

    fn apply_message(&self, message: Message) -> Result<ClientResponse> {
        match message {
            Message::CreateIndex(ref index_info) => {
                let result = self
                    .store
                    .create_index(index_info)
                    .map_err(|e| e.to_string());
                info!(
                    "Created index: {}",
                    index_info.uid.as_deref().unwrap_or_default()
                );
                Ok(ClientResponse::IndexUpdate(result))
            }
            Message::DocumentAddition {
                update_query,
                index_uid,
                documents,
                partial,
            } => {
                let documents: Vec<IndexMap<String, Value>> = serde_json::from_str(&documents)?;
                match self.store.update_multiple_documents(
                    &index_uid,
                    update_query,
                    documents,
                    partial,
                ) {
                    Ok(r) => {
                        info!("Added documents to index: {}", index_uid);
                        Ok(ClientResponse::UpdateResponse(Ok(r)))
                    }
                    Err(r) => {
                        error!("Error adding documents: {}", r);
                        Ok(ClientResponse::UpdateResponse(Err(r.to_string())))
                    }
                }
            }
            Message::UpdateIndex { index_uid, update } => {
                let result = self
                    .store
                    .update_index(&index_uid, update)
                    .map_err(|e| e.to_string());
                info!("Updated index: {}", index_uid);
                Ok(ClientResponse::IndexUpdate(result))
            }
            Message::DeleteIndex(index_uid) => {
                let result = self
                    .store
                    .delete_index(&index_uid)
                    .map_err(|e| e.to_string());
                info!("Deleted index: {}", index_uid);
                Ok(ClientResponse::DeleteIndex(result))
            }
            Message::SettingsUpdate { index_uid, update } => {
                let result = self
                    .store
                    .update_settings(&index_uid, update)
                    .map_err(|e| e.to_string());
                info!("Update settings for index: {}", index_uid);
                Ok(ClientResponse::UpdateResponse(result))
            }
            Message::DocumentsDeletion { index_uid, ids } => {
                let result = self
                    .store
                    .delete_documents(&index_uid, ids)
                    .map_err(|e| e.to_string());
                info!("Deleted documents for index: {}", index_uid);
                Ok(ClientResponse::UpdateResponse(result))
            }
            Message::ClearAllDocuments { index_uid } => {
                let result = self
                    .store
                    .clear_all_documents(&index_uid)
                    .map_err(|e| e.to_string());
                info!("Deleted all documents for index: {}", index_uid);
                Ok(ClientResponse::UpdateResponse(result))
            }
        }
    }

    fn snapshot_path_from_id(&self, id: &str) -> PathBuf {
        self.snapshot_dir.join(format!("{}.snap", id))
    }

    fn create_snapshot_and_compact(&self, through: u64) -> Result<RaftSnapshot> {
        let mut txn = self.env.write_txn()?;

        // 1. get term
        let term = self
            .logs
            .get(&txn, &through)?
            .ok_or_else(|| anyhow::anyhow!(ERR_INCONSISTENT_LOG))?
            .term;
        // 2. snapshot_id is term-index
        let snapshot_id = self.generate_snapshot_id();

        // 3. get current membership config
        let membership_config = self
            .membership_config(&txn)?
            .unwrap_or_else(|| MembershipConfig::new_initial(self.id));

        // 4. create snapshot file
        let snapshot_path_temp = self.snapshot_dir.join("temp.snap");
        crate::snapshot::create_snapshot(&self.store, &snapshot_path_temp)?;
        // snapshot is finished, rename it:
        let snapshot_path = self.snapshot_path_from_id(&snapshot_id);
        std::fs::rename(snapshot_path_temp, snapshot_path.clone())?;

        // 6. insert new snapshot entry
        let entry = Entry::new_snapshot_pointer(
            through,
            term,
            snapshot_id.clone(),
            membership_config.clone(),
        );

        self.logs.delete_range(&mut txn, &(..=through))?;

        self.put_log(&mut txn, through, &entry)?;

        let raft_snapshot = RaftSnapshot {
            path: snapshot_path,
            id: snapshot_id,
            index: through,
            term,
            membership: membership_config,
        };

        self.set_current_snapshot(&mut txn, &raft_snapshot)?;

        txn.commit()?;
        Ok(raft_snapshot)
    }

    /// Returns the current state of the node
    pub async fn state(&self) -> Result<NodeState> {
        let members = self.get_membership_config().await?.members;
        if members.len() <= 1 {
            Ok(NodeState::Uninitialized)
        } else {
            Ok(NodeState::Initialized)
        }
    }
}

#[async_trait]
impl RaftStorage<ClientRequest, ClientResponse> for RaftStore {
    type Snapshot = tokio::fs::File;

    async fn get_membership_config(&self) -> Result<MembershipConfig> {
        let txn = self.env.read_txn()?;
        Ok(self
            .membership_config(&txn)?
            .unwrap_or_else(|| MembershipConfig::new_initial(self.id)))
    }

    async fn get_initial_state(&self) -> Result<InitialState> {
        let membership = self.get_membership_config().await?;
        let mut txn = self.env.write_txn()?;
        let hs = self.hard_state(&txn)?;
        let last_applied_log = self.last_applied_log(&txn)?.unwrap_or_default();
        let state = match hs {
            Some(inner) => {
                let last_entry = self.logs.last(&txn)?;
                let (last_log_index, last_log_term) = match last_entry {
                    Some((_, entry)) => (entry.index, entry.term),
                    None => (0, 0),
                };
                InitialState {
                    last_log_index,
                    last_log_term,
                    last_applied_log,
                    hard_state: inner.clone(),
                    membership,
                }
            }
            None => {
                let new = InitialState::new_initial(self.id);
                self.set_hard_state(&mut txn, &new.hard_state)?;
                new
            }
        };
        txn.commit()?;
        Ok(state)
    }

    async fn save_hard_state(&self, hs: &HardState) -> Result<()> {
        let mut txn = self.env.write_txn()?;
        self.set_hard_state(&mut txn, hs)?;
        txn.commit()?;
        Ok(())
    }

    async fn get_log_entries(&self, start: u64, stop: u64) -> Result<Vec<Entry<ClientRequest>>> {
        let txn = self.env.read_txn()?;
        let entries = if start == stop {
            let entry = self.logs.get(&txn, &start)?;
            let mut entries = vec![];
            if let Some(entry) = entry {
                entries.push(entry);
            }
            entries
        } else {
            self.logs
                .range(&txn, &(start..=stop))?
                .filter_map(|e| e.ok().map(|(_, e)| e))
                .collect()
        };
        Ok(entries)
    }

    async fn delete_logs_from(&self, start: u64, stop: Option<u64>) -> Result<()> {
        let mut txn = self.env.write_txn()?;
        match stop {
            Some(stop) => self.logs.delete_range(&mut txn, &(start..stop))?,
            None => self.logs.delete_range(&mut txn, &(start..))?,
        };
        txn.commit()?;
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn append_entry_to_log(
        &self,
        entry: &async_raft::raft::Entry<ClientRequest>,
    ) -> Result<()> {
        let mut txn = self.env.write_txn()?;
        let index = entry.index;
        self.put_log(&mut txn, index, &entry)?;
        txn.commit()?;
        Ok(())
    }

    async fn replicate_to_log(
        &self,
        entries: &[async_raft::raft::Entry<ClientRequest>],
    ) -> Result<()> {
        let mut txn = self.env.write_txn()?;
        for entry in entries {
            let index = entry.index;
            self.put_log(&mut txn, index, &entry)?;
        }
        txn.commit()?;
        Ok(())
    }

    async fn apply_entry_to_state_machine(
        &self,
        index: &u64,
        data: &ClientRequest,
    ) -> Result<ClientResponse> {
        self.next_serial.store(data.serial, Ordering::Release);
        let mut txn = self.env.write_txn()?;
        let last_applied_log = *index;
        let response = self.apply_message(data.message.clone())?;
        self.set_last_applied_log(&mut txn, last_applied_log)?;
        txn.commit()?;
        Ok(response)
    }

    async fn replicate_to_state_machine(&self, entries: &[(&u64, &ClientRequest)]) -> Result<()> {
        let mut txn = self.env.write_txn()?;
        let mut last_applied_log = self.last_applied_log(&txn)?.unwrap_or_default();
        for (index, request) in entries {
            last_applied_log = **index;
            self.apply_message(request.message.clone())?;
        }
        self.set_last_applied_log(&mut txn, last_applied_log)?;
        txn.commit()?;
        Ok(())
    }

    async fn do_log_compaction(&self, through: u64) -> Result<CurrentSnapshotData<Self::Snapshot>> {
        // it is necessary to do all the heed transation in a standalone function because heed
        // transations are not thread safe.
        info!("compacting log");
        let snapshot = self.create_snapshot_and_compact(through)?;
        let snapshot_file = File::open(&snapshot.path).await?;

        Ok(CurrentSnapshotData {
            term: snapshot.term,
            index: snapshot.index,
            membership: snapshot.membership.clone(),
            snapshot: Box::new(snapshot_file),
        })
    }

    async fn create_snapshot(&self) -> Result<(String, Box<Self::Snapshot>)> {
        let id = self.generate_snapshot_id();
        let path = self.snapshot_path_from_id(&id);
        let file = File::open(path).await?;
        Ok((id, Box::new(file)))
    }

    async fn finalize_snapshot_installation(
        &self,
        index: u64,
        term: u64,
        delete_through: Option<u64>,
        id: String,
        _snapshot: Box<Self::Snapshot>,
    ) -> Result<()> {
        info!("Restoring snapshot.");
        let mut txn = self.env.write_txn()?;
        match delete_through {
            Some(index) => {
                self.logs.delete_range(&mut txn, &(0..index))?;
            }
            None => self.logs.clear(&mut txn)?,
        }
        let membership_config = self
            .membership_config(&txn)?
            .unwrap_or_else(|| MembershipConfig::new_initial(self.id));
        let entry = Entry::new_snapshot_pointer(index, term, id.clone(), membership_config.clone());
        self.put_log(&mut txn, index, &entry)?;

        let raft_snapshot = RaftSnapshot {
            index,
            term,
            membership: membership_config,
            path: self.snapshot_path_from_id(&id),
            id: id.clone(),
        };

        self.set_current_snapshot(&mut txn, &raft_snapshot)?;

        let new_db_path = PathBuf::from(format!("{}_new", self.store.db_path));
        info!("unpacking snapshot in {:#?}...", new_db_path);
        crate::helpers::compression::from_tar_gz(&self.snapshot_path_from_id(&id), &new_db_path)?;
        info!("unpacking done.");
        let db_opt = DatabaseOptions {
            main_map_size: self.store.opt.max_mdb_size,
            update_map_size: self.store.opt.max_udb_size,
        };
        let new_db = Db::open_or_create(new_db_path, db_opt)?;
        let old_db = self.store.db.swap(Arc::new(new_db));

        txn.commit()?;

        std::thread::spawn(|| {
            match Arc::try_unwrap(old_db) {
                Ok(db) => {
                    // Get a write txn and do nothing with it
                    let _ = db.env.write_txn().map(|txn| { let _ = txn.commit(); });
                    while db.env.reader_list().len() > 1 {
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    // The call to close is safe, because we took ownership on the env, and waited
                    // for all txn to terminate, no subsequent operation can occur on the
                    // environement.
                    unsafe { db.env.close() };
                    info!("closed environement");
                }
                Err(_) => {
                    // there shouldn't be other refs at this point
                    panic!("can't get db ownership");
                }
            }
        });

        Ok(())
    }

    async fn get_current_snapshot(
        &self,
    ) -> Result<Option<async_raft::storage::CurrentSnapshotData<Self::Snapshot>>> {
        let current_snapshot = self.current_snapshot_txn()?;
        match current_snapshot {
            Some(RaftSnapshot {
                path,
                index,
                membership,
                term,
                ..
            }) => {
                let file = File::open(path).await?;
                let snapshot_data = CurrentSnapshotData {
                    index,
                    term,
                    membership,
                    snapshot: Box::new(file),
                };
                Ok(Some(snapshot_data))
            }
            None => Ok(None),
        }
    }
}
