//! Bolt protocol v4 server -- provides Neo4j driver compatibility.
//!
//! Bolt is a binary protocol over TCP. This implements enough of it for
//! Neo4j drivers (Python, Java, Go, JS) to connect and execute Cypher queries.
//!
//! Protocol flow:
//!   Client -> [magic 0x6060B017] [version negotiation]
//!   Server <- [agreed version]
//!   Client -> HELLO {auth}
//!   Server <- SUCCESS {server info}
//!   Client -> RUN "MATCH ..." {params}
//!   Server <- SUCCESS {fields: [...]}
//!   Client -> PULL {n: -1}
//!   Server <- RECORD [row1]
//!   Server <- RECORD [row2]
//!   Server <- SUCCESS {type: "r"}

use crate::engine::NexusEngine;
use crate::error::ServerResult;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::{Duration, timeout};
use tracing::{error, info, warn};

const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];
const BOLT_VERSION_4_0: u32 = 0x00000004;
const DEFAULT_MAX_CONNECTIONS: usize = 256;
const CONNECTION_TIMEOUT_SECS: u64 = 300;

#[derive(Debug)]
enum BoltMessage {
    Hello { auth: HashMap<String, String> },
    Run { query: String },
    Pull,
    Goodbye,
    Reset,
    Unknown(u8),
}

#[derive(Debug)]
#[allow(dead_code)]
enum BoltResponse {
    Success(Vec<u8>),
    Record(Vec<u8>),
    Failure(Vec<u8>),
}

pub struct BoltServer {
    engine: Arc<NexusEngine>,
    bind_addr: String,
    max_connections: usize,
    auth_token: Option<String>,
}

impl BoltServer {
    pub fn new(engine: Arc<NexusEngine>, bind_addr: String) -> Self {
        Self {
            engine,
            bind_addr,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            auth_token: None,
        }
    }

    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    pub async fn run(&self) -> ServerResult<()> {
        let listener = TcpListener::bind(&self.bind_addr).await?;
        let semaphore = Arc::new(Semaphore::new(self.max_connections));
        info!(
            "Bolt server listening on {} (max {} connections)",
            self.bind_addr, self.max_connections
        );

        loop {
            let (stream, addr) = listener.accept().await?;
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            info!(
                "Bolt connection from {} ({} slots remaining)",
                addr,
                semaphore.available_permits()
            );
            let engine = self.engine.clone();
            let auth_token = self.auth_token.clone();
            tokio::spawn(async move {
                let result = timeout(
                    Duration::from_secs(CONNECTION_TIMEOUT_SECS),
                    handle_bolt_connection(stream, engine, auth_token),
                )
                .await;

                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!("Bolt connection error from {}: {}", addr, e),
                    Err(_) => warn!(
                        "Bolt connection from {} timed out after {}s",
                        addr, CONNECTION_TIMEOUT_SECS
                    ),
                }
                drop(permit);
            });
        }
    }
}

async fn handle_bolt_connection(
    stream: TcpStream,
    engine: Arc<NexusEngine>,
    auth_token: Option<String>,
) -> ServerResult<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);

    // Handshake: read magic bytes + version negotiation
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).await?;
    if magic != BOLT_MAGIC {
        error!("invalid Bolt magic: {:?}", magic);
        return Ok(());
    }

    // Read 4 version proposals (16 bytes)
    let mut versions = [0u8; 16];
    reader.read_exact(&mut versions).await?;

    // Respond with version 4.0
    writer.write_all(&BOLT_VERSION_4_0.to_be_bytes()).await?;
    writer.flush().await?;

    let mut pending_result: Option<nexus_cypher::executor::QueryResult> = None;

    loop {
        let msg = read_bolt_message(&mut reader).await?;

        match msg {
            BoltMessage::Hello { ref auth } => {
                let principal = auth.get("principal").map(|s| s.as_str()).unwrap_or("");
                if !bolt_auth_allowed(auth, auth_token.as_deref()) {
                    warn!("Bolt auth rejected: principal={}", principal);
                    let resp = pack_failure("unauthorized");
                    write_bolt_response(&mut writer, &resp).await?;
                    break;
                }
                info!("Bolt auth: principal={}", principal);
                let resp = pack_success_map(&[
                    ("server", "Domyn Nexus/0.1.0"),
                    ("connection_id", "nexus-1"),
                ]);
                write_bolt_response(&mut writer, &resp).await?;
            }
            BoltMessage::Run { query } => match engine.execute_cypher(&query) {
                Ok(result) => {
                    let fields_str = result.columns.join(",");
                    let resp = pack_success_map(&[("fields", &fields_str)]);
                    write_bolt_response(&mut writer, &resp).await?;
                    pending_result = Some(result);
                }
                Err(e) => {
                    let resp = pack_failure(&format!("{e}"));
                    write_bolt_response(&mut writer, &resp).await?;
                }
            },
            BoltMessage::Pull => {
                if let Some(ref result) = pending_result {
                    for row in &result.rows {
                        let record = pack_record(row);
                        write_bolt_response(&mut writer, &record).await?;
                    }
                }
                let resp = pack_success_map(&[("type", "r")]);
                write_bolt_response(&mut writer, &resp).await?;
                pending_result = None;
            }
            BoltMessage::Reset => {
                pending_result = None;
                let resp = pack_success_map(&[]);
                write_bolt_response(&mut writer, &resp).await?;
            }
            BoltMessage::Goodbye => {
                info!("Bolt client disconnected (GOODBYE)");
                break;
            }
            BoltMessage::Unknown(tag) => {
                warn!("unknown Bolt message tag: 0x{:02X}", tag);
                let resp = pack_failure("unknown message");
                write_bolt_response(&mut writer, &resp).await?;
            }
        }
    }

    Ok(())
}

async fn read_bolt_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> ServerResult<BoltMessage> {
    // Bolt chunks: [u16 size][data...][u16 0x0000]
    let mut chunk_size_buf = [0u8; 2];
    reader.read_exact(&mut chunk_size_buf).await?;
    let chunk_size = u16::from_be_bytes(chunk_size_buf) as usize;

    if chunk_size == 0 {
        return Ok(BoltMessage::Unknown(0));
    }

    let mut data = vec![0u8; chunk_size];
    reader.read_exact(&mut data).await?;

    // Read the trailing 0x0000 terminator
    let mut terminator = [0u8; 2];
    reader.read_exact(&mut terminator).await?;

    if data.is_empty() {
        return Ok(BoltMessage::Unknown(0));
    }

    // Simplified PackStream parsing: first byte after struct marker is the tag
    let tag = find_struct_tag(&data);

    match tag {
        0x01 => {
            let auth = extract_hello_auth(&data);
            Ok(BoltMessage::Hello { auth })
        }
        0x10 => {
            let query = extract_run_query(&data);
            Ok(BoltMessage::Run { query })
        }
        0x3F => Ok(BoltMessage::Pull),
        0x02 => Ok(BoltMessage::Goodbye),
        0x0F => Ok(BoltMessage::Reset),
        _ => Ok(BoltMessage::Unknown(tag)),
    }
}

fn find_struct_tag(data: &[u8]) -> u8 {
    // PackStream struct: 0xB? (tiny struct) followed by tag byte
    for (i, &b) in data.iter().enumerate() {
        if (b & 0xF0) == 0xB0 {
            if i + 1 < data.len() {
                return data[i + 1];
            }
        }
    }
    data.first().copied().unwrap_or(0)
}

fn extract_run_query(data: &[u8]) -> String {
    // After the struct header (B2 10), find the first PackStream string.
    // PackStream tiny string: 0x80-0x8F (len in low nibble)
    // PackStream string8: 0xD0 [u8 len] [bytes]
    // PackStream string16: 0xD1 [u16 len] [bytes]
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b >= 0x80 && b <= 0x8F {
            let len = (b & 0x0F) as usize;
            if i + 1 + len <= data.len() {
                return String::from_utf8_lossy(&data[i + 1..i + 1 + len]).to_string();
            }
        } else if b == 0xD0 && i + 1 < data.len() {
            let len = data[i + 1] as usize;
            if i + 2 + len <= data.len() {
                return String::from_utf8_lossy(&data[i + 2..i + 2 + len]).to_string();
            }
        } else if b == 0xD1 && i + 2 < data.len() {
            let len = u16::from_be_bytes([data[i + 1], data[i + 2]]) as usize;
            if i + 3 + len <= data.len() {
                return String::from_utf8_lossy(&data[i + 3..i + 3 + len]).to_string();
            }
        }
        i += 1;
    }
    String::new()
}

/// Best-effort extraction of the auth map from a HELLO message.
///
/// HELLO payload is a struct with one field: a map of string->string.
/// We skip the struct header (B1 01) and parse the map that follows.
fn extract_hello_auth(data: &[u8]) -> HashMap<String, String> {
    let mut auth = HashMap::new();

    // Find the struct header (B1 01) and skip past it to the map.
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == 0xB1 && data[i + 1] == 0x01 {
            i += 2;
            break;
        }
        i += 1;
    }

    if i >= data.len() {
        return auth;
    }

    let map_byte = data[i];
    let map_len = if (map_byte & 0xF0) == 0xA0 {
        i += 1;
        (map_byte & 0x0F) as usize
    } else {
        return auth;
    };

    for _ in 0..map_len {
        if let Some((key, adv)) = read_packstream_string(data, i) {
            i += adv;
            if let Some((val, adv2)) = read_packstream_string(data, i) {
                i += adv2;
                auth.insert(key, val);
            } else {
                break;
            }
        } else {
            break;
        }
    }

    auth
}

fn bolt_auth_allowed(auth: &HashMap<String, String>, expected_token: Option<&str>) -> bool {
    let Some(expected) = expected_token else {
        return true;
    };

    auth.get("credentials")
        .is_some_and(|token| token == expected)
        || auth.get("password").is_some_and(|token| token == expected)
}

/// Read a PackStream string starting at `pos`, returning (string, bytes_consumed).
fn read_packstream_string(data: &[u8], pos: usize) -> Option<(String, usize)> {
    if pos >= data.len() {
        return None;
    }
    let b = data[pos];
    if b >= 0x80 && b <= 0x8F {
        let len = (b & 0x0F) as usize;
        if pos + 1 + len <= data.len() {
            let s = String::from_utf8_lossy(&data[pos + 1..pos + 1 + len]).to_string();
            return Some((s, 1 + len));
        }
    } else if b == 0xD0 && pos + 1 < data.len() {
        let len = data[pos + 1] as usize;
        if pos + 2 + len <= data.len() {
            let s = String::from_utf8_lossy(&data[pos + 2..pos + 2 + len]).to_string();
            return Some((s, 2 + len));
        }
    } else if b == 0xD1 && pos + 2 < data.len() {
        let len = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
        if pos + 3 + len <= data.len() {
            let s = String::from_utf8_lossy(&data[pos + 3..pos + 3 + len]).to_string();
            return Some((s, 3 + len));
        }
    }
    None
}

fn pack_success_map(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = Vec::new();
    let n = fields.len();
    // Struct marker: SUCCESS = 0x70
    buf.push(0xB1);
    buf.push(0x70);
    // Tiny map
    buf.push(0xA0 | (n as u8 & 0x0F));
    for (k, v) in fields {
        pack_string(&mut buf, k);
        pack_string(&mut buf, v);
    }
    buf
}

fn pack_failure(message: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    // FAILURE = 0x7F
    buf.push(0xB1);
    buf.push(0x7F);
    buf.push(0xA1); // map with 1 entry
    pack_string(&mut buf, "message");
    pack_string(&mut buf, message);
    buf
}

fn pack_record(values: &[nexus_core::types::Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    // RECORD = 0x71
    buf.push(0xB1);
    buf.push(0x71);
    // Pack as a list
    let n = values.len();
    if n < 16 {
        buf.push(0x90 | (n as u8));
    } else {
        buf.push(0xD4);
        buf.extend_from_slice(&(n as u8).to_be_bytes());
    }
    for v in values {
        pack_value(&mut buf, v);
    }
    buf
}

fn pack_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len < 16 {
        buf.push(0x80 | (len as u8));
    } else if len < 256 {
        buf.push(0xD0);
        buf.push(len as u8);
    } else {
        buf.push(0xD1);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    }
    buf.extend_from_slice(bytes);
}

fn pack_value(buf: &mut Vec<u8>, value: &nexus_core::types::Value) {
    use nexus_core::types::Value;
    match value {
        Value::Null => buf.push(0xC0),
        Value::Bool(true) => buf.push(0xC3),
        Value::Bool(false) => buf.push(0xC2),
        Value::Int64(v) => {
            if *v >= -16 && *v <= 127 {
                buf.push(*v as u8);
            } else {
                buf.push(0xCB);
                buf.extend_from_slice(&v.to_be_bytes());
            }
        }
        Value::Float64(v) => {
            buf.push(0xC1);
            buf.extend_from_slice(&v.to_be_bytes());
        }
        Value::String(s) => pack_string(buf, s),
        Value::Bytes(b) => {
            buf.push(0xCC);
            buf.push(b.len() as u8);
            buf.extend_from_slice(b);
        }
        Value::List(items) => {
            if items.len() < 16 {
                buf.push(0x90 | (items.len() as u8));
            } else {
                buf.push(0xD4);
                buf.push(items.len() as u8);
            }
            for item in items {
                pack_value(buf, item);
            }
        }
        Value::Map(entries) => {
            if entries.len() < 16 {
                buf.push(0xA0 | (entries.len() as u8));
            } else {
                buf.push(0xD8);
                buf.push(entries.len() as u8);
            }
            for (k, v) in entries {
                pack_string(buf, k);
                pack_value(buf, v);
            }
        }
    }
}

async fn write_bolt_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    data: &[u8],
) -> ServerResult<()> {
    let len = data.len() as u16;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(data).await?;
    writer.write_all(&[0x00, 0x00]).await?; // chunk terminator
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_success() {
        let data = pack_success_map(&[("server", "Domyn Nexus")]);
        assert!(!data.is_empty());
        assert_eq!(data[0], 0xB1);
        assert_eq!(data[1], 0x70); // SUCCESS
    }

    #[test]
    fn pack_record_values() {
        use nexus_core::types::Value;
        let row = vec![
            Value::Int64(42),
            Value::String("hello".into()),
            Value::Bool(true),
        ];
        let data = pack_record(&row);
        assert_eq!(data[0], 0xB1);
        assert_eq!(data[1], 0x71); // RECORD
    }

    #[test]
    fn extract_query_from_packstream() {
        let mut data = Vec::new();
        data.push(0xB2); // struct with 2 fields
        data.push(0x10); // RUN tag
        pack_string(&mut data, "MATCH (n) RETURN n");
        data.push(0xA0); // empty params map

        let query = extract_run_query(&data);
        assert_eq!(query, "MATCH (n) RETURN n");
    }

    #[test]
    fn find_tag_in_struct() {
        let data = vec![0xB1, 0x01]; // HELLO
        assert_eq!(find_struct_tag(&data), 0x01);

        let data = vec![0xB2, 0x10]; // RUN
        assert_eq!(find_struct_tag(&data), 0x10);
    }

    #[test]
    fn extract_hello_auth_fields() {
        // Build a HELLO payload: B1 01 { "principal": "neo4j", "scheme": "basic" }
        let mut data = Vec::new();
        data.push(0xB1); // struct with 1 field
        data.push(0x01); // HELLO tag
        data.push(0xA2); // tiny map with 2 entries
        pack_string(&mut data, "principal");
        pack_string(&mut data, "neo4j");
        pack_string(&mut data, "scheme");
        pack_string(&mut data, "basic");

        let auth = extract_hello_auth(&data);
        assert_eq!(auth.get("principal").unwrap(), "neo4j");
        assert_eq!(auth.get("scheme").unwrap(), "basic");
    }

    #[test]
    fn extract_hello_auth_empty() {
        // HELLO with empty auth map
        let data = vec![0xB1, 0x01, 0xA0];
        let auth = extract_hello_auth(&data);
        assert!(auth.is_empty());
    }

    #[test]
    fn bolt_auth_verifies_credentials_when_configured() {
        let mut auth = HashMap::new();
        auth.insert("principal".to_string(), "neo4j".to_string());
        auth.insert("credentials".to_string(), "secret".to_string());

        assert!(bolt_auth_allowed(&auth, Some("secret")));
        assert!(!bolt_auth_allowed(&auth, Some("wrong")));
        assert!(bolt_auth_allowed(&HashMap::new(), None));
    }
}
