use std::collections::HashSet;
use crate::unique_ids::{UniqueIds, UID};
use heed::types::*;
use heed::{Database, Env};
use heed::byteorder::BigEndian;
use nostr::{Event, Kind};
use crate::error::SocialGraphError;

pub type SerializedUserList = (UID, Vec<UID>, Option<u64>);

#[derive(Debug)]
pub struct SerializedSocialGraph {
    pub unique_ids: Vec<(String, UID)>,
    pub follow_lists: Vec<SerializedUserList>,
    pub mute_lists: Option<Vec<SerializedUserList>>,
}

pub struct SocialGraph {
    root: UID,
    env: Env,
    follow_distance_by_user: Database<SerdeBincode<UID>, U32<BigEndian>>,
    followed_by_user: Database<Str, SerdeBincode<HashSet<UID>>>,
    followers_by_user: Database<Str, SerdeBincode<HashSet<UID>>>,
    follow_list_created_at: Database<SerdeBincode<UID>, U64<BigEndian>>,
    muted_by_user: Database<Str, SerdeBincode<HashSet<UID>>>,
    mute_list_created_at: Database<SerdeBincode<UID>, U64<BigEndian>>,
    users_by_follow_distance: Database<U32<BigEndian>, SerdeBincode<HashSet<UID>>>,
    ids: UniqueIds,
}

impl SocialGraph {
    pub fn new(root: &str, env: Env, serialized: Option<SerializedSocialGraph>) -> Result<Self, SocialGraphError> {
        // Create unique_ids subdirectory
        let unique_ids_path = env.path().join("unique_ids");
        std::fs::create_dir_all(&unique_ids_path)?;
        
        let ids = UniqueIds::new(&unique_ids_path, serialized.as_ref().map(|s| s.unique_ids.clone()))
            .map_err(|e| SocialGraphError::Database(e.to_string()))?;
        let root_id = ids.get_or_create_id(root);
        
        let (
            follow_distance_by_user,
            followed_by_user,
            followers_by_user,
            follow_list_created_at,
            muted_by_user,
            mute_list_created_at,
            users_by_follow_distance,
        ) = {
            let mut wtxn = env.write_txn()?;
            let dbs = (
                env.create_database(&mut wtxn, Some("follow_distance_by_user"))?,
                env.create_database(&mut wtxn, Some("followed_by_user"))?,
                env.create_database(&mut wtxn, Some("followers_by_user"))?,
                env.create_database(&mut wtxn, Some("follow_list_created_at"))?,
                env.create_database(&mut wtxn, Some("muted_by_user"))?,
                env.create_database(&mut wtxn, Some("mute_list_created_at"))?,
                env.create_database(&mut wtxn, Some("users_by_follow_distance"))?,
            );
            wtxn.commit()?;
            dbs
        };
        
        let graph = Self {
            root: root_id,
            env,
            follow_distance_by_user,
            followed_by_user,
            followers_by_user,
            follow_list_created_at,
            muted_by_user,
            mute_list_created_at,
            users_by_follow_distance,
            ids,
        };

        let mut wtxn = graph.env.write_txn()?;
        graph.follow_distance_by_user.put(&mut wtxn, &root_id, &0)?;
        wtxn.commit()?;

        if let Some(s) = serialized {
            graph.deserialize(&s.follow_lists, s.mute_lists.as_deref())?;
        }

        Ok(graph)
    }

    pub fn get_root(&self) -> Result<String, SocialGraphError> {
        Ok(self.ids.str(self.root)?.clone())
    }

    pub fn set_root(&mut self, root: &str) -> Result<(), SocialGraphError> {
        let root_id = self.ids.get_or_create_id(root);
        self.root = root_id;
        self.recalculate_follow_distances()?;
        Ok(())
    }

    pub fn recalculate_follow_distances(&self) -> Result<(), SocialGraphError> {
        let mut wtxn = self.env.write_txn()?;
        
        // Clear existing distances
        self.follow_distance_by_user.clear(&mut wtxn)?;
        self.users_by_follow_distance.clear(&mut wtxn)?;
        
        // Set root distance to 0
        self.follow_distance_by_user.put(&mut wtxn, &self.root, &0)?;
        let mut root_users = HashSet::new();
        root_users.insert(self.root);
        self.users_by_follow_distance.put(&mut wtxn, &0, &root_users)?;
        
        let mut queue = vec![self.root];
        
        while let Some(user) = queue.pop() {
            let distance = self.follow_distance_by_user.get(&wtxn, &user)?.unwrap_or(0);

            if let Some(followed_users) = self.followed_by_user.get(&wtxn, &user.to_string())? {
                for &followed in &followed_users {
                    if self.follow_distance_by_user.get(&wtxn, &followed)?.is_none() {
                        let new_distance = distance + 1;
                        self.follow_distance_by_user.put(&mut wtxn, &followed, &new_distance)?;
                        
                        // Update users_by_follow_distance
                        let mut users = self.users_by_follow_distance
                            .get(&wtxn, &new_distance)?
                            .unwrap_or_default();
                        users.insert(followed);
                        self.users_by_follow_distance.put(&mut wtxn, &new_distance, &users)?;
                        
                        queue.push(followed);
                    }
                }
            }
        }

        wtxn.commit()?;
        Ok(())
    }

    pub fn handle_event(&self, event: &Event) -> Result<(), SocialGraphError> {
        if event.kind != Kind::ContactList {
            return Ok(());
        }
        
        let author = self.ids.get_or_create_id(&event.pubkey.to_hex());
        let created_at = event.created_at.as_u64();
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
                
        if created_at > (current_time + 10 * 60) {
            return Ok(());
        }

        // Check timestamp with read transaction
        {
            let rtxn = self.env.read_txn()?;
            if let Some(existing_created_at) = self.follow_list_created_at.get(&rtxn, &author)? {
                if created_at <= existing_created_at {
                    return Ok(());
                }
            }
        }

        let mut followed_in_event = HashSet::new();
        for tag in &event.tags {
            if let Some(letter_tag) = tag.single_letter_tag() {
                if letter_tag.as_char() == 'p' {
                    if let Some(pubkey) = tag.content() {
                        if pubkey.len() == 64 && pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
                            let followed_user = self.ids.get_or_create_id(pubkey);
                            if followed_user != author {
                                followed_in_event.insert(followed_user);
                            }
                        }
                    }
                }
            }
        }

        // Get current follow list for comparison
        let current_follows = {
            let rtxn = self.env.read_txn()?;
            self.followed_by_user
                .get(&rtxn, &author.to_string())?
                .unwrap_or_default()
        };

        // Skip update if nothing changed
        if current_follows == followed_in_event {
            return Ok(());
        }

        // Single write transaction for all updates
        let needs_recalculation = {
            let mut wtxn = self.env.write_txn()?;
            let mut recalc_needed = false;
            
            self.follow_list_created_at.put(&mut wtxn, &author, &created_at)?;
            
            // Handle unfollows first
            for unfollowed in current_follows.difference(&followed_in_event) {
                let mut followers = self.followers_by_user
                    .get(&wtxn, &unfollowed.to_string())?
                    .unwrap_or_default();
                followers.remove(&author);
                
                if followers.is_empty() {
                    self.followers_by_user.delete(&mut wtxn, &unfollowed.to_string())?;
                } else {
                    self.followers_by_user.put(&mut wtxn, &unfollowed.to_string(), &followers)?;
                }
                recalc_needed = true;
            }

            // Handle new follows
            for &followed in followed_in_event.difference(&current_follows) {
                let mut followers = self.followers_by_user
                    .get(&wtxn, &followed.to_string())?
                    .unwrap_or_default();
                followers.insert(author);
                self.followers_by_user.put(&mut wtxn, &followed.to_string(), &followers)?;
                
                if followed != self.root {
                    if author == self.root {
                        self.follow_distance_by_user.put(&mut wtxn, &followed, &1)?;
                        Self::update_users_by_distance_internal(&self.users_by_follow_distance, followed, 1, &mut wtxn)?;
                    } else if let Some(follower_distance) = self.follow_distance_by_user.get(&wtxn, &author)? {
                        let new_distance = follower_distance + 1;
                        let current_distance = self.follow_distance_by_user.get(&wtxn, &followed)?;
                        
                        if current_distance.is_none() || new_distance < current_distance.unwrap() {
                            self.follow_distance_by_user.put(&mut wtxn, &followed, &new_distance)?;
                            Self::update_users_by_distance_internal(&self.users_by_follow_distance, followed, new_distance, &mut wtxn)?;
                        }
                    }
                }
                recalc_needed = true;
            }

            // Update followed_by_user in one operation
            if !followed_in_event.is_empty() {
                self.followed_by_user.put(&mut wtxn, &author.to_string(), &followed_in_event)?;
            } else {
                self.followed_by_user.delete(&mut wtxn, &author.to_string())?;
            }

            wtxn.commit()?;
            recalc_needed
        };

        // Only recalculate if needed
        if needs_recalculation {
            self.recalculate_follow_distances()?;
        }

        Ok(())
    }

    pub fn is_following(&self, follower: &str, followed_user: &str) -> Result<bool, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        let followed_user_id = match self.ids.id(followed_user) {
            Some(id) => id,
            None => return Ok(false),
        };
        let follower_id = match self.ids.id(follower) {
            Some(id) => id,
            None => return Ok(false),
        };
        
        let is_following = self.followed_by_user
            .get(&rtxn, &follower_id.to_string())?
            .map(|set| set.contains(&followed_user_id))
            .unwrap_or(false);
            
        Ok(is_following)
    }

    pub fn get_follow_distance(&self, user: &str) -> Result<u32, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        let user_id = match self.ids.id(user) {
            Some(id) => id,
            None => return Ok(1000),
        };
        
        Ok(self.follow_distance_by_user
            .get(&rtxn, &user_id)?
            .unwrap_or(1000))
    }

    pub fn add_follower(&self, follower: &str, followed_user: &str) -> Result<(), SocialGraphError> {
        let follower_id = self.ids.get_or_create_id(follower);
        let followed_user_id = self.ids.get_or_create_id(followed_user);
        
        // Check if already following
        {
            let rtxn = self.env.read_txn()?;
            if let Some(followed) = self.followed_by_user.get(&rtxn, &follower_id.to_string())? {
                if followed.contains(&followed_user_id) {
                    return Ok(());
                }
            }
        }

        let mut wtxn = self.env.write_txn()?;
        
        // Update followers
        let mut followers = self.followers_by_user
            .get(&wtxn, &followed_user_id.to_string())?
            .unwrap_or_default();
        followers.insert(follower_id);
        self.followers_by_user.put(&mut wtxn, &followed_user_id.to_string(), &followers)?;

        // Update followed
        let mut followed = self.followed_by_user
            .get(&wtxn, &follower_id.to_string())?
            .unwrap_or_default();
        followed.insert(followed_user_id);
        self.followed_by_user.put(&mut wtxn, &follower_id.to_string(), &followed)?;

        wtxn.commit()?;
        self.recalculate_follow_distances()?;
        Ok(())
    }

    pub fn remove_follower(&self, follower: &str, followed_user: &str) -> Result<(), SocialGraphError> {
        let follower_id = match self.ids.id(follower) {
            Some(id) => id,
            None => return Ok(()),
        };
        let followed_user_id = match self.ids.id(followed_user) {
            Some(id) => id,
            None => return Ok(()),
        };
        self.private_remove_follower(followed_user_id, follower_id)
    }

    pub fn follower_count(&self, address: &str) -> Result<usize, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        // Read-only operation, return 0 if user doesn't exist
        let id = match self.ids.id(address) {
            Some(id) => id.to_string(),
            None => return Ok(0),
        };
        
        Ok(self.followers_by_user
            .get(&rtxn, &id)?
            .map_or(0, |s| s.len()))
    }

    pub fn followed_by_friends_count(&self, address: &str) -> Result<usize, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        // Read-only operation, return 0 if user doesn't exist
        let id = match self.ids.id(address) {
            Some(id) => id.to_string(),
            None => return Ok(0),
        };
        
        let count = if let Some(followers) = self.followers_by_user.get(&rtxn, &id)? {
            if let Some(root_follows) = self.followed_by_user.get(&rtxn, &self.root.to_string())? {
                followers.iter()
                    .filter(|&follower| root_follows.contains(follower))
                    .count()
            } else {
                0
            }
        } else {
            0
        };
        
        Ok(count)
    }

    pub fn size(&self) -> Result<(usize, usize), SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        
        let mut total_follows = 0;
        let mut iter = self.followed_by_user.iter(&rtxn)?;
        while let Some(Ok((_, followed_set))) = iter.next() {
            total_follows += followed_set.len();
        }

        let mut total_users = 0;
        let mut iter = self.follow_distance_by_user.iter(&rtxn)?;
        while let Some(Ok(_)) = iter.next() {
            total_users += 1;
        }

        Ok((total_users, total_follows))
    }

    pub fn followed_by_friends(&self, address: &str) -> Result<HashSet<String>, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        // Read-only operation, return empty set if user doesn't exist
        let id = match self.ids.id(address) {
            Some(id) => id.to_string(),
            None => return Ok(HashSet::new()),
        };
        
        let mut set = HashSet::new();
        if let Some(followers) = self.followers_by_user.get(&rtxn, &id)? {
            if let Some(root_follows) = self.followed_by_user.get(&rtxn, &self.root.to_string())? {
                for &follower in &followers {
                    if root_follows.contains(&follower) {
                        if let Ok(str_id) = self.ids.str(follower) {
                            set.insert(str_id.clone());
                        }
                    }
                }
            }
        }
        
        Ok(set)
    }

    pub fn get_followed_by_user(&self, user: &str, include_self: bool) -> Result<HashSet<String>, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        // Read-only operation, return empty set if user doesn't exist
        let user_id = match self.ids.id(user) {
            Some(id) => id.to_string(),
            None => return Ok(HashSet::new()),
        };
        
        let mut set = HashSet::new();
        if let Some(followed) = self.followed_by_user.get(&rtxn, &user_id)? {
            for id in followed {
                if let Ok(str_id) = self.ids.str(id) {
                    set.insert(str_id.clone());
                }
            }
        }
        
        if include_self {
            set.insert(user.to_string());
        }
        Ok(set)
    }

    pub fn get_followers_by_user(&self, address: &str) -> Result<HashSet<String>, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        // Read-only operation, return empty set if user doesn't exist
        let user_id = match self.ids.id(address) {
            Some(id) => id.to_string(),
            None => return Ok(HashSet::new()),
        };
        
        let mut set = HashSet::new();
        if let Some(followers) = self.followers_by_user.get(&rtxn, &user_id)? {
            for id in followers {
                if let Ok(str_id) = self.ids.str(id) {
                    set.insert(str_id.clone());
                }
            }
        }
        Ok(set)
    }

    pub fn get_follow_list_created_at(&self, user: &str) -> Result<Option<u64>, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        // Read-only operation, return None if user doesn't exist
        let user_id = match self.ids.id(user) {
            Some(id) => id,
            None => return Ok(None),
        };
        Ok(self.follow_list_created_at.get(&rtxn, &user_id)?)
    }

    fn deserialize(&self, follow_lists: &[(UID, Vec<UID>, Option<u64>)], mute_lists: Option<&[(UID, Vec<UID>, Option<u64>)]>) -> Result<(), SocialGraphError> {
        let mut wtxn = self.env.write_txn()?;

        // Handle follow lists
        for (follower, followed_users, created_at) in follow_lists {
            if let Some(timestamp) = created_at {
                self.follow_list_created_at.put(&mut wtxn, follower, timestamp)?;
            }
            
            let mut followed_set = HashSet::new();
            followed_set.extend(followed_users);
            self.followed_by_user.put(&mut wtxn, &follower.to_string(), &followed_set)?;
            
            for &followed in followed_users {
                let mut followers = self.followers_by_user
                    .get(&wtxn, &followed.to_string())?
                    .unwrap_or_default();
                followers.insert(*follower);
                self.followers_by_user.put(&mut wtxn, &followed.to_string(), &followers)?;
            }
        }

        // Handle mute lists
        if let Some(mute_lists) = mute_lists {
            for &(user, ref muted_users, created_at) in mute_lists {
                if let Some(timestamp) = created_at {
                    self.mute_list_created_at.put(&mut wtxn, &user, &timestamp)?;
                }
                
                let mut muted_set = HashSet::new();
                muted_set.extend(muted_users);
                self.muted_by_user.put(&mut wtxn, &user.to_string(), &muted_set)?;
            }
        }

        wtxn.commit()?;
        self.recalculate_follow_distances()?;
        Ok(())
    }

    fn private_remove_follower(&self, followed_user: UID, follower: UID) -> Result<(), SocialGraphError> {
        let mut wtxn = self.env.write_txn()?;
        
        // Update followed_by_user
        if let Some(mut followed_set) = self.followed_by_user.get(&wtxn, &follower.to_string())? {
            followed_set.remove(&followed_user);
            self.followed_by_user.put(&mut wtxn, &follower.to_string(), &followed_set)?;
        }

        // Update followers_by_user
        if let Some(mut followers_set) = self.followers_by_user.get(&wtxn, &followed_user.to_string())? {
            followers_set.remove(&follower);
            self.followers_by_user.put(&mut wtxn, &followed_user.to_string(), &followers_set)?;
        }

        wtxn.commit()?;
        self.recalculate_follow_distances()?;
        Ok(())
    }

    pub fn serialize(&self) -> Result<SerializedSocialGraph, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        let mut follow_lists = Vec::new();
        
        let mut iter = self.followed_by_user.iter(&rtxn)?;
        while let Some(Ok((user_str, followed_set))) = iter.next() {
            let user = user_str.parse::<UID>()?;
            let followed_users: Vec<_> = followed_set.iter().copied().collect();
            let created_at = self.follow_list_created_at.get(&rtxn, &user)?;
            follow_lists.push((user, followed_users, created_at));
        }

        let mut mute_lists = Vec::new();
        let mut iter = self.muted_by_user.iter(&rtxn)?;
        while let Some(Ok((user_str, muted_set))) = iter.next() {
            let user = user_str.parse::<UID>()?;
            let muted_users: Vec<_> = muted_set.iter().copied().collect();
            let created_at = self.mute_list_created_at.get(&rtxn, &user)?;
            mute_lists.push((user, muted_users, created_at));
        }

        Ok(SerializedSocialGraph {
            unique_ids: self.ids.serialize()?,
            follow_lists,
            mute_lists: Some(mute_lists),
        })
    }

    fn update_users_by_distance_internal(
        users_by_follow_distance: &Database<U32<BigEndian>, SerdeBincode<HashSet<UID>>>,
        user: UID,
        new_distance: u32,
        wtxn: &mut heed::RwTxn,
    ) -> Result<(), SocialGraphError> {
        // Add to new distance group
        let mut users = users_by_follow_distance
            .get(wtxn, &new_distance)?
            .unwrap_or_default();
        users.insert(user);
        users_by_follow_distance.put(wtxn, &new_distance, &users)?;
        
        // Remove from all higher distances
        let mut distance = new_distance + 1;
        while let Some(mut users) = users_by_follow_distance.get(wtxn, &distance)? {
            if users.remove(&user) {
                if users.is_empty() {
                    users_by_follow_distance.delete(wtxn, &distance)?;
                } else {
                    users_by_follow_distance.put(wtxn, &distance, &users)?;
                }
            }
            distance += 1;
        }
        
        Ok(())
    }

    pub fn get_users_by_follow_distance(&self, distance: u32) -> Result<HashSet<String>, SocialGraphError> {
        let rtxn = self.env.read_txn()?;
        let mut result = HashSet::new();
        
        if let Some(users) = self.users_by_follow_distance.get(&rtxn, &distance)? {
            for user_id in users {
                if let Ok(str_id) = self.ids.str(user_id) {
                    result.insert(str_id.clone());
                }
            }
        }
        
        Ok(result)
    }
}

