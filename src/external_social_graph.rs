use heed::byteorder::BigEndian;
use heed::types::{SerdeBincode, Str, U32};
use heed::{Database, Env, EnvFlags, EnvOpenOptions, Error as HeedError, RoTxn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
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
const MUTED_BY_USER_DB: &str = "muted_by_user";

pub struct ExternalSocialGraph {
    source: ExternalSocialGraphSource,
}

enum ExternalSocialGraphSource {
    Lmdb(LmdbExternalSocialGraph),
    Binary(BinaryExternalSocialGraph),
}

struct LmdbExternalSocialGraph {
    env: Env,
    str_to_unique_id: Database<Str, U32<BigEndian>>,
    follow_distance_by_user: Database<U32<BigEndian>, U32<BigEndian>>,
    muted_by_user: Database<U32<BigEndian>, SerdeBincode<Vec<u32>>>,
}

struct BinaryExternalSocialGraph {
    pubkey_to_id: HashMap<String, u32>,
    follow_distance_by_user: HashMap<u32, u32>,
    muted_by_user: HashMap<u32, HashSet<u32>>,
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

        open_read_only_graph_with_retries(path)
    }

    fn from_env(env: Env) -> Result<Self, Box<dyn Error + Send + Sync>> {
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
        let follow_distance_by_user = open_required_database::<U32<BigEndian>, U32<BigEndian>>(
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
        drop(rtxn);

        Ok(Self {
            source: ExternalSocialGraphSource::Lmdb(LmdbExternalSocialGraph {
                env,
                str_to_unique_id,
                follow_distance_by_user,
                muted_by_user,
            }),
        })
    }

    pub fn is_pubkey_in_graph(&self, pubkey: &str) -> Result<bool, Box<dyn Error + Send + Sync>> {
        match &self.source {
            ExternalSocialGraphSource::Lmdb(graph) => {
                let rtxn = graph.env.read_txn()?;
                let Some(pubkey_id) = graph.str_to_unique_id.get(&rtxn, pubkey)? else {
                    return Ok(false);
                };

                Ok(graph
                    .follow_distance_by_user
                    .get(&rtxn, &pubkey_id)?
                    .is_some_and(|distance| distance < UNKNOWN_FOLLOW_DISTANCE))
            }
            ExternalSocialGraphSource::Binary(graph) => Ok(graph
                .pubkey_to_id
                .get(pubkey)
                .and_then(|pubkey_id| graph.follow_distance_by_user.get(pubkey_id))
                .is_some_and(|distance| *distance < UNKNOWN_FOLLOW_DISTANCE)),
        }
    }

    pub fn recipient_has_muted_author(
        &self,
        recipient: &str,
        author: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        match &self.source {
            ExternalSocialGraphSource::Lmdb(graph) => {
                let rtxn = graph.env.read_txn()?;
                let Some(recipient_id) = graph.str_to_unique_id.get(&rtxn, recipient)? else {
                    return Ok(false);
                };
                let Some(author_id) = graph.str_to_unique_id.get(&rtxn, author)? else {
                    return Ok(false);
                };

                Ok(graph
                    .muted_by_user
                    .get(&rtxn, &recipient_id)?
                    .is_some_and(|muted| muted.contains(&author_id)))
            }
            ExternalSocialGraphSource::Binary(graph) => {
                let Some(recipient_id) = graph.pubkey_to_id.get(recipient) else {
                    return Ok(false);
                };
                let Some(author_id) = graph.pubkey_to_id.get(author) else {
                    return Ok(false);
                };

                Ok(graph
                    .muted_by_user
                    .get(recipient_id)
                    .is_some_and(|muted| muted.contains(author_id)))
            }
        }
    }

    fn from_social_graph_url(
        url: &str,
        root_pubkey: &str,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let bytes = fetch_http_snapshot(url)?;
        Self::from_social_graph_binary(root_pubkey, &bytes)
    }

    fn from_social_graph_binary(
        root_pubkey: &str,
        data: &[u8],
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let snapshot = parse_social_graph_binary(root_pubkey, data)?;
        Ok(Self {
            source: ExternalSocialGraphSource::Binary(snapshot),
        })
    }
}

fn open_read_only_graph_with_retries(
    path: &Path,
) -> Result<ExternalSocialGraph, Box<dyn Error + Send + Sync>> {
    for attempt in 0..=OPEN_RETRY_ATTEMPTS {
        match open_read_only_graph(path) {
            Ok(graph) => return Ok(graph),
            Err(error)
                if is_temporary_resource_error(error.as_ref()) && attempt < OPEN_RETRY_ATTEMPTS =>
            {
                thread::sleep(Duration::from_millis(OPEN_RETRY_DELAY_MS));
            }
            Err(error) => return Err(error),
        }
    }

    open_read_only_graph(path)
}

fn open_read_only_graph(path: &Path) -> Result<ExternalSocialGraph, Box<dyn Error + Send + Sync>> {
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
    ExternalSocialGraph::from_env(env)
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
) -> Result<BinaryExternalSocialGraph, Box<dyn Error + Send + Sync>> {
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
        let mut followed = Vec::with_capacity(followed_count);
        for _ in 0..followed_count {
            followed.push(
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

    let root_id = *pubkey_to_id
        .get(root_pubkey)
        .ok_or("social graph binary does not contain the configured root")?;
    let follow_distance_by_user = calculate_follow_distances(root_id, &followed_by_user);

    Ok(BinaryExternalSocialGraph {
        pubkey_to_id,
        follow_distance_by_user,
        muted_by_user,
    })
}

fn calculate_follow_distances(
    root_id: u32,
    followed_by_user: &HashMap<u32, Vec<u32>>,
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

    const TEST_MAP_SIZE: usize = 128 * 1024 * 1024;

    #[test]
    fn reads_follow_distances_and_mute_lists_from_snapshot() {
        let path =
            std::env::temp_dir().join(format!("nns-external-social-graph-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();

        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(TEST_MAP_SIZE)
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

    fn repeated_hex(byte: u8) -> String {
        (0..32).map(|_| format!("{byte:02x}")).collect()
    }

    fn push_id_for_test(bytes: &mut Vec<u8>, pubkey: &str, id: u64) {
        for index in (0..pubkey.len()).step_by(2) {
            bytes.push(u8::from_str_radix(&pubkey[index..index + 2], 16).unwrap());
        }
        write_varint_for_test(bytes, id);
    }

    fn write_varint_for_test(bytes: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            bytes.push(((value as u8) & 0x7f) | 0x80);
            value >>= 7;
        }
        bytes.push((value & 0x7f) as u8);
    }
}
