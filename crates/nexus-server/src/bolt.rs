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

use crate::config::BoltConfig;
use crate::engine::{NexusEngine, native_entrypoint_claims};
use crate::error::ServerResult;
use crate::tls::{ReloadingTlsAcceptor, TlsConfig};
use nexus_core::types::Value;
use nexus_cypher::ast::Statement;
use nexus_cypher::error::CypherError;
use nexus_cypher::executor::QueryResult;
use nexus_cypher::parser::Parser;
use nexus_parser::{NexusParser, ParsedStatement};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::{Duration, timeout};
use tracing::{error, info, warn};

const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];
const BOLT_VERSION_4_0: u32 = 0x00000004;
const DEFAULT_MAX_CONNECTIONS: usize = 256;
const CONNECTION_TIMEOUT_SECS: u64 = 300;
const DEFAULT_QUERY_TIMEOUT_SECS: u64 = 30;
const DEFAULT_QUERY_MEMORY_BUDGET_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_QUERY_ROW_LIMIT: usize = 10_000;

#[derive(Debug)]
enum BoltMessage {
    Hello {
        auth: HashMap<String, String>,
    },
    Run {
        query: String,
        params: HashMap<String, Value>,
    },
    Pull {
        n: i64,
    },
    Route,
    Begin,
    Commit,
    Rollback,
    Goodbye,
    Reset,
    Unknown(u8),
}

#[derive(Debug)]
struct PendingResult {
    result: QueryResult,
    cursor: usize,
}

impl PendingResult {
    fn new(result: QueryResult) -> Self {
        Self { result, cursor: 0 }
    }

    fn pull(&mut self, n: i64) -> (Vec<Vec<Value>>, bool) {
        let remaining = self.result.rows.len().saturating_sub(self.cursor);
        let limit = if n < 0 {
            remaining
        } else {
            (n as usize).min(remaining)
        };
        let end = self.cursor + limit;
        let rows = self.result.rows[self.cursor..end].to_vec();
        self.cursor = end;
        let has_more = self.cursor < self.result.rows.len();
        (rows, has_more)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoltTxState {
    AutoCommit,
    Explicit,
}

impl BoltTxState {
    fn begin(&mut self) -> Result<(), &'static str> {
        match self {
            BoltTxState::AutoCommit => {
                *self = BoltTxState::Explicit;
                Ok(())
            }
            BoltTxState::Explicit => Err("nested explicit transactions are not supported"),
        }
    }

    fn finish(&mut self) -> Result<(), &'static str> {
        match self {
            BoltTxState::Explicit => {
                *self = BoltTxState::AutoCommit;
                Ok(())
            }
            BoltTxState::AutoCommit => Err("no explicit transaction is open"),
        }
    }

    fn in_explicit_transaction(self) -> bool {
        matches!(self, BoltTxState::Explicit)
    }
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
    tls: Option<TlsConfig>,
    query_limits: BoltQueryLimits,
}

#[derive(Debug, Clone, Copy)]
struct BoltQueryLimits {
    query_timeout_secs: u64,
    default_query_limit: usize,
    query_memory_budget_bytes: usize,
}

impl Default for BoltQueryLimits {
    fn default() -> Self {
        Self {
            query_timeout_secs: DEFAULT_QUERY_TIMEOUT_SECS,
            default_query_limit: DEFAULT_QUERY_ROW_LIMIT,
            query_memory_budget_bytes: DEFAULT_QUERY_MEMORY_BUDGET_BYTES,
        }
    }
}

impl BoltQueryLimits {
    fn row_budget(self) -> Option<usize> {
        (self.default_query_limit > 0).then_some(self.default_query_limit)
    }

    fn byte_budget(self) -> Option<usize> {
        (self.query_memory_budget_bytes > 0).then_some(self.query_memory_budget_bytes)
    }
}

impl BoltServer {
    pub fn new(engine: Arc<NexusEngine>, bind_addr: String) -> Self {
        Self {
            engine,
            bind_addr,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            auth_token: None,
            tls: None,
            query_limits: BoltQueryLimits::default(),
        }
    }

    pub fn from_config(
        engine: Arc<NexusEngine>,
        config: &BoltConfig,
        auth_token: Option<String>,
    ) -> Option<Self> {
        if !config.enabled {
            return None;
        }

        let mut server = Self::new(engine, config.bind_addr.clone())
            .with_max_connections(config.max_connections)
            .with_query_limits(
                config.query_timeout_secs,
                config.default_query_limit,
                config.query_memory_budget_bytes,
            );
        if let Some(token) = auth_token {
            server = server.with_auth_token(token);
        }
        if let Some(tls) = config.tls.clone() {
            server = server.with_tls(tls);
        }
        Some(server)
    }

    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    pub fn with_tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn with_query_limits(
        mut self,
        query_timeout_secs: u64,
        default_query_limit: usize,
        query_memory_budget_bytes: usize,
    ) -> Self {
        self.query_limits = BoltQueryLimits {
            query_timeout_secs,
            default_query_limit,
            query_memory_budget_bytes,
        };
        self
    }

    pub async fn run(&self) -> ServerResult<()> {
        let listener = TcpListener::bind(&self.bind_addr).await?;
        let semaphore = Arc::new(Semaphore::new(self.max_connections));
        let tls_acceptor = self
            .tls
            .as_ref()
            .map(|tls| ReloadingTlsAcceptor::new(tls.clone()))
            .transpose()?;
        info!(
            "Bolt server listening on {} (max {} connections, tls={})",
            self.bind_addr,
            self.max_connections,
            tls_acceptor.is_some()
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
            let tls_acceptor = tls_acceptor.clone();
            let advertised_addr = self.bind_addr.clone();
            let query_limits = self.query_limits;
            tokio::spawn(async move {
                let result = if let Some(acceptor) = tls_acceptor {
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            timeout(
                                Duration::from_secs(CONNECTION_TIMEOUT_SECS),
                                handle_bolt_connection(
                                    tls_stream,
                                    engine,
                                    auth_token,
                                    advertised_addr.clone(),
                                    query_limits,
                                ),
                            )
                            .await
                        }
                        Err(err) => {
                            warn!("Bolt TLS handshake failed from {}: {}", addr, err);
                            drop(permit);
                            return;
                        }
                    }
                } else {
                    timeout(
                        Duration::from_secs(CONNECTION_TIMEOUT_SECS),
                        handle_bolt_connection(
                            stream,
                            engine,
                            auth_token,
                            advertised_addr,
                            query_limits,
                        ),
                    )
                    .await
                };

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
    stream: impl AsyncRead + AsyncWrite + Unpin,
    engine: Arc<NexusEngine>,
    auth_token: Option<String>,
    advertised_addr: String,
    query_limits: BoltQueryLimits,
) -> ServerResult<()> {
    let (read_half, write_half) = tokio::io::split(stream);
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

    let mut pending_result: Option<PendingResult> = None;
    let mut tx_state = BoltTxState::AutoCommit;
    let mut failed = false;

    loop {
        let msg = read_bolt_message(&mut reader).await?;
        if failed && !bolt_message_allowed_in_failed_state(&msg) {
            let resp = pack_ignored();
            write_bolt_response(&mut writer, &resp).await?;
            continue;
        }

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
            BoltMessage::Run { query, params } => {
                if let Some((code, message)) =
                    explicit_write_transaction_rejection(&tx_state, &query)
                {
                    let resp = pack_failure_with_code(code, message);
                    write_bolt_response(&mut writer, &resp).await?;
                    failed = true;
                    continue;
                }

                match execute_bolt_query(engine.clone(), query, params, query_limits).await {
                    Ok(result) => {
                        let resp = pack_success_fields(&result.columns);
                        write_bolt_response(&mut writer, &resp).await?;
                        pending_result = Some(PendingResult::new(result));
                    }
                    Err(e) => {
                        let resp = pack_failure(&format!("{e}"));
                        write_bolt_response(&mut writer, &resp).await?;
                        failed = true;
                    }
                }
            }
            BoltMessage::Pull { n } => {
                let has_more = if let Some(ref mut result) = pending_result {
                    let (rows, has_more) = result.pull(n);
                    for row in &rows {
                        let record = pack_record(row);
                        write_bolt_response(&mut writer, &record).await?;
                    }
                    has_more
                } else {
                    false
                };

                let resp = pack_success_metadata(&[
                    ("type", Value::String("r".into())),
                    ("has_more", Value::Bool(has_more)),
                ]);
                write_bolt_response(&mut writer, &resp).await?;
                if !has_more {
                    pending_result = None;
                }
            }
            BoltMessage::Reset => {
                pending_result = None;
                tx_state = BoltTxState::AutoCommit;
                failed = false;
                let resp = pack_success_map(&[]);
                write_bolt_response(&mut writer, &resp).await?;
            }
            BoltMessage::Begin => {
                pending_result = None;
                let resp = match tx_state.begin() {
                    Ok(()) => pack_success_map(&[]),
                    Err(msg) => {
                        failed = true;
                        pack_failure_with_code("Neo.ClientError.Transaction.TransactionError", msg)
                    }
                };
                write_bolt_response(&mut writer, &resp).await?;
            }
            BoltMessage::Commit | BoltMessage::Rollback => {
                pending_result = None;
                let resp = match tx_state.finish() {
                    Ok(()) => pack_success_map(&[]),
                    Err(msg) => {
                        failed = true;
                        pack_failure_with_code("Neo.ClientError.Transaction.TransactionError", msg)
                    }
                };
                write_bolt_response(&mut writer, &resp).await?;
            }
            BoltMessage::Route => {
                let resp = pack_success_metadata(&[("rt", route_table_value(&advertised_addr))]);
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
                failed = true;
            }
        }
    }

    Ok(())
}

async fn execute_bolt_query(
    engine: Arc<NexusEngine>,
    query: String,
    params: HashMap<String, Value>,
    limits: BoltQueryLimits,
) -> Result<QueryResult, CypherError> {
    let cancellation = Arc::new(AtomicBool::new(false));
    let query_cancellation = cancellation.clone();
    let row_budget = limits.row_budget();
    let byte_budget = limits.byte_budget();
    let worker = tokio::task::spawn_blocking(move || {
        engine.execute_cypher_with_params_cancellation_and_limits(
            &query,
            params,
            Some(query_cancellation),
            row_budget,
            byte_budget,
        )
    });

    if limits.query_timeout_secs == 0 {
        return worker
            .await
            .map_err(|err| CypherError::Execution(format!("bolt query task panicked: {err}")))?;
    }

    match timeout(Duration::from_secs(limits.query_timeout_secs), worker).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => Err(CypherError::Execution(format!(
            "bolt query task panicked: {err}"
        ))),
        Err(_) => {
            cancellation.store(true, Ordering::Relaxed);
            Err(CypherError::Execution(format!(
                "query timed out after {}s",
                limits.query_timeout_secs
            )))
        }
    }
}

async fn read_bolt_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> ServerResult<BoltMessage> {
    // Bolt chunks: repeated [u16 size][data...] terminated by [u16 0x0000].
    let mut data = Vec::new();
    loop {
        let mut chunk_size_buf = [0u8; 2];
        reader.read_exact(&mut chunk_size_buf).await?;
        let chunk_size = u16::from_be_bytes(chunk_size_buf) as usize;
        if chunk_size == 0 {
            break;
        }

        let start = data.len();
        data.resize(start + chunk_size, 0);
        reader.read_exact(&mut data[start..]).await?;
    }

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
            let (query, params) = extract_run_query_and_params(&data);
            Ok(BoltMessage::Run { query, params })
        }
        0x11 => Ok(BoltMessage::Begin),
        0x12 => Ok(BoltMessage::Commit),
        0x13 => Ok(BoltMessage::Rollback),
        0x3F => Ok(BoltMessage::Pull {
            n: extract_pull_n(&data),
        }),
        0x66 => Ok(BoltMessage::Route),
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

fn extract_run_query_and_params(data: &[u8]) -> (String, HashMap<String, Value>) {
    let mut i = 0;
    while i + 1 < data.len() {
        if (data[i] & 0xF0) == 0xB0 && data[i + 1] == 0x10 {
            i += 2;
            break;
        }
        i += 1;
    }

    let Some((query, consumed)) = read_packstream_string(data, i) else {
        return (String::new(), HashMap::new());
    };
    i += consumed;

    let params = match read_packstream_value(data, i) {
        Some((Value::Map(entries), _)) => entries.into_iter().collect(),
        _ => HashMap::new(),
    };

    (query, params)
}

fn extract_pull_n(data: &[u8]) -> i64 {
    let mut i = 0;
    while i + 1 < data.len() {
        if (data[i] & 0xF0) == 0xB0 && data[i + 1] == 0x3F {
            i += 2;
            break;
        }
        i += 1;
    }

    match read_packstream_value(data, i) {
        Some((Value::Map(entries), _)) => entries
            .into_iter()
            .find_map(|(key, value)| match (key.as_str(), value) {
                ("n", Value::Int64(n)) => Some(n),
                _ => None,
            })
            .unwrap_or(-1),
        _ => -1,
    }
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

fn read_packstream_value(data: &[u8], pos: usize) -> Option<(Value, usize)> {
    if pos >= data.len() {
        return None;
    }

    let b = data[pos];
    match b {
        0xC0 => Some((Value::Null, 1)),
        0xC2 => Some((Value::Bool(false), 1)),
        0xC3 => Some((Value::Bool(true), 1)),
        0xC1 if pos + 8 < data.len() => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&data[pos + 1..pos + 9]);
            Some((Value::Float64(f64::from_be_bytes(bytes)), 9))
        }
        0xC8 if pos + 1 < data.len() => Some((Value::Int64(data[pos + 1] as i8 as i64), 2)),
        0xC9 if pos + 2 < data.len() => Some((
            Value::Int64(i16::from_be_bytes([data[pos + 1], data[pos + 2]]) as i64),
            3,
        )),
        0xCA if pos + 4 < data.len() => Some((
            Value::Int64(i32::from_be_bytes([
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
                data[pos + 4],
            ]) as i64),
            5,
        )),
        0xCB if pos + 8 < data.len() => Some((
            Value::Int64(i64::from_be_bytes([
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
                data[pos + 4],
                data[pos + 5],
                data[pos + 6],
                data[pos + 7],
                data[pos + 8],
            ])),
            9,
        )),
        0x00..=0x7F => Some((Value::Int64(b as i64), 1)),
        0xF0..=0xFF => Some((Value::Int64(b as i8 as i64), 1)),
        0x80..=0x8F | 0xD0 | 0xD1 => {
            read_packstream_string(data, pos).map(|(s, consumed)| (Value::String(s), consumed))
        }
        0x90..=0x9F => read_packstream_list(data, pos + 1, (b & 0x0F) as usize)
            .map(|(items, consumed)| (Value::List(items), consumed + 1)),
        0xD4 if pos + 1 < data.len() => read_packstream_list(data, pos + 2, data[pos + 1] as usize)
            .map(|(items, consumed)| (Value::List(items), consumed + 2)),
        0xD5 if pos + 2 < data.len() => {
            let len = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
            read_packstream_list(data, pos + 3, len)
                .map(|(items, consumed)| (Value::List(items), consumed + 3))
        }
        0xA0..=0xAF => read_packstream_map(data, pos + 1, (b & 0x0F) as usize)
            .map(|(items, consumed)| (Value::Map(items), consumed + 1)),
        0xD8 if pos + 1 < data.len() => read_packstream_map(data, pos + 2, data[pos + 1] as usize)
            .map(|(items, consumed)| (Value::Map(items), consumed + 2)),
        0xD9 if pos + 2 < data.len() => {
            let len = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
            read_packstream_map(data, pos + 3, len)
                .map(|(items, consumed)| (Value::Map(items), consumed + 3))
        }
        _ => None,
    }
}

fn read_packstream_list(data: &[u8], mut pos: usize, len: usize) -> Option<(Vec<Value>, usize)> {
    let start = pos;
    let mut items = Vec::with_capacity(len);
    for _ in 0..len {
        let (value, consumed) = read_packstream_value(data, pos)?;
        pos += consumed;
        items.push(value);
    }
    Some((items, pos - start))
}

fn read_packstream_map(
    data: &[u8],
    mut pos: usize,
    len: usize,
) -> Option<(Vec<(String, Value)>, usize)> {
    let start = pos;
    let mut entries = Vec::with_capacity(len);
    for _ in 0..len {
        let (key, key_consumed) = read_packstream_string(data, pos)?;
        pos += key_consumed;
        let (value, value_consumed) = read_packstream_value(data, pos)?;
        pos += value_consumed;
        entries.push((key, value));
    }
    Some((entries, pos - start))
}

fn bolt_query_is_write(query: &str) -> bool {
    match Parser::parse(query) {
        Ok(Statement::Write(_)) => return true,
        Ok(Statement::Read(_)) => return false,
        Err(_) if native_entrypoint_claims(query) => return false,
        Err(_) => {}
    }

    matches!(
        NexusParser::parse_statement_kyu(query),
        Ok(ParsedStatement::Write(_))
    )
}

fn explicit_write_transaction_rejection(
    tx_state: &BoltTxState,
    query: &str,
) -> Option<(&'static str, &'static str)> {
    if tx_state.in_explicit_transaction() && bolt_query_is_write(query) {
        return Some((
            "Neo.ClientError.Transaction.Unsupported",
            "explicit write transactions are not supported yet; run writes in auto-commit mode",
        ));
    }
    None
}

fn bolt_message_allowed_in_failed_state(msg: &BoltMessage) -> bool {
    matches!(msg, BoltMessage::Reset | BoltMessage::Goodbye)
}

fn route_table_value(address: &str) -> Value {
    fn server(role: &str, address: &str) -> Value {
        Value::Map(vec![
            ("role".into(), Value::String(role.into())),
            (
                "addresses".into(),
                Value::List(vec![Value::String(address.into())]),
            ),
        ])
    }

    Value::Map(vec![
        ("ttl".into(), Value::Int64(300)),
        (
            "servers".into(),
            Value::List(vec![
                server("ROUTE", address),
                server("READ", address),
                server("WRITE", address),
            ]),
        ),
    ])
}

fn pack_success_metadata(fields: &[(&str, Value)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(0xB1);
    buf.push(0x70);
    pack_value(
        &mut buf,
        &Value::Map(
            fields
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        ),
    );
    buf
}

fn pack_success_map(fields: &[(&str, &str)]) -> Vec<u8> {
    let metadata: Vec<_> = fields
        .iter()
        .map(|(key, value)| (*key, Value::String((*value).into())))
        .collect();
    pack_success_metadata(&metadata)
}

fn pack_success_fields(fields: &[String]) -> Vec<u8> {
    pack_success_metadata(&[(
        "fields",
        Value::List(fields.iter().cloned().map(Value::String).collect()),
    )])
}

fn pack_failure(message: &str) -> Vec<u8> {
    pack_failure_with_code("Neo.ClientError.Statement.SyntaxError", message)
}

fn pack_failure_with_code(code: &str, message: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    // FAILURE = 0x7F
    buf.push(0xB1);
    buf.push(0x7F);
    buf.push(0xA2); // map with 2 entries
    pack_string(&mut buf, "code");
    pack_string(&mut buf, code);
    pack_string(&mut buf, "message");
    pack_string(&mut buf, message);
    buf
}

fn pack_ignored() -> Vec<u8> {
    vec![0xB1, 0x7E, 0xA0]
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
    use crate::engine::EngineBuilder;
    use nexus_core::properties::PropertyType;

    fn bolt_test_engine(num_vertices: usize) -> Arc<NexusEngine> {
        let mut builder = EngineBuilder::new(num_vertices + 1, 1);
        builder.register_vertex_property("name", PropertyType::String, true, false);
        for i in 0..num_vertices {
            let vertex = builder.add_vertex("Entity");
            builder.set_vertex_property(vertex, "name", Value::String(format!("v{i}")));
        }
        Arc::new(builder.build())
    }

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

        let (query, params) = extract_run_query_and_params(&data);
        assert_eq!(query, "MATCH (n) RETURN n");
        assert!(params.is_empty());
    }

    #[test]
    fn extract_run_params_from_packstream() {
        let mut data = Vec::new();
        data.push(0xB2); // struct with 2 fields
        data.push(0x10); // RUN tag
        pack_string(&mut data, "MATCH (n) WHERE n.name = $name RETURN n.age");
        data.push(0xA4); // params map
        pack_string(&mut data, "name");
        pack_value(&mut data, &Value::String("Ada".into()));
        pack_string(&mut data, "age");
        pack_value(&mut data, &Value::Int64(42));
        pack_string(&mut data, "active");
        pack_value(&mut data, &Value::Bool(true));
        pack_string(&mut data, "tags");
        pack_value(
            &mut data,
            &Value::List(vec![
                Value::String("math".into()),
                Value::String("logic".into()),
            ]),
        );

        let (query, params) = extract_run_query_and_params(&data);
        assert_eq!(query, "MATCH (n) WHERE n.name = $name RETURN n.age");
        assert_eq!(params.get("name"), Some(&Value::String("Ada".into())));
        assert_eq!(params.get("age"), Some(&Value::Int64(42)));
        assert_eq!(params.get("active"), Some(&Value::Bool(true)));
        assert_eq!(
            params.get("tags"),
            Some(&Value::List(vec![
                Value::String("math".into()),
                Value::String("logic".into())
            ]))
        );
    }

    #[test]
    fn extract_pull_n_from_packstream() {
        let mut data = Vec::new();
        data.push(0xB1); // struct with metadata field
        data.push(0x3F); // PULL tag
        data.push(0xA1); // metadata map
        pack_string(&mut data, "n");
        pack_value(&mut data, &Value::Int64(2));

        assert_eq!(extract_pull_n(&data), 2);
        assert_eq!(extract_pull_n(&[0xB1, 0x3F, 0xA0]), -1);
    }

    #[test]
    fn pending_result_pulls_in_batches() {
        let result = QueryResult {
            columns: vec!["n".into()],
            rows: vec![
                vec![Value::Int64(1)],
                vec![Value::Int64(2)],
                vec![Value::Int64(3)],
            ],
        };
        let mut pending = PendingResult::new(result);

        let (rows, has_more) = pending.pull(2);
        assert_eq!(rows.len(), 2);
        assert!(has_more);

        let (rows, has_more) = pending.pull(2);
        assert_eq!(rows, vec![vec![Value::Int64(3)]]);
        assert!(!has_more);
    }

    #[test]
    fn pending_result_negative_pull_reads_all_remaining() {
        let result = QueryResult {
            columns: vec!["n".into()],
            rows: vec![vec![Value::Int64(1)], vec![Value::Int64(2)]],
        };
        let mut pending = PendingResult::new(result);

        let (rows, has_more) = pending.pull(-1);
        assert_eq!(rows.len(), 2);
        assert!(!has_more);
    }

    #[test]
    fn route_table_contains_read_write_and_route_roles() {
        let route_table = route_table_value("127.0.0.1:7687");
        let Value::Map(entries) = route_table else {
            panic!("route table should be a map");
        };
        let servers = entries
            .iter()
            .find_map(|(key, value)| (key == "servers").then_some(value))
            .expect("servers entry");
        let Value::List(servers) = servers else {
            panic!("servers should be a list");
        };
        let roles: Vec<_> = servers
            .iter()
            .filter_map(|server| match server {
                Value::Map(entries) => entries.iter().find_map(|(key, value)| {
                    if key == "role" {
                        Some(value.clone())
                    } else {
                        None
                    }
                }),
                _ => None,
            })
            .collect();

        assert!(roles.contains(&Value::String("ROUTE".into())));
        assert!(roles.contains(&Value::String("READ".into())));
        assert!(roles.contains(&Value::String("WRITE".into())));
    }

    #[test]
    fn explicit_tx_state_rejects_nesting_and_missing_finish() {
        let mut state = BoltTxState::AutoCommit;
        assert!(state.finish().is_err());
        assert!(state.begin().is_ok());
        assert!(state.begin().is_err());
        assert!(state.in_explicit_transaction());
        assert!(state.finish().is_ok());
        assert!(!state.in_explicit_transaction());
    }

    #[test]
    fn failed_state_only_allows_reset_or_goodbye() {
        assert!(bolt_message_allowed_in_failed_state(&BoltMessage::Reset));
        assert!(bolt_message_allowed_in_failed_state(&BoltMessage::Goodbye));
        assert!(!bolt_message_allowed_in_failed_state(&BoltMessage::Pull {
            n: -1
        }));
        assert!(!bolt_message_allowed_in_failed_state(&BoltMessage::Run {
            query: "MATCH (n) RETURN n".into(),
            params: HashMap::new(),
        }));
    }

    #[test]
    fn pack_ignored_uses_bolt_ignored_signature() {
        assert_eq!(pack_ignored(), vec![0xB1, 0x7E, 0xA0]);
    }

    #[test]
    fn bolt_query_write_detection_uses_native_and_kyu_paths() {
        assert!(bolt_query_is_write("CREATE (:Person {name: 'Ada'})"));
        assert!(bolt_query_is_write("MATCH (n) SET n.name = 'Ada'"));
        assert!(!bolt_query_is_write("MATCH (n) RETURN n"));
    }

    #[test]
    fn explicit_write_transaction_rejection_is_documented_and_specific() {
        let mut state = BoltTxState::AutoCommit;
        assert!(state.begin().is_ok());

        let Some((code, message)) =
            explicit_write_transaction_rejection(&state, "CREATE (:Person {name: 'Ada'})")
        else {
            panic!("expected explicit write transaction rejection");
        };
        assert_eq!(code, "Neo.ClientError.Transaction.Unsupported");
        assert!(message.contains("auto-commit"));

        assert!(explicit_write_transaction_rejection(&state, "MATCH (n) RETURN n").is_none());
        assert!(
            explicit_write_transaction_rejection(
                &BoltTxState::AutoCommit,
                "CREATE (:Person {name: 'Ada'})"
            )
            .is_none()
        );
    }

    #[test]
    fn pack_success_fields_uses_packstream_list() {
        let data = pack_success_fields(&["name".to_string(), "age".to_string()]);
        assert_eq!(data[0], 0xB1);
        assert_eq!(data[1], 0x70);
        assert!(data.windows(2).any(|window| window == [0x86, b'f']));
        assert!(data.contains(&0x92));
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

    #[test]
    fn bolt_server_configures_query_limits() {
        let server =
            BoltServer::new(bolt_test_engine(0), "127.0.0.1:0".into()).with_query_limits(7, 8, 9);

        assert_eq!(server.query_limits.query_timeout_secs, 7);
        assert_eq!(server.query_limits.default_query_limit, 8);
        assert_eq!(server.query_limits.query_memory_budget_bytes, 9);
    }

    #[test]
    fn bolt_server_from_config_applies_runtime_knobs() {
        let config = BoltConfig {
            bind_addr: "127.0.0.1:17687".into(),
            max_connections: 17,
            query_timeout_secs: 18,
            default_query_limit: 19,
            query_memory_budget_bytes: 20,
            tls: Some(TlsConfig {
                cert_path: "/bolt-cert.pem".into(),
                key_path: "/bolt-key.pem".into(),
                client_ca_path: None,
                require_client_auth: false,
            }),
            ..BoltConfig::default()
        };

        let server =
            BoltServer::from_config(bolt_test_engine(0), &config, Some("bolt-secret".into()))
                .expect("bolt enabled");

        assert_eq!(server.bind_addr, "127.0.0.1:17687");
        assert_eq!(server.max_connections, 17);
        assert_eq!(server.auth_token.as_deref(), Some("bolt-secret"));
        assert!(server.tls.is_some());
        assert_eq!(server.query_limits.query_timeout_secs, 18);
        assert_eq!(server.query_limits.default_query_limit, 19);
        assert_eq!(server.query_limits.query_memory_budget_bytes, 20);
    }

    #[test]
    fn bolt_server_from_config_respects_disabled_bolt() {
        let config = BoltConfig {
            enabled: false,
            ..BoltConfig::default()
        };

        assert!(BoltServer::from_config(bolt_test_engine(0), &config, None).is_none());
    }

    #[tokio::test]
    async fn bolt_run_applies_row_budget() {
        let engine = bolt_test_engine(2);

        let err = execute_bolt_query(
            engine,
            "MATCH (n:Entity) RETURN n.name".into(),
            HashMap::new(),
            BoltQueryLimits {
                query_timeout_secs: 30,
                default_query_limit: 1,
                query_memory_budget_bytes: 0,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("query row budget exceeded"));
    }

    #[tokio::test]
    async fn bolt_run_applies_byte_budget() {
        let engine = bolt_test_engine(0);

        let err = execute_bolt_query(
            engine,
            "RETURN '0123456789abcdef' AS s".into(),
            HashMap::new(),
            BoltQueryLimits {
                query_timeout_secs: 30,
                default_query_limit: 0,
                query_memory_budget_bytes: 8,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("query byte budget exceeded"));
    }
}
