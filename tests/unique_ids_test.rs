use nostr_social_graph::UniqueIds;

#[test]
fn unique_ids_survive_restarts_and_interleaved_writers() {
    let path = std::env::temp_dir().join(format!("notification-ids-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    {
        let ids = UniqueIds::new(&path, None).unwrap();
        assert_eq!(ids.get_or_create_id("root").unwrap(), 0);
        assert_eq!(ids.get_or_create_id("friend").unwrap(), 1);
    }
    {
        let first = UniqueIds::new(&path, None).unwrap();
        let second = UniqueIds::new(&path, None).unwrap();
        assert_eq!(first.get_or_create_id("newcomer").unwrap(), 2);
        assert_eq!(
            second
                .batch_insert(&["batch".into(), "root".into()])
                .unwrap(),
            vec![3, 0]
        );
        assert_eq!(first.get_or_create_id("last").unwrap(), 4);
        for (name, id) in first.serialize().unwrap() {
            assert_eq!(first.str(id).unwrap(), name);
            assert_eq!(second.id(&name), Some(id));
        }
    }
    std::fs::remove_dir_all(path).unwrap();
}
