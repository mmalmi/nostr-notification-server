use heed::byteorder::BigEndian;
use heed::types::{SerdeBincode, Str, U32};
use heed::{Database, Env, EnvFlags, EnvOpenOptions, Error as HeedError, RoTxn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;

const DEFAULT_MAP_SIZE: usize = 4 * 1024 * 1024 * 1024;
const MAX_DBS: u32 = 16;
const OPEN_RETRY_ATTEMPTS: usize = 40;
const OPEN_RETRY_DELAY_MS: u64 = 250;
const HTTP_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);
const SOCIAL_GRAPH_BINARY_FORMAT_VERSION: u64 = 2;
const UNKNOWN_FOLLOW_DISTANCE: u32 = 1000;

const STR_TO_UNIQUE_ID_DB: &str = "str_to_unique_id";
const FOLLOW_DISTANCE_BY_USER_DB: &str = "follow_distance_by_user";
const FOLLOWED_BY_USER_DB: &str = "followed_by_user";
const FOLLOWERS_BY_USER_DB: &str = "followers_by_user";
const MUTED_BY_USER_DB: &str = "muted_by_user";
const USER_MUTED_BY_DB: &str = "user_muted_by";
const OVERMUTE_THRESHOLD: usize = 3;

pub struct ExternalSocialGraph {
    policy: RwLock<VisibilityPolicy>,
}

struct VisibilityPolicy {
    pubkey_to_id: HashMap<String, u32>,
    follow_distance_by_user: HashMap<u32, u32>,
    followed_by_user: HashMap<u32, HashSet<u32>>,
    muted_by_user: HashMap<u32, HashSet<u32>>,
    overmuted_users: HashSet<u32>,
}

#[derive(Debug)]
struct ExternalSocialGraphOpenError {
    step: String,
    source: Box<dyn Error + Send + Sync>,
}

impl ExternalSocialGraphOpenError {
    fn boxed(
        step: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Box<dyn Error + Send + Sync> {
        Box::new(Self {
            step: step.into(),
            source: Box::new(source),
        })
    }

    fn boxed_source(
        step: impl Into<String>,
        source: Box<dyn Error + Send + Sync>,
    ) -> Box<dyn Error + Send + Sync> {
        Box::new(Self {
            step: step.into(),
            source,
        })
    }
}

impl fmt::Display for ExternalSocialGraphOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.step, self.source)
    }
}

impl Error for ExternalSocialGraphOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl ExternalSocialGraph {
    pub fn open(source: &str, root_pubkey: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if source.starts_with("http://") {
            return Self::from_social_graph_url(source, root_pubkey);
        }
        if source.starts_with("https://") {
            return Err(
                "https social graph snapshots are not supported; use an internal http URL".into(),
            );
        }

        let path = Path::new(source);
        if !path.exists() {
            return Err(format!("social graph path does not exist: {}", path.display()).into());
        }

        open_read_only_graph_with_retries(path, root_pubkey)
    }

    fn from_env(env: Env, root_pubkey: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let rtxn = env.read_txn().map_err(|error| {
            let info = env.info();
            ExternalSocialGraphOpenError::boxed(
                format!(
                    "open read transaction (readers {}/{}, map {} bytes)",
                    info.number_of_readers, info.maximum_number_of_readers, info.map_size
                ),
                error,
            )
        })?;
        let str_to_unique_id =
            open_required_database::<Str, U32<BigEndian>>(&env, &rtxn, STR_TO_UNIQUE_ID_DB)
                .map_err(|error| {
                    ExternalSocialGraphOpenError::boxed_source(
                        format!("open {STR_TO_UNIQUE_ID_DB} database"),
                        error,
                    )
                })?;
        let _follow_distance_by_user = open_required_database::<U32<BigEndian>, U32<BigEndian>>(
            &env,
            &rtxn,
            FOLLOW_DISTANCE_BY_USER_DB,
        )
        .map_err(|error| {
            ExternalSocialGraphOpenError::boxed_source(
                format!("open {FOLLOW_DISTANCE_BY_USER_DB} database"),
                error,
            )
        })?;
        let followed_by_user = open_required_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
            &env,
            &rtxn,
            FOLLOWED_BY_USER_DB,
        )
        .map_err(|error| {
            ExternalSocialGraphOpenError::boxed_source(
                format!("open {FOLLOWED_BY_USER_DB} database"),
                error,
            )
        })?;
        let followers_by_user = open_required_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
            &env,
            &rtxn,
            FOLLOWERS_BY_USER_DB,
        )
        .map_err(|error| {
            ExternalSocialGraphOpenError::boxed_source(
                format!("open {FOLLOWERS_BY_USER_DB} database"),
                error,
            )
        })?;
        let muted_by_user = open_required_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
            &env,
            &rtxn,
            MUTED_BY_USER_DB,
        )
        .map_err(|error| {
            ExternalSocialGraphOpenError::boxed_source(
                format!("open {MUTED_BY_USER_DB} database"),
                error,
            )
        })?;
        let user_muted_by = open_required_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
            &env,
            &rtxn,
            USER_MUTED_BY_DB,
        )
        .map_err(|error| {
            ExternalSocialGraphOpenError::boxed_source(
                format!("open {USER_MUTED_BY_DB} database"),
                error,
            )
        })?;

        let root_id = str_to_unique_id
            .get(&rtxn, root_pubkey)?
            .ok_or("external social graph does not contain the configured root")?;
        if _follow_distance_by_user.get(&rtxn, &root_id)? != Some(0) {
            return Err("external social graph root does not match the configured root".into());
        }

        let pubkey_to_id = str_to_unique_id
            .iter(&rtxn)?
            .map(|entry| entry.map(|(pubkey, id)| (pubkey.to_string(), id)))
            .collect::<Result<HashMap<_, _>, _>>()?;
        let followed_by_user = collect_id_sets(&followed_by_user, &rtxn)?;
        let followers_by_user = collect_id_sets(&followers_by_user, &rtxn)?;
        let muted_by_user = collect_id_sets(&muted_by_user, &rtxn)?;
        let user_muted_by = collect_id_sets(&user_muted_by, &rtxn)?;
        drop(rtxn);

        Ok(Self {
            policy: VisibilityPolicy::new(
                root_pubkey,
                pubkey_to_id,
                followed_by_user,
                followers_by_user,
                muted_by_user,
                user_muted_by,
            )?
            .into(),
        })
    }

    pub fn is_pubkey_in_graph(&self, pubkey: &str) -> Result<bool, Box<dyn Error + Send + Sync>> {
        Ok(self
            .policy
            .read()
            .map_err(|_| "graph policy lock poisoned")?
            .is_pubkey_in_graph(pubkey))
    }

    pub fn recipient_has_muted_author(
        &self,
        recipient: &str,
        author: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        Ok(self
            .policy
            .read()
            .map_err(|_| "graph policy lock poisoned")?
            .recipient_has_muted_author(recipient, author))
    }

    pub fn is_author_visible(
        &self,
        recipient: &str,
        author: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        Ok(self
            .policy
            .read()
            .map_err(|_| "graph policy lock poisoned")?
            .is_author_visible(recipient, author))
    }

    pub fn refresh(
        &self,
        source: &str,
        root_pubkey: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Fetch and fully compute the next policy before replacing the active
        // snapshot. Readers never observe a partially rebuilt graph.
        let replacement = Self::open(source, root_pubkey)?;
        let policy = replacement
            .policy
            .into_inner()
            .map_err(|_| "graph policy lock poisoned")?;
        *self
            .policy
            .write()
            .map_err(|_| "graph policy lock poisoned")? = policy;
        Ok(())
    }

    fn from_social_graph_url(
        url: &str,
        root_pubkey: &str,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let bytes = fetch_http_snapshot(url)?;
        Self::from_social_graph_binary(root_pubkey, &bytes)
    }

    pub(crate) fn from_social_graph_binary(
        root_pubkey: &str,
        data: &[u8],
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let snapshot = parse_social_graph_binary(root_pubkey, data)?;
        Ok(Self {
            policy: snapshot.into(),
        })
    }
}

impl VisibilityPolicy {
    fn new(
        root_pubkey: &str,
        pubkey_to_id: HashMap<String, u32>,
        followed_by_user: HashMap<u32, HashSet<u32>>,
        followers_by_user: HashMap<u32, HashSet<u32>>,
        muted_by_user: HashMap<u32, HashSet<u32>>,
        user_muted_by: HashMap<u32, HashSet<u32>>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let root_id = *pubkey_to_id
            .get(root_pubkey)
            .ok_or("social graph does not contain the configured root")?;
        let follow_distance_by_user = calculate_follow_distances(root_id, &followed_by_user);
        let mut overmuted_users = calculate_overmuted_users(
            &pubkey_to_id,
            &follow_distance_by_user,
            &followers_by_user,
            &user_muted_by,
        );
        overmuted_users.remove(&root_id);

        Ok(Self {
            pubkey_to_id,
            follow_distance_by_user,
            followed_by_user,
            muted_by_user,
            overmuted_users,
        })
    }

    fn is_pubkey_in_graph(&self, pubkey: &str) -> bool {
        self.pubkey_to_id
            .get(pubkey)
            .and_then(|pubkey_id| self.follow_distance_by_user.get(pubkey_id))
            .is_some_and(|distance| *distance < UNKNOWN_FOLLOW_DISTANCE)
    }

    fn recipient_has_muted_author(&self, recipient: &str, author: &str) -> bool {
        let Some(recipient_id) = self.pubkey_to_id.get(recipient) else {
            return false;
        };
        let Some(author_id) = self.pubkey_to_id.get(author) else {
            return false;
        };

        self.muted_by_user
            .get(recipient_id)
            .is_some_and(|muted| muted.contains(author_id))
    }

    fn is_author_visible(&self, recipient: &str, author: &str) -> bool {
        if self.recipient_has_muted_author(recipient, author) {
            return false;
        }
        if recipient == author {
            return true;
        }

        let Some(author_id) = self.pubkey_to_id.get(author) else {
            return false;
        };
        if self
            .pubkey_to_id
            .get(recipient)
            .and_then(|recipient_id| self.followed_by_user.get(recipient_id))
            .is_some_and(|followed| followed.contains(author_id))
        {
            return true;
        }

        self.follow_distance_by_user
            .get(author_id)
            .is_some_and(|distance| *distance < UNKNOWN_FOLLOW_DISTANCE)
            && !self.overmuted_users.contains(author_id)
    }
}

fn collect_id_sets(
    database: &Database<U32<BigEndian>, SerdeBincode<Vec<u32>>>,
    rtxn: &RoTxn<'_>,
) -> Result<HashMap<u32, HashSet<u32>>, Box<dyn Error + Send + Sync>> {
    Ok(database
        .iter(rtxn)?
        .map(|entry| entry.map(|(owner, values)| (owner, values.into_iter().collect())))
        .collect::<Result<HashMap<_, _>, _>>()?)
}

fn invert_edges(edges_by_owner: &HashMap<u32, HashSet<u32>>) -> HashMap<u32, HashSet<u32>> {
    let mut owners_by_target: HashMap<u32, HashSet<u32>> = HashMap::new();
    for (owner, targets) in edges_by_owner {
        for target in targets {
            owners_by_target.entry(*target).or_default().insert(*owner);
        }
    }
    owners_by_target
}

fn calculate_overmuted_users(
    pubkey_to_id: &HashMap<String, u32>,
    follow_distance_by_user: &HashMap<u32, u32>,
    followers_by_user: &HashMap<u32, HashSet<u32>>,
    user_muted_by: &HashMap<u32, HashSet<u32>>,
) -> HashSet<u32> {
    let mut overmuted = HashSet::new();
    for target in pubkey_to_id.values() {
        let mut nearest_distance = UNKNOWN_FOLLOW_DISTANCE;
        let mut nearest_followers = 0usize;
        let mut nearest_muters = 0usize;

        let mut record_opinion = |opinion_user: &u32, is_mute: bool| {
            let distance = follow_distance_by_user
                .get(opinion_user)
                .copied()
                .unwrap_or(UNKNOWN_FOLLOW_DISTANCE);
            if distance >= UNKNOWN_FOLLOW_DISTANCE {
                return;
            }
            if distance < nearest_distance {
                nearest_distance = distance;
                nearest_followers = 0;
                nearest_muters = 0;
            }
            if distance != nearest_distance {
                return;
            }
            if is_mute {
                nearest_muters += 1;
            } else {
                nearest_followers += 1;
            }
        };

        for follower in followers_by_user.get(target).into_iter().flatten() {
            record_opinion(follower, false);
        }
        for muter in user_muted_by.get(target).into_iter().flatten() {
            record_opinion(muter, true);
        }

        if nearest_distance < UNKNOWN_FOLLOW_DISTANCE
            && nearest_muters.saturating_mul(OVERMUTE_THRESHOLD) > nearest_followers
        {
            overmuted.insert(*target);
        }
    }
    overmuted
}

fn open_read_only_graph_with_retries(
    path: &Path,
    root_pubkey: &str,
) -> Result<ExternalSocialGraph, Box<dyn Error + Send + Sync>> {
    for attempt in 0..=OPEN_RETRY_ATTEMPTS {
        match open_read_only_graph(path, root_pubkey) {
            Ok(graph) => return Ok(graph),
            Err(error)
                if is_temporary_resource_error(error.as_ref()) && attempt < OPEN_RETRY_ATTEMPTS =>
            {
                thread::sleep(Duration::from_millis(OPEN_RETRY_DELAY_MS));
            }
            Err(error) => return Err(error),
        }
    }

    open_read_only_graph(path, root_pubkey)
}

fn open_read_only_graph(
    path: &Path,
    root_pubkey: &str,
) -> Result<ExternalSocialGraph, Box<dyn Error + Send + Sync>> {
    let env = open_read_only_env(path)
        .map_err(|error| ExternalSocialGraphOpenError::boxed("open read-only LMDB env", error))?;
    env.clear_stale_readers().map_err(|error| {
        let info = env.info();
        ExternalSocialGraphOpenError::boxed(
            format!(
                "clear stale readers (readers {}/{}, map {} bytes)",
                info.number_of_readers, info.maximum_number_of_readers, info.map_size
            ),
            error,
        )
    })?;
    ExternalSocialGraph::from_env(env, root_pubkey)
}

fn open_read_only_env(path: &Path) -> Result<Env, HeedError> {
    let mut options = EnvOpenOptions::new();
    options.map_size(DEFAULT_MAP_SIZE).max_dbs(MAX_DBS);
    unsafe {
        options.flags(EnvFlags::READ_ONLY);
        options.open(path)
    }
}

fn is_temporary_resource_error(error: &(dyn Error + 'static)) -> bool {
    if let Some(heed_error) = error.downcast_ref::<HeedError>() {
        if matches!(
            heed_error,
            HeedError::Io(io_error)
                if io_error.kind() == io::ErrorKind::WouldBlock
                    || io_error.raw_os_error() == Some(11)
        ) {
            return true;
        }
    }

    if let Some(io_error) = error.downcast_ref::<io::Error>() {
        if io_error.kind() == io::ErrorKind::WouldBlock || io_error.raw_os_error() == Some(11) {
            return true;
        }
    }

    error.source().is_some_and(is_temporary_resource_error)
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
    env.open_database(rtxn, Some(name))?.ok_or_else(|| {
        format!("required database missing from external social graph: {name}").into()
    })
}

fn parse_social_graph_binary(
    root_pubkey: &str,
    data: &[u8],
) -> Result<VisibilityPolicy, Box<dyn Error + Send + Sync>> {
    let mut offset = 0usize;
    let version = read_varint(data, &mut offset)?;
    if version != SOCIAL_GRAPH_BINARY_FORMAT_VERSION {
        return Err(format!("unsupported social graph binary version: {version}").into());
    }

    let ids_count = usize::try_from(read_varint(data, &mut offset)?)
        .map_err(|_| "social graph ids count does not fit in memory")?;
    let mut pubkey_to_id = HashMap::with_capacity(ids_count);
    for _ in 0..ids_count {
        let pubkey = hex_lower(read_bytes(data, &mut offset, 32)?);
        let id = u32::try_from(read_varint(data, &mut offset)?)
            .map_err(|_| "social graph id exceeds u32")?;
        pubkey_to_id.insert(pubkey, id);
    }

    let follow_lists_count = usize::try_from(read_varint(data, &mut offset)?)
        .map_err(|_| "social graph follow list count does not fit in memory")?;
    let mut followed_by_user = HashMap::with_capacity(follow_lists_count);
    for _ in 0..follow_lists_count {
        let owner = u32::try_from(read_varint(data, &mut offset)?)
            .map_err(|_| "social graph follow owner id exceeds u32")?;
        let _created_at = read_varint(data, &mut offset)?;
        let followed_count = usize::try_from(read_varint(data, &mut offset)?)
            .map_err(|_| "social graph follow count does not fit in memory")?;
        let mut followed = HashSet::with_capacity(followed_count);
        for _ in 0..followed_count {
            followed.insert(
                u32::try_from(read_varint(data, &mut offset)?)
                    .map_err(|_| "social graph followed id exceeds u32")?,
            );
        }
        followed_by_user.insert(owner, followed);
    }

    let mute_lists_count = usize::try_from(read_varint(data, &mut offset)?)
        .map_err(|_| "social graph mute list count does not fit in memory")?;
    let mut muted_by_user = HashMap::with_capacity(mute_lists_count);
    for _ in 0..mute_lists_count {
        let owner = u32::try_from(read_varint(data, &mut offset)?)
            .map_err(|_| "social graph mute owner id exceeds u32")?;
        let _created_at = read_varint(data, &mut offset)?;
        let muted_count = usize::try_from(read_varint(data, &mut offset)?)
            .map_err(|_| "social graph mute count does not fit in memory")?;
        let mut muted = HashSet::with_capacity(muted_count);
        for _ in 0..muted_count {
            muted.insert(
                u32::try_from(read_varint(data, &mut offset)?)
                    .map_err(|_| "social graph muted id exceeds u32")?,
            );
        }
        muted_by_user.insert(owner, muted);
    }

    if offset != data.len() {
        return Err("social graph binary has trailing bytes".into());
    }

    let followers_by_user = invert_edges(&followed_by_user);
    let user_muted_by = invert_edges(&muted_by_user);
    VisibilityPolicy::new(
        root_pubkey,
        pubkey_to_id,
        followed_by_user,
        followers_by_user,
        muted_by_user,
        user_muted_by,
    )
}

fn calculate_follow_distances(
    root_id: u32,
    followed_by_user: &HashMap<u32, HashSet<u32>>,
) -> HashMap<u32, u32> {
    let mut distances: HashMap<u32, u32> = HashMap::new();
    distances.insert(root_id, 0);

    let mut queue = VecDeque::from([root_id]);
    while let Some(user) = queue.pop_front() {
        let Some(distance) = distances.get(&user).copied() else {
            continue;
        };
        let Some(followed_users) = followed_by_user.get(&user) else {
            continue;
        };
        let next_distance = distance.saturating_add(1);
        for followed_user in followed_users {
            if distances.contains_key(followed_user) {
                continue;
            }
            distances.insert(*followed_user, next_distance);
            queue.push_back(*followed_user);
        }
    }

    distances
}

fn fetch_http_snapshot(url: &str) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
    let (host, port, path) = parse_http_url(url)?;
    let address = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or("social graph snapshot host did not resolve")?;
    let mut stream = TcpStream::connect_timeout(&address, HTTP_SNAPSHOT_TIMEOUT)?;
    stream.set_read_timeout(Some(HTTP_SNAPSHOT_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_SNAPSHOT_TIMEOUT))?;

    let host_header = if port == 80 {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nAccept: application/octet-stream\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("social graph snapshot response is missing headers")?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let status = headers
        .lines()
        .next()
        .ok_or("social graph snapshot response is empty")?;
    if !status.contains(" 200 ") {
        return Err(format!("social graph snapshot request failed: {status}").into());
    }

    let body = &response[header_end + 4..];
    if headers
        .to_ascii_lowercase()
        .lines()
        .any(|line| line.trim() == "transfer-encoding: chunked")
    {
        decode_chunked_body(body)
    } else {
        Ok(body.to_vec())
    }
}

fn parse_http_url(url: &str) -> Result<(String, u16, String), Box<dyn Error + Send + Sync>> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("only http social graph snapshot URLs are supported")?;
    let (authority, path) = rest
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((rest, "/".to_string()));
    if authority.is_empty() {
        return Err("social graph snapshot URL is missing a host".into());
    }

    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => (host.to_string(), port.parse()?),
        _ => (authority.to_string(), 80),
    };

    Ok((host, port, path))
}

fn decode_chunked_body(body: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
    let mut offset = 0usize;
    let mut decoded = Vec::new();
    loop {
        let line_end = find_crlf(body, offset).ok_or("invalid chunked social graph response")?;
        let line = std::str::from_utf8(&body[offset..line_end])?;
        let size_text = line.split(';').next().unwrap_or(line).trim();
        let size = usize::from_str_radix(size_text, 16)?;
        offset = line_end + 2;
        if size == 0 {
            return Ok(decoded);
        }
        let end = offset
            .checked_add(size)
            .ok_or("chunked social graph response is too large")?;
        if end + 2 > body.len() || &body[end..end + 2] != b"\r\n" {
            return Err("invalid chunked social graph response body".into());
        }
        decoded.extend_from_slice(&body[offset..end]);
        offset = end + 2;
    }
}

fn find_crlf(bytes: &[u8], start: usize) -> Option<usize> {
    bytes
        .get(start..)?
        .windows(2)
        .position(|window| window == b"\r\n")
        .map(|position| start + position)
}

fn read_varint(data: &[u8], offset: &mut usize) -> Result<u64, Box<dyn Error + Send + Sync>> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        if shift >= 64 {
            return Err("social graph varint is too large".into());
        }
        let byte = *data
            .get(*offset)
            .ok_or("unexpected end of social graph binary")?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn read_bytes<'a>(
    data: &'a [u8],
    offset: &mut usize,
    len: usize,
) -> Result<&'a [u8], Box<dyn Error + Send + Sync>> {
    let end = offset
        .checked_add(len)
        .ok_or("social graph binary offset overflow")?;
    if end > data.len() {
        return Err("unexpected end of social graph binary".into());
    }
    let slice = &data[*offset..end];
    *offset = end;
    Ok(slice)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn refresh_updates_visibility_and_keeps_complete_policy_on_failure() {
        let root = "11".repeat(32);
        let friend = "22".repeat(32);
        let sender = "33".repeat(32);
        let initial = binary_snapshot(&[&root, &friend, &sender], &[(1, &[2]), (2, &[3])], &[]);
        let muted = binary_snapshot(
            &[&root, &friend, &sender],
            &[(1, &[2]), (2, &[3])],
            &[(1, &[3])],
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/social-graph", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for body in [initial, muted, vec![255]] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let graph = ExternalSocialGraph::open(&url, &root).unwrap();
        assert!(graph.is_author_visible(&root, &sender).unwrap());
        graph.refresh(&url, &root).unwrap();
        assert!(!graph.is_author_visible(&root, &sender).unwrap());
        assert!(graph.is_author_visible(&root, &friend).unwrap());
        assert!(graph.refresh(&url, &root).is_err());
        assert!(!graph.is_author_visible(&root, &sender).unwrap());
        assert!(graph.is_author_visible(&root, &friend).unwrap());
        server.join().unwrap();
    }

    const TEST_MAP_SIZE: usize = 128 * 1024 * 1024;

    #[test]
    fn reads_follow_distances_and_mute_lists_from_snapshot() {
        let path =
            std::env::temp_dir().join(format!("nns-external-social-graph-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();

        let env = write_lmdb_snapshot(
            &path,
            &[
                ("recipient", 1),
                ("allowed", 2),
                ("muted", 3),
                ("distant", 4),
            ],
            1,
            &[(1, &[2]), (2, &[3])],
            &[(1, &[3])],
        );

        let graph = ExternalSocialGraph::from_env(env, "recipient").unwrap();

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
        assert!(graph.is_author_visible("recipient", "allowed").unwrap());
        assert!(!graph.is_author_visible("recipient", "muted").unwrap());

        drop(graph);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn lmdb_snapshot_precomputes_overmute_visibility() {
        let path =
            std::env::temp_dir().join(format!("nns-external-visibility-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        let env = write_lmdb_snapshot(
            &path,
            &[("root", 1), ("follower", 2), ("muter", 3), ("target", 4)],
            1,
            &[(1, &[2, 3]), (2, &[4])],
            &[(3, &[4])],
        );

        let graph = ExternalSocialGraph::from_env(env, "root").unwrap();
        assert!(!graph.is_author_visible("root", "target").unwrap());

        drop(graph);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn reads_follow_distances_and_mute_lists_from_binary_snapshot() {
        let root = repeated_hex(1);
        let allowed = repeated_hex(2);
        let muted = repeated_hex(3);
        let distant = repeated_hex(4);
        let recipient = repeated_hex(5);

        let mut bytes = Vec::new();
        write_varint_for_test(&mut bytes, SOCIAL_GRAPH_BINARY_FORMAT_VERSION);
        write_varint_for_test(&mut bytes, 5);
        push_id_for_test(&mut bytes, &root, 1);
        push_id_for_test(&mut bytes, &allowed, 2);
        push_id_for_test(&mut bytes, &muted, 3);
        push_id_for_test(&mut bytes, &distant, 4);
        push_id_for_test(&mut bytes, &recipient, 5);

        write_varint_for_test(&mut bytes, 1);
        write_varint_for_test(&mut bytes, 1);
        write_varint_for_test(&mut bytes, 0);
        write_varint_for_test(&mut bytes, 2);
        write_varint_for_test(&mut bytes, 2);
        write_varint_for_test(&mut bytes, 3);

        write_varint_for_test(&mut bytes, 1);
        write_varint_for_test(&mut bytes, 5);
        write_varint_for_test(&mut bytes, 0);
        write_varint_for_test(&mut bytes, 1);
        write_varint_for_test(&mut bytes, 3);

        let graph = ExternalSocialGraph::from_social_graph_binary(&root, &bytes).unwrap();

        assert!(graph.is_pubkey_in_graph(&allowed).unwrap());
        assert!(graph.is_pubkey_in_graph(&muted).unwrap());
        assert!(!graph.is_pubkey_in_graph(&distant).unwrap());
        assert!(!graph.is_pubkey_in_graph(&repeated_hex(6)).unwrap());

        assert!(graph
            .recipient_has_muted_author(&recipient, &muted)
            .unwrap());
        assert!(!graph
            .recipient_has_muted_author(&recipient, &allowed)
            .unwrap());
        assert!(!graph
            .recipient_has_muted_author(&repeated_hex(6), &muted)
            .unwrap());
    }

    #[test]
    fn blocks_one_near_muter_against_one_near_follower() {
        let root = repeated_hex(1);
        let follower = repeated_hex(2);
        let muter = repeated_hex(3);
        let target = repeated_hex(4);
        let bytes = binary_snapshot(
            &[&root, &follower, &muter, &target],
            &[(1, &[2, 3]), (2, &[4])],
            &[(3, &[4])],
        );
        let graph = ExternalSocialGraph::from_social_graph_binary(&root, &bytes).unwrap();

        assert!(!graph.is_author_visible(&root, &target).unwrap());
    }

    #[test]
    fn blocks_unknown_authors() {
        let root = repeated_hex(1);
        let known = repeated_hex(2);
        let unreachable = repeated_hex(3);
        let missing = repeated_hex(4);
        let bytes = binary_snapshot(&[&root, &known, &unreachable], &[(1, &[2])], &[]);
        let graph = ExternalSocialGraph::from_social_graph_binary(&root, &bytes).unwrap();

        assert!(graph.is_author_visible(&root, &known).unwrap());
        assert!(!graph.is_author_visible(&root, &unreachable).unwrap());
        assert!(!graph.is_author_visible(&root, &missing).unwrap());
    }

    #[test]
    fn preserves_self_and_direct_follows_unless_directly_muted() {
        let root = repeated_hex(1);
        let followed = repeated_hex(2);
        let bytes = binary_snapshot(&[&root, &followed], &[(1, &[2])], &[]);
        let graph = ExternalSocialGraph::from_social_graph_binary(&root, &bytes).unwrap();

        assert!(graph.is_author_visible(&root, &root).unwrap());
        assert!(graph.is_author_visible(&root, &followed).unwrap());

        let muted_bytes = binary_snapshot(&[&root, &followed], &[(1, &[2])], &[(1, &[2])]);
        let muted_graph =
            ExternalSocialGraph::from_social_graph_binary(&root, &muted_bytes).unwrap();
        assert!(!muted_graph.is_author_visible(&root, &followed).unwrap());
    }

    #[test]
    fn recipient_direct_follow_bypasses_global_overmute() {
        let root = repeated_hex(1);
        let follower = repeated_hex(2);
        let muter = repeated_hex(3);
        let target = repeated_hex(4);
        let recipient = repeated_hex(5);
        let bytes = binary_snapshot(
            &[&root, &follower, &muter, &target, &recipient],
            &[(1, &[2, 3, 5]), (2, &[4]), (5, &[4])],
            &[(3, &[4])],
        );
        let graph = ExternalSocialGraph::from_social_graph_binary(&root, &bytes).unwrap();

        assert!(!graph.is_author_visible(&root, &target).unwrap());
        assert!(graph.is_author_visible(&recipient, &target).unwrap());
    }

    #[test]
    fn configured_root_is_never_globally_overmuted() {
        let root = repeated_hex(1);
        let muter = repeated_hex(2);
        let recipient = repeated_hex(3);
        let bytes = binary_snapshot(&[&root, &muter, &recipient], &[(1, &[2, 3])], &[(2, &[1])]);
        let graph = ExternalSocialGraph::from_social_graph_binary(&root, &bytes).unwrap();

        assert!(graph.is_author_visible(&recipient, &root).unwrap());
    }

    #[test]
    fn ignores_farther_mutes_when_a_nearer_follow_opinion_exists() {
        let root = repeated_hex(1);
        let near_follower = repeated_hex(2);
        let bridge = repeated_hex(3);
        let far_muter = repeated_hex(4);
        let target = repeated_hex(5);
        let bytes = binary_snapshot(
            &[&root, &near_follower, &bridge, &far_muter, &target],
            &[(1, &[2, 3]), (2, &[5]), (3, &[4])],
            &[(4, &[5])],
        );
        let graph = ExternalSocialGraph::from_social_graph_binary(&root, &bytes).unwrap();

        assert!(graph.is_author_visible(&root, &target).unwrap());
    }

    fn write_lmdb_snapshot(
        path: &Path,
        pubkeys: &[(&str, u32)],
        root_id: u32,
        follows: &[(u32, &[u32])],
        mutes: &[(u32, &[u32])],
    ) -> Env {
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(TEST_MAP_SIZE)
                .max_dbs(MAX_DBS)
                .open(path)
                .unwrap()
        };
        let followed_sets: HashMap<u32, HashSet<u32>> = follows
            .iter()
            .map(|(owner, targets)| (*owner, targets.iter().copied().collect()))
            .collect();
        let muted_sets: HashMap<u32, HashSet<u32>> = mutes
            .iter()
            .map(|(owner, targets)| (*owner, targets.iter().copied().collect()))
            .collect();
        let followers = invert_edges(&followed_sets);
        let muters = invert_edges(&muted_sets);
        let distances = calculate_follow_distances(root_id, &followed_sets);

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
            let followed_by_user = env
                .create_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
                    &mut wtxn,
                    Some(FOLLOWED_BY_USER_DB),
                )
                .unwrap();
            let followers_by_user = env
                .create_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
                    &mut wtxn,
                    Some(FOLLOWERS_BY_USER_DB),
                )
                .unwrap();
            let muted_by_user = env
                .create_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
                    &mut wtxn,
                    Some(MUTED_BY_USER_DB),
                )
                .unwrap();
            let user_muted_by = env
                .create_database::<U32<BigEndian>, SerdeBincode<Vec<u32>>>(
                    &mut wtxn,
                    Some(USER_MUTED_BY_DB),
                )
                .unwrap();

            for (pubkey, id) in pubkeys {
                str_to_unique_id.put(&mut wtxn, pubkey, id).unwrap();
            }
            for (id, distance) in distances {
                follow_distance_by_user
                    .put(&mut wtxn, &id, &distance)
                    .unwrap();
            }
            for (owner, targets) in &followed_sets {
                followed_by_user
                    .put(&mut wtxn, owner, &targets.iter().copied().collect())
                    .unwrap();
            }
            for (target, owners) in &followers {
                followers_by_user
                    .put(&mut wtxn, target, &owners.iter().copied().collect())
                    .unwrap();
            }
            for (owner, targets) in &muted_sets {
                muted_by_user
                    .put(&mut wtxn, owner, &targets.iter().copied().collect())
                    .unwrap();
            }
            for (target, owners) in &muters {
                user_muted_by
                    .put(&mut wtxn, target, &owners.iter().copied().collect())
                    .unwrap();
            }
            wtxn.commit().unwrap();
        }
        env
    }

    fn repeated_hex(byte: u8) -> String {
        (0..32).map(|_| format!("{byte:02x}")).collect()
    }

    fn push_id_for_test(bytes: &mut Vec<u8>, pubkey: &str, id: u64) {
        for index in (0..pubkey.len()).step_by(2) {
            bytes.push(u8::from_str_radix(&pubkey[index..index + 2], 16).unwrap());
        }
        write_varint_for_test(bytes, id);
    }

    fn binary_snapshot(
        pubkeys: &[&str],
        follows: &[(u64, &[u64])],
        mutes: &[(u64, &[u64])],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_varint_for_test(&mut bytes, SOCIAL_GRAPH_BINARY_FORMAT_VERSION);
        write_varint_for_test(&mut bytes, pubkeys.len() as u64);
        for (index, pubkey) in pubkeys.iter().enumerate() {
            push_id_for_test(&mut bytes, pubkey, (index + 1) as u64);
        }

        write_varint_for_test(&mut bytes, follows.len() as u64);
        for (owner, followed) in follows {
            write_varint_for_test(&mut bytes, *owner);
            write_varint_for_test(&mut bytes, 0);
            write_varint_for_test(&mut bytes, followed.len() as u64);
            for target in *followed {
                write_varint_for_test(&mut bytes, *target);
            }
        }

        write_varint_for_test(&mut bytes, mutes.len() as u64);
        for (owner, muted) in mutes {
            write_varint_for_test(&mut bytes, *owner);
            write_varint_for_test(&mut bytes, 0);
            write_varint_for_test(&mut bytes, muted.len() as u64);
            for target in *muted {
                write_varint_for_test(&mut bytes, *target);
            }
        }
        bytes
    }

    fn write_varint_for_test(bytes: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            bytes.push(((value as u8) & 0x7f) | 0x80);
            value >>= 7;
        }
        bytes.push((value & 0x7f) as u8);
    }
}
