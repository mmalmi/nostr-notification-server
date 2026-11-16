use async_trait::async_trait;
use hashtree_core::{to_hex, Cid, Hash, HashTree, HashTreeConfig, Store, StoreError};
use hashtree_resolver::nostr::{NostrResolverConfig, NostrRootResolver};
use hashtree_resolver::RootResolver;
use nostr_sdk::prelude::{FromBech32, PublicKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{collections::BTreeMap, env, error::Error, io, sync::Arc, thread};
use url::Url;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const MAX_GRAPH_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_BLOCK_BYTES: usize = 8 * 1024 * 1024;
const NAME_WINDOW: Duration = Duration::from_secs(5);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(120);
const GRAPH_FILE: &str = "social-graph.bin";

#[derive(Clone)]
pub(super) struct Generation {
    cid: Cid,
    completed_at: u64,
    generated_at: u64,
    scoring_root: String,
    graph: FileDigest,
}

#[derive(Deserialize)]
struct Manifest {
    version: u32,
    root: String,
    graph_completed_at: u64,
    graph_since: u64,
    generated_at: u64,
    files: BTreeMap<String, FileDigest>,
}

#[derive(Clone, Deserialize, PartialEq)]
struct FileDigest {
    size: u64,
    sha256: String,
}

pub(super) struct NamedSnapshot {
    name: String,
    directory: String,
    relays: Vec<String>,
    endpoint: Url,
}

impl NamedSnapshot {
    pub(super) fn from_env(source: &str) -> Result<Self> {
        Self::new(
            source,
            &env::var("NNS_HASHTREE_RELAYS")?,
            &env::var("NNS_HASHTREE_BLOSSOM_URL")?,
        )
    }

    fn new(source: &str, relays: &str, blossom: &str) -> Result<Self> {
        let path = source
            .strip_prefix("htree://")
            .ok_or("expected a Hashtree URI")?
            .trim_end_matches('/');
        let (publisher, path) = path.split_once('/').ok_or("missing published tree name")?;
        PublicKey::from_bech32(publisher)?;
        if path
            .split('/')
            .any(|part| matches!(part, "" | "." | "..") || part.contains(['?', '#', '%', '\\']))
        {
            return Err("invalid Hashtree graph path".into());
        }
        let (tree, directory) = path.split_once('/').unwrap_or((path, ""));
        let relays = relays
            .split(',')
            .map(str::trim)
            .map(|relay| {
                let url = Url::parse(relay)?;
                if !matches!(url.scheme(), "ws" | "wss") || !url.has_host() {
                    return Err("expected ws or wss Hashtree name relay".into());
                }
                Ok(relay.to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        let mut endpoint = Url::parse(blossom)?;
        if !matches!(endpoint.scheme(), "https" | "http")
            || !endpoint.has_host()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
        {
            return Err("expected HTTP(S) Blossom read endpoint".into());
        }
        if !endpoint.path().ends_with('/') {
            endpoint
                .path_segments_mut()
                .map_err(|_| "invalid Blossom path")?
                .push("");
        }
        Ok(Self {
            name: format!("{publisher}/{tree}"),
            directory: directory.to_owned(),
            relays,
            endpoint,
        })
    }

    pub(super) fn fetch(
        &self,
        previous: Option<&Generation>,
    ) -> Result<(Generation, Option<Vec<u8>>)> {
        // The same synchronous database API is used outside Tokio, on startup
        // inside Tokio, and on its blocking refresh worker.
        thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(async {
                            tokio::time::timeout(REFRESH_TIMEOUT, self.fetch_generation(previous))
                                .await?
                        })
                })
                .join()
                .map_err(|_| io::Error::other("Hashtree reader thread stopped"))?
        })
    }

    async fn fetch_generation(
        &self,
        previous: Option<&Generation>,
    ) -> Result<(Generation, Option<Vec<u8>>)> {
        let resolver = NostrRootResolver::new(NostrResolverConfig {
            relays: self.relays.clone(),
            ..Default::default()
        })
        .await?;
        let result = resolver.resolve_open(&self.name, NAME_WINDOW).await;
        resolver.stop().await?;
        let cid = result?;
        if let Some(current) = previous.filter(|current| current.cid == cid) {
            return Ok((current.clone(), None));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        let tree = HashTree::new(HashTreeConfig::new(Arc::new(BlossomReader {
            endpoint: self.endpoint.clone(),
            http,
        })));
        let root = if self.directory.is_empty() {
            cid.clone()
        } else {
            tree.resolve_path(&cid, &self.directory)
                .await?
                .ok_or("missing graph directory")?
        };
        let manifest = read_member(&tree, &root, "manifest.json", MAX_MANIFEST_BYTES).await?;
        let manifest: Manifest = serde_json::from_slice(&manifest)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let entry = manifest.validate(previous, now)?;
        let data = if previous.is_some_and(|old| &old.graph == entry) {
            None
        } else {
            let bytes = read_member(&tree, &root, GRAPH_FILE, entry.size).await?;
            if bytes.len() as u64 != entry.size || to_hex(&Sha256::digest(&bytes)) != entry.sha256 {
                return Err("graph size or digest differs from signed tree manifest".into());
            }
            Some(bytes)
        };
        Ok((
            Generation {
                cid,
                graph: entry.clone(),
                completed_at: manifest.graph_completed_at,
                generated_at: manifest.generated_at,
                scoring_root: manifest.root,
            },
            data,
        ))
    }
}

impl Manifest {
    fn validate(&self, previous: Option<&Generation>, now: u64) -> Result<&FileDigest> {
        let hex = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        };
        if self.version != 1
            || !hex(&self.root)
            || self.graph_completed_at == 0
            || self.graph_since > self.graph_completed_at
            || self.graph_completed_at > self.generated_at
            || self.generated_at > now.saturating_add(300)
        {
            return Err("invalid graph manifest".into());
        }
        if previous.is_some_and(|old| {
            old.scoring_root != self.root
                || self.graph_completed_at < old.completed_at
                || self.generated_at < old.generated_at
        }) {
            return Err("graph generation moved backwards or changed scoring root".into());
        }
        let graph = self
            .files
            .get(GRAPH_FILE)
            .ok_or("graph binary missing from manifest")?;
        if graph.size == 0 || graph.size > MAX_GRAPH_BYTES || !hex(&graph.sha256) {
            return Err("invalid graph manifest entry".into());
        }
        Ok(graph)
    }
}

async fn read_member<S: Store>(
    tree: &HashTree<S>,
    root: &Cid,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    let file = tree
        .resolve_path(root, name)
        .await?
        .ok_or_else(|| format!("missing {name} in graph tree"))?;
    tree.get(&file, Some(limit))
        .await?
        .ok_or_else(|| format!("missing {name} content").into())
}

struct BlossomReader {
    endpoint: Url,
    http: reqwest::Client,
}

#[async_trait]
impl Store for BlossomReader {
    async fn get(&self, hash: &Hash) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        let error = |e: reqwest::Error| StoreError::Other(e.without_url().to_string());
        let url = self
            .endpoint
            .join(&format!("{}.bin", to_hex(hash)))
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let mut response = self.http.get(url).send().await.map_err(error)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response.error_for_status_ref().map_err(error)?;
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BLOCK_BYTES as u64)
        {
            return Err(StoreError::Other("oversize Hashtree block".into()));
        }
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(error)? {
            if chunk.len() > MAX_BLOCK_BYTES - data.len() {
                return Err(StoreError::Other("oversize Hashtree block".into()));
            }
            data.extend_from_slice(&chunk);
        }
        if !hashtree_core::verify(hash, &data) {
            return Err(StoreError::Other("invalid Hashtree block digest".into()));
        }
        Ok(Some(data))
    }
    async fn has(&self, hash: &Hash) -> std::result::Result<bool, StoreError> {
        Ok(self.get(hash).await?.is_some())
    }
    async fn put(&self, _: Hash, _: Vec<u8>) -> std::result::Result<bool, StoreError> {
        Err(StoreError::Other("read-only".into()))
    }
    async fn delete(&self, _: &Hash) -> std::result::Result<bool, StoreError> {
        Err(StoreError::Other("read-only".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_social_graph::ExternalSocialGraph;
    use futures_util::{SinkExt, StreamExt};
    use hashtree_core::{DirEntry, MemoryStore};
    use nostr_sdk::prelude::{EventBuilder, Keys, Kind, Tag, ToBech32};
    use serde_json::{json, Value};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        RwLock,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    async fn publish(
        store: Arc<MemoryStore>,
        keys: &Keys,
        graph: &[u8],
        completed: u64,
        wrong_digest: bool,
    ) -> Value {
        let tree = HashTree::new(HashTreeConfig::new(store));
        let (binary, len) = tree.put(graph).await.unwrap();
        let digest = if wrong_digest {
            "0".repeat(64)
        } else {
            to_hex(&Sha256::digest(graph))
        };
        let manifest = serde_json::to_vec(&json!({"version":1,"root":"ab".repeat(32),
            "graph_completed_at":completed,"graph_since":completed-1,"generated_at":completed,
            "files":{GRAPH_FILE:{"size":len, "sha256": digest}}}))
        .unwrap();
        let (metadata, size) = tree.put(&manifest).await.unwrap();
        let root = tree
            .put_directory(vec![
                DirEntry::from_cid("manifest.json", &metadata).with_size(size),
                DirEntry::from_cid(GRAPH_FILE, &binary).with_size(len),
            ])
            .await
            .unwrap();
        let hash = to_hex(&root.hash);
        let key = root.key.unwrap().to_hex();
        let event = EventBuilder::new(Kind::Custom(30064), "")
            .tag(Tag::parse(["d", "social-graph"]).unwrap())
            .tag(Tag::parse(["l", "hashtree"]).unwrap())
            .tag(Tag::parse(["hash", hash.as_str()]).unwrap())
            .tag(Tag::parse(["key", key.as_str()]).unwrap())
            .sign_with_keys(keys)
            .unwrap();
        serde_json::to_value(event).unwrap()
    }

    fn graph(friend: u8) -> Vec<u8> {
        // Two typed v3 pubkeys, one follow from owner 1 to owner 2, no mutes.
        let mut bytes = vec![3, 2, 1, 0];
        bytes.extend([1; 32]);
        bytes.extend([2, 0]);
        bytes.extend([friend; 32]);
        bytes.extend([1, 1, 1, 1, 2, 0]);
        bytes
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signed_tree_refresh_reuses_content_and_preserves_policy_after_failure() {
        let store = Arc::new(MemoryStore::new());
        let signer = Keys::generate();
        let current = Arc::new(RwLock::new(
            publish(store.clone(), &signer, &graph(2), 5, false).await,
        ));
        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relays = format!("ws://{}", relay.local_addr().unwrap());
        let latest = current.clone();
        let relay_task = tokio::spawn(async move {
            while let Ok((tcp, _)) = relay.accept().await {
                let latest = latest.clone();
                tokio::spawn(async move {
                    let mut ws = accept_async(tcp).await.unwrap();
                    while let Some(Ok(msg)) = ws.next().await {
                        let Ok(text) = msg.to_text() else { continue };
                        let Ok(frame) = serde_json::from_str::<Value>(text) else {
                            continue;
                        };
                        if frame[0] == "REQ" {
                            let event = latest.read().unwrap().clone();
                            let _ = ws
                                .send(Message::text(json!(["EVENT", frame[1], event]).to_string()))
                                .await;
                            let _ = ws
                                .send(Message::text(json!(["EOSE", frame[1]]).to_string()))
                                .await;
                        }
                    }
                });
            }
        });
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", http.local_addr().unwrap());
        let gets = Arc::new(AtomicUsize::new(0));
        let counter = gets.clone();
        let reader = store.clone();
        let http_task = tokio::spawn(async move {
            while let Ok((tcp, _)) = http.accept().await {
                let mut io = BufReader::new(tcp);
                let mut line = String::new();
                io.read_line(&mut line).await.unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_string();
                loop {
                    line.clear();
                    if io.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let Some(hex) = path.strip_prefix('/').and_then(|p| p.strip_suffix(".bin")) else {
                    continue;
                };
                let hash = hashtree_core::from_hex(hex).unwrap();
                let data = reader.get(&hash).await.unwrap().unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                io.get_mut().write_all(head.as_bytes()).await.unwrap();
                io.get_mut().write_all(&data).await.unwrap();
            }
        });
        let uri = format!(
            "htree://{}/social-graph",
            signer.public_key().to_bech32().unwrap()
        );
        let source = NamedSnapshot::new(&uri, &relays, &origin).unwrap();
        let root = "01".repeat(32);
        let friend = "02".repeat(32);
        let next = "03".repeat(32);
        let policy = ExternalSocialGraph::open_named(source, &root).unwrap();
        assert!(policy.is_pubkey_in_graph(&friend).unwrap());
        let initial = gets.load(Ordering::SeqCst);
        policy.refresh(&uri, &root).unwrap();
        assert_eq!(gets.load(Ordering::SeqCst), initial);
        let replace = |event| {
            *current.write().unwrap() = event;
        };
        replace(publish(store.clone(), &signer, &graph(3), 6, true).await);
        assert!(policy.refresh(&uri, &root).is_err());
        assert!(policy.is_pubkey_in_graph(&friend).unwrap());
        replace(publish(store.clone(), &signer, &graph(3), 7, false).await);
        policy.refresh(&uri, &root).unwrap();
        assert!(policy.is_pubkey_in_graph(&next).unwrap());
        assert!(!policy.is_pubkey_in_graph(&friend).unwrap());
        replace(publish(store, &signer, &graph(2), 5, false).await);
        assert!(policy.refresh(&uri, &root).is_err());
        assert!(policy.is_pubkey_in_graph(&next).unwrap());
        relay_task.abort();
        http_task.abort();
    }
}
