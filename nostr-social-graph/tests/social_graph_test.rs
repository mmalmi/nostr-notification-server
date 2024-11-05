use nostr_social_graph::SocialGraph;
use nostr::{Event, Kind, Tag, PublicKey, UnsignedEvent, Keys, SecretKey};
use std::str::FromStr;

const ADAM: &str = "020f2d21ae09bf35fcdfb65decf1478b846f5f728ab30c5eaabcd6d081a81c3e";
const FIATJAF: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
const SNOWDEN: &str = "84dee6e676e5bb67b4ad4e042cf70cbd8681155db535942fcc6a0533858a7240";
const SIRIUS: &str = "4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0";

fn create_follow_event_with_timestamp(pubkey: &str, followed_pubkey: &str, created_at: u64) -> Event {
    let secret_key = SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001").unwrap();
    let public_key = PublicKey::from_str(pubkey).unwrap();
    let followed_key = PublicKey::from_str(followed_pubkey).unwrap();
    
    let unsigned_event = UnsignedEvent::new(
        public_key,
        created_at.into(),
        Kind::ContactList,
        vec![Tag::parse(&["p", &followed_key.to_string()]).unwrap()],
        String::new(),
    );
    
    let keys = Keys::new(secret_key);
    unsigned_event.sign(&keys).unwrap()
}

fn create_follow_event(pubkey: &str, followed_pubkey: &str) -> Event {
    create_follow_event_with_timestamp(pubkey, followed_pubkey, 1234567890)
}

fn create_test_env(path: &str) -> heed::Env {
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).expect("Failed to create test directory");
    
    unsafe {
        heed::EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB
            .max_dbs(3000)
            .open(path)
            .expect("Failed to create env")
    }
}

#[test]
fn test_initialize_with_root_user() {
    let env = create_test_env("test_db_init");
    let graph = SocialGraph::new(ADAM, env, None).expect("Failed to create graph");
    assert_eq!(graph.get_follow_distance(ADAM).unwrap(), 0);
    let _ = std::fs::remove_dir_all("test_db_init");
}

#[test]
fn test_handle_follow_events() {
    let env = create_test_env("test_db_follow");
    let graph = SocialGraph::new(ADAM, env, None).expect("Failed to create graph");
    let event = create_follow_event(ADAM, FIATJAF);
    
    graph.handle_event(&event).expect("Failed to handle event");
    assert!(graph.is_following(ADAM, FIATJAF).unwrap());
    let _ = std::fs::remove_dir_all("test_db_follow");
}

#[test]
fn test_update_follow_distances() {
    let env = create_test_env("test_db_distances");
    let graph = SocialGraph::new(ADAM, env, None).expect("Failed to create graph");
    
    let event1 = create_follow_event(ADAM, FIATJAF);
    let event2 = create_follow_event(FIATJAF, SNOWDEN);
    
    graph.handle_event(&event1).expect("Failed to handle event");
    graph.handle_event(&event2).expect("Failed to handle event");
    
    assert_eq!(graph.get_follow_distance(SNOWDEN).unwrap(), 2);
    let _ = std::fs::remove_dir_all("test_db_distances");
}

#[test]
fn test_update_follow_distances_when_root_changed() {
    let env = create_test_env("test_db_root_change");
    let mut graph = SocialGraph::new(ADAM, env, None).expect("Failed to create graph");
    
    let event1 = create_follow_event(ADAM, FIATJAF);
    let event2 = create_follow_event(FIATJAF, SNOWDEN);
    
    graph.handle_event(&event1).expect("Failed to handle event");
    graph.handle_event(&event2).expect("Failed to handle event");
    
    assert_eq!(graph.get_follow_distance(ADAM).unwrap(), 0);
    assert_eq!(graph.get_follow_distance(FIATJAF).unwrap(), 1);
    assert_eq!(graph.get_follow_distance(SNOWDEN).unwrap(), 2);
    
    graph.set_root(SNOWDEN).expect("Failed to set root");
    assert_eq!(graph.get_follow_distance(SNOWDEN).unwrap(), 0);
    assert_eq!(graph.get_follow_distance(FIATJAF).unwrap(), 1000);
    assert_eq!(graph.get_follow_distance(ADAM).unwrap(), 1000);
    
    graph.set_root(FIATJAF).expect("Failed to set root");
    assert_eq!(graph.get_follow_distance(SNOWDEN).unwrap(), 1);
    assert_eq!(graph.get_follow_distance(FIATJAF).unwrap(), 0);
    assert_eq!(graph.get_follow_distance(ADAM).unwrap(), 1000);
    
    let _ = std::fs::remove_dir_all("test_db_root_change");
}

#[test]
fn test_follower_counts() {
    let env = create_test_env("test_db_follower_counts");
    let graph = SocialGraph::new(ADAM, env, None).expect("Failed to create graph");
    
    let event1 = create_follow_event(ADAM, FIATJAF);
    let event2 = create_follow_event(SNOWDEN, FIATJAF);
    let event3 = create_follow_event(SIRIUS, FIATJAF);
    
    graph.handle_event(&event1).expect("Failed to handle event");
    graph.handle_event(&event2).expect("Failed to handle event");
    graph.handle_event(&event3).expect("Failed to handle event");
    
    assert_eq!(graph.follower_count(FIATJAF).unwrap(), 3);
    assert_eq!(graph.follower_count(ADAM).unwrap(), 0);
    assert_eq!(graph.followed_by_friends_count(FIATJAF).unwrap(), 0);
    let _ = std::fs::remove_dir_all("test_db_follower_counts");
}

#[test]
fn test_followed_by_friends() {
    let env = create_test_env("test_db_followed_by_friends");
    let graph = SocialGraph::new(ADAM, env, None).expect("Failed to create graph");
    
    let event1 = create_follow_event(ADAM, FIATJAF);
    let event2 = create_follow_event(FIATJAF, SNOWDEN);
    
    graph.handle_event(&event1).expect("Failed to handle event");
    graph.handle_event(&event2).expect("Failed to handle event");
    
    let friends = graph.followed_by_friends(SNOWDEN).unwrap();
    assert!(friends.contains(FIATJAF));
    assert_eq!(friends.len(), 1);
    let _ = std::fs::remove_dir_all("test_db_followed_by_friends");
}

#[test]
fn test_get_users_by_follow_distance() {
    let env = create_test_env("test_db_get_users_by_follow_distance");
    let graph = SocialGraph::new(ADAM, env, None).expect("Failed to create graph");
    
    let event1 = create_follow_event(ADAM, FIATJAF);
    let event2 = create_follow_event(FIATJAF, SNOWDEN);
    
    graph.handle_event(&event1).expect("Failed to handle event");
    graph.handle_event(&event2).expect("Failed to handle event");
    
    let distance_1_users = graph.get_users_by_follow_distance(1).unwrap();
    let distance_2_users = graph.get_users_by_follow_distance(2).unwrap();
    
    assert_eq!(distance_1_users.len(), 1);
    assert!(distance_1_users.contains(&FIATJAF.to_string()));
    
    assert_eq!(distance_2_users.len(), 1);
    assert!(distance_2_users.contains(&SNOWDEN.to_string()));
    
    let _ = std::fs::remove_dir_all("test_db_get_users_by_follow_distance");
}

#[test]
fn test_serialize_deserialize() {
    let env = create_test_env("test_db_serialize_deserialize");
    let graph = SocialGraph::new(ADAM, env.clone(), None).expect("Failed to create graph");
    
    let event1 = create_follow_event(ADAM, FIATJAF);
    let event2 = create_follow_event(FIATJAF, SNOWDEN);
    
    graph.handle_event(&event1).expect("Failed to handle event");
    graph.handle_event(&event2).expect("Failed to handle event");
    
    // Check initial state
    assert_eq!(graph.get_follow_distance(ADAM).unwrap(), 0);
    assert_eq!(graph.get_follow_distance(FIATJAF).unwrap(), 1);
    assert_eq!(graph.get_follow_distance(SNOWDEN).unwrap(), 2);
    assert!(graph.is_following(ADAM, FIATJAF).unwrap());
    assert!(graph.is_following(FIATJAF, SNOWDEN).unwrap());
    
    // Serialize and create new graph
    let serialized = graph.serialize().expect("Failed to serialize");
    let new_graph = SocialGraph::new(ADAM, env, Some(serialized)).expect("Failed to create new graph");
    
    // Check state is preserved
    assert_eq!(new_graph.get_follow_distance(ADAM).unwrap(), 0);
    assert_eq!(new_graph.get_follow_distance(FIATJAF).unwrap(), 1);
    assert_eq!(new_graph.get_follow_distance(SNOWDEN).unwrap(), 2);
    assert!(new_graph.is_following(ADAM, FIATJAF).unwrap());
    assert!(new_graph.is_following(FIATJAF, SNOWDEN).unwrap());
    let _ = std::fs::remove_dir_all("test_db_serialize_deserialize");
}

#[test]
fn test_utilize_existing_follow_lists() {
    // Create test directories
    let db_path1 = "test_db_1";
    let db_path2 = "test_db_2";
    let _ = std::fs::remove_dir_all(db_path1); // Clean up any existing directory
    let _ = std::fs::remove_dir_all(db_path2);
    std::fs::create_dir_all(db_path1).expect("Failed to create test directory 1");
    std::fs::create_dir_all(db_path2).expect("Failed to create test directory 2");

    let env = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(1024 * 1024 * 1024) // 10MB
            .max_dbs(3000)
            .open(db_path1)
            .expect("Failed to create env")
    };

    let graph = SocialGraph::new(ADAM, env.clone(), None).expect("Failed to create initial graph");
    
    let event1 = create_follow_event(ADAM, FIATJAF);
    let event2 = create_follow_event(FIATJAF, SNOWDEN);
    
    graph.handle_event(&event1).expect("Failed to handle event");
    graph.handle_event(&event2).expect("Failed to handle event");
    
    // Check initial state
    assert_eq!(graph.get_follow_distance(ADAM).unwrap(), 0);
    assert_eq!(graph.get_follow_distance(FIATJAF).unwrap(), 1);
    assert_eq!(graph.get_follow_distance(SNOWDEN).unwrap(), 2);
    assert!(graph.is_following(ADAM, FIATJAF).unwrap());
    assert!(graph.is_following(FIATJAF, SNOWDEN).unwrap());
    
    // Serialize and create new graph with different root
    let serialized = graph.serialize().expect("Failed to serialize");
    
    // Create new environment for the new graph
    let env2 = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(1024 * 1024 * 1024)
            .max_dbs(3000)
            .open(db_path2)
            .expect("Failed to create env")
    };
    
    let new_graph = SocialGraph::new(SIRIUS, env2, Some(serialized)).expect("Failed to create new graph");
    
    // Check initial state of new graph
    assert!(new_graph.is_following(ADAM, FIATJAF).unwrap());
    assert!(new_graph.is_following(FIATJAF, SNOWDEN).unwrap());
    assert_eq!(new_graph.get_follow_distance(SIRIUS).unwrap(), 0);
    assert_eq!(new_graph.get_follow_distance(ADAM).unwrap(), 1000);
    assert_eq!(new_graph.get_follow_distance(FIATJAF).unwrap(), 1000);
    assert_eq!(new_graph.get_follow_distance(SNOWDEN).unwrap(), 1000);
    
    // Add new follow event
    let event3 = create_follow_event(SIRIUS, ADAM);
    new_graph.handle_event(&event3).expect("Failed to handle event");
    
    new_graph.recalculate_follow_distances().expect("Failed to recalculate distances");
    
    // Check updated state
    assert!(new_graph.is_following(SIRIUS, ADAM).unwrap());
    assert!(new_graph.is_following(ADAM, FIATJAF).unwrap());
    assert!(new_graph.is_following(FIATJAF, SNOWDEN).unwrap());
    assert_eq!(new_graph.get_follow_distance(SIRIUS).unwrap(), 0);
    assert_eq!(new_graph.get_follow_distance(ADAM).unwrap(), 1);
    assert_eq!(new_graph.get_follow_distance(FIATJAF).unwrap(), 2);
    assert_eq!(new_graph.get_follow_distance(SNOWDEN).unwrap(), 3);

    // Clean up test directories
    let _ = std::fs::remove_dir_all(db_path1);
    let _ = std::fs::remove_dir_all(db_path2);
}
