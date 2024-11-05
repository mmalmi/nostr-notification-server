use heed::types::*;
use heed::{Database, Env};
use heed::byteorder::BigEndian;
use std::sync::Mutex;
use std::path::Path;

pub type UID = u64;
pub type SerializedUniqueIds = Vec<(String, UID)>;

pub struct UniqueIds {
    env: Env,
    str_to_unique_id: Database<Str, U64<BigEndian>>,
    unique_id_to_str: Database<U64<BigEndian>, Str>,
    current_unique_id: Mutex<UID>,
}

impl UniqueIds {
    pub fn new(path: impl AsRef<Path>, serialized: Option<SerializedUniqueIds>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut env_builder = heed::EnvOpenOptions::new();
        env_builder
            .map_size(1024 * 1024 * 1024) // 1GB default size
            .max_dbs(2);
        let env = unsafe { env_builder.open(path.as_ref())? };
        
        let (str_to_unique_id, unique_id_to_str) = {
            let mut wtxn = env.write_txn()?;
            let str_to_id = env.create_database(&mut wtxn, Some("str_to_id"))?;
            let id_to_str = env.create_database(&mut wtxn, Some("id_to_str"))?;
            wtxn.commit()?;
            (str_to_id, id_to_str)
        };

        let instance = Self {
            env,
            str_to_unique_id,
            unique_id_to_str,
            current_unique_id: Mutex::new(0),
        };

        if let Some(data) = serialized {
            let mut wtxn = instance.env.write_txn()?;
            let mut max_id = 0;
            for (s, id) in data {
                instance.str_to_unique_id.put(&mut wtxn, &s, &id)?;
                instance.unique_id_to_str.put(&mut wtxn, &id, &s)?;
                max_id = max_id.max(id + 1);
            }
            wtxn.commit()?;
            *instance.current_unique_id.lock().unwrap() = max_id;
        }

        Ok(instance)
    }

    pub fn id(&self, s: &str) -> Option<UID> {
        let rtxn = self.env.read_txn().unwrap();
        self.str_to_unique_id.get(&rtxn, s).unwrap()
    }

    pub fn get_or_create_id(&self, s: &str) -> UID {
        if let Some(id) = self.id(s) {
            return id;
        }

        let new_id = {
            let mut current_id = self.current_unique_id.lock().unwrap();
            let id = *current_id;
            *current_id += 1;
            id
        };

        let mut wtxn = self.env.write_txn().unwrap();
        self.str_to_unique_id.put(&mut wtxn, s, &new_id).unwrap();
        self.unique_id_to_str.put(&mut wtxn, &new_id, s).unwrap();
        wtxn.commit().unwrap();

        new_id
    }

    pub fn str(&self, id: UID) -> Result<String, Box<dyn std::error::Error>> {
        let rtxn = self.env.read_txn()?;
        self.unique_id_to_str.get(&rtxn, &id)?
            .map(|s| s.to_string())
            .ok_or_else(|| format!("invalid id {}", id).into())
    }

    pub fn has(&self, s: &str) -> bool {
        let rtxn = self.env.read_txn().unwrap();
        self.str_to_unique_id.get(&rtxn, s).unwrap().is_some()
    }

    pub fn serialize(&self) -> Result<SerializedUniqueIds, Box<dyn std::error::Error>> {
        let rtxn = self.env.read_txn()?;
        let mut result = Vec::new();
        
        let mut iter = self.str_to_unique_id.iter(&rtxn)?;
        while let Some(Ok((s, id))) = iter.next() {
            result.push((s.to_string(), id));
        }
        
        Ok(result)
    }
}