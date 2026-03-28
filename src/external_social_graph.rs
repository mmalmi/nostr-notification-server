use heed::byteorder::BigEndian;
use heed::types::{SerdeBincode, Str, U32};
use heed::{Database, Env, EnvFlags, EnvOpenOptions, RoTxn};
use std::error::Error;
use std::path::Path;

const DEFAULT_MAP_SIZE: usize = 4 * 1024 * 1024 * 1024;
const MAX_DBS: u32 = 16;

const STR_TO_UNIQUE_ID_DB: &str = "str_to_unique_id";
const FOLLOW_DISTANCE_BY_USER_DB: &str = "follow_distance_by_user";
const MUTED_BY_USER_DB: &str = "muted_by_user";

pub struct ExternalSocialGraph {
    env: Env,
    str_to_unique_id: Database<Str, U32<BigEndian>>,
    follow_distance_by_user: Database<U32<BigEndian>, U32<BigEndian>>,
    muted_by_user: Database<U32<BigEndian>, SerdeBincode<Vec<u32>>>,
}

impl ExternalSocialGraph {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(format!("social graph path does not exist: {}", path.display()).into());
        }

        let mut options = EnvOpenOptions::new();
        options.map_size(DEFAULT_MAP_SIZE).max_dbs(MAX_DBS);
        unsafe {
            options.flags(EnvFlags::READ_ONLY);
        }

        let env = unsafe { options.open(path)? };
        Self::from_env(env)
    }

    fn from_env(env: Env) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let rtxn = env.read_txn()?;
        let str_to_unique_id = open_required_database::<Str, U32<BigEndian>>(
            &env,
            &rtxn,
            STR_TO_UNIQUE_ID_DB,
        )?;
        let follow_distance_by_user = open_required_database::<U32<BigEndian>, U32<BigEndian>>(
            &env,
            &rtxn,
            FOLLOW_DISTANCE_BY_USER_DB,
        )?;
        let muted_by_user = open_required_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
            &env,
            &rtxn,
            MUTED_BY_USER_DB,
        )?;
        drop(rtxn);

        Ok(Self {
            env,
            str_to_unique_id,
            follow_distance_by_user,
            muted_by_user,
        })
    }

    pub fn is_pubkey_in_graph(&self, pubkey: &str) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let Some(pubkey_id) = self.str_to_unique_id.get(&rtxn, pubkey)? else {
            return Ok(false);
        };

        Ok(self
            .follow_distance_by_user
            .get(&rtxn, &pubkey_id)?
            .is_some_and(|distance| distance < 1000))
    }

    pub fn recipient_has_muted_author(
        &self,
        recipient: &str,
        author: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let Some(recipient_id) = self.str_to_unique_id.get(&rtxn, recipient)? else {
            return Ok(false);
        };
        let Some(author_id) = self.str_to_unique_id.get(&rtxn, author)? else {
            return Ok(false);
        };

        Ok(self
            .muted_by_user
            .get(&rtxn, &recipient_id)?
            .is_some_and(|muted| muted.contains(&author_id)))
    }
}

fn open_required_database<KC, DC>(
    env: &Env,
    rtxn: &RoTxn,
    name: &'static str,
) -> Result<Database<KC, DC>, Box<dyn Error + Send + Sync>>
where
    KC: heed::BytesEncode<'static> + heed::BytesDecode<'static> + 'static,
    DC: heed::BytesEncode<'static> + heed::BytesDecode<'static> + 'static,
{
    env.open_database(rtxn, Some(name))?
        .ok_or_else(|| format!("required database missing from external social graph: {name}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn reads_follow_distances_and_mute_lists_from_snapshot() {
        let path = std::env::temp_dir().join(format!(
            "nns-external-social-graph-test-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();

        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(DEFAULT_MAP_SIZE)
                .max_dbs(MAX_DBS)
                .open(&path)
                .unwrap()
        };

        {
            let mut wtxn = env.write_txn().unwrap();
            let str_to_unique_id = env
                .create_database::<Str, U32<BigEndian>>(&mut wtxn, Some(STR_TO_UNIQUE_ID_DB))
                .unwrap();
            let follow_distance_by_user = env
                .create_database::<U32<BigEndian>, U32<BigEndian>>(
                    &mut wtxn,
                    Some(FOLLOW_DISTANCE_BY_USER_DB),
                )
                .unwrap();
            let muted_by_user = env
                .create_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
                    &mut wtxn,
                    Some(MUTED_BY_USER_DB),
                )
                .unwrap();

            str_to_unique_id.put(&mut wtxn, "recipient", &1).unwrap();
            str_to_unique_id.put(&mut wtxn, "allowed", &2).unwrap();
            str_to_unique_id.put(&mut wtxn, "muted", &3).unwrap();
            str_to_unique_id.put(&mut wtxn, "distant", &4).unwrap();

            follow_distance_by_user.put(&mut wtxn, &2, &1).unwrap();
            follow_distance_by_user.put(&mut wtxn, &3, &2).unwrap();
            follow_distance_by_user.put(&mut wtxn, &4, &1000).unwrap();

            muted_by_user.put(&mut wtxn, &1, &vec![3]).unwrap();
            wtxn.commit().unwrap();
        }

        let graph = ExternalSocialGraph::from_env(env).unwrap();

        assert!(graph.is_pubkey_in_graph("allowed").unwrap());
        assert!(graph.is_pubkey_in_graph("muted").unwrap());
        assert!(!graph.is_pubkey_in_graph("distant").unwrap());
        assert!(!graph.is_pubkey_in_graph("missing").unwrap());

        assert!(graph
            .recipient_has_muted_author("recipient", "muted")
            .unwrap());
        assert!(!graph
            .recipient_has_muted_author("recipient", "allowed")
            .unwrap());
        assert!(!graph
            .recipient_has_muted_author("missing", "muted")
            .unwrap());

        drop(graph);
        fs::remove_dir_all(path).unwrap();
    }
}
