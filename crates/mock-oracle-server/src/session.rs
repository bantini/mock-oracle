//! One client connection: TNS handshake, then a loop of TTC request/response.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use mock_oracle_core::{Column, Database, OraError, Session as DbSession, SqlType, Value};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::auth::{self, Challenge, LoginError};
use crate::oranum;
use crate::tns::{self, invalid};
use crate::ttc::{ReadBuf, WriteBuf};
use crate::Config;

// TTC message types.
const MSG_PROTOCOL: u8 = 1;
const MSG_DATA_TYPES: u8 = 2;
const MSG_FUNCTION: u8 = 3;
const MSG_ERROR: u8 = 4;
const MSG_ROW_HEADER: u8 = 6;
const MSG_ROW_DATA: u8 = 7;
const MSG_PARAMETER: u8 = 8;
const MSG_STATUS: u8 = 9;
const MSG_DESCRIBE_INFO: u8 = 16;
const MSG_PIGGYBACK: u8 = 17;

// TTC function codes.
const FUNC_REEXECUTE: u8 = 4;
const FUNC_FETCH: u8 = 5;
const FUNC_LOGOFF: u8 = 9;
const FUNC_COMMIT: u8 = 14;
const FUNC_ROLLBACK: u8 = 15;
const FUNC_REEXECUTE_AND_FETCH: u8 = 78;
const FUNC_EXECUTE: u8 = 94;
const FUNC_CLOSE_CURSORS: u8 = 105;
const FUNC_AUTH_PHASE_TWO: u8 = 115;
const FUNC_AUTH_PHASE_ONE: u8 = 118;
const FUNC_PING: u8 = 147;

// Execute options.
const EXEC_BIND: u32 = 0x08;
const EXEC_DEFINE: u32 = 0x10;
const EXEC_FETCH: u32 = 0x40;
const EXEC_COMMIT: u32 = 0x100;
/// Re-execute flag 2: commit on success.
const EXEC_COMMIT_REEXECUTE: u32 = 0x1;

/// End-of-call status flag telling the client a transaction is open.
const CALL_STATUS_TXN_IN_PROGRESS: u32 = 0x02;

// Oracle wire data types.
const TYPE_VARCHAR: u8 = 1;
const TYPE_NUMBER: u8 = 2;
const TYPE_BINARY_INTEGER: u8 = 3;
const TYPE_DATE: u8 = 12;
const TYPE_CHAR: u8 = 96;
const TYPE_TIMESTAMP: u8 = 180;
const TYPE_TIMESTAMP_TZ: u8 = 181;
const TYPE_TIMESTAMP_LTZ: u8 = 231;

const CHARSET_UTF8: u16 = 873;
const CHARSET_UTF16: u16 = 2000;
/// TTC field version we speak: 19.1. It decides which optional fields appear in messages.
const TTC_FIELD_VERSION: u8 = 12;
const COMPILE_CAPS_LEN: usize = 53;
const CCAP_FIELD_VERSION: usize = 7;
const RUNTIME_CAPS_LEN: usize = 7;
const RCAP_TTC: usize = 6;
const RCAP_TTC_32K: u8 = 0x04;
/// Reported as 19.0.0.0.0.
const SERVER_VERSION_NO: u32 = 19 << 24;

const ORA_NO_DATA_FOUND: u32 = 1403;

pub struct Session<S> {
    stream: S,
    db: Arc<Database>,
    config: Arc<Config>,
    sdu: usize,
    service_name: String,
    login: LoginState,
    /// The database session, once logged in.
    db_session: Option<DbSession>,
    cursors: HashMap<u32, Cursor>,
    next_cursor_id: u32,
}

enum LoginState {
    None,
    Challenged { user: String, challenge: Challenge },
    LoggedIn,
}

struct Cursor {
    sql: String,
    bind_types: Vec<u8>,
    columns: Vec<Column>,
    pending: VecDeque<Vec<Value>>,
    fetched: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Session<S> {
    pub fn new(stream: S, db: Arc<Database>, config: Arc<Config>) -> Self {
        Self {
            stream,
            db,
            config,
            sdu: 8192,
            service_name: String::new(),
            login: LoginState::None,
            db_session: None,
            cursors: HashMap::new(),
            next_cursor_id: 1,
        }
    }

    pub async fn run(mut self) -> io::Result<()> {
        if !self.handshake().await? {
            return Ok(());
        }
        while let Some(request) = self.read_request().await? {
            let reply = self.dispatch(&request);
            tns::write_data(&mut self.stream, self.sdu, &reply.buf).await?;
        }
        Ok(())
    }

    /// Reads CONNECT and answers ACCEPT (or REFUSE). Returns false if refused.
    async fn handshake(&mut self) -> io::Result<bool> {
        let Some(packet) = tns::read_packet(&mut self.stream, false).await? else {
            return Ok(false);
        };
        if packet.kind != tns::CONNECT {
            return Err(invalid(format!(
                "expected CONNECT, got packet type {}",
                packet.kind
            )));
        }
        let mut connect = tns::parse_connect(&packet)?;
        if connect.data.len() < connect.data_len {
            // Connect data too long for the CONNECT packet follows in a DATA packet.
            let Some(more) = tns::read_packet(&mut self.stream, false).await? else {
                return Ok(false);
            };
            connect.data = more.data().to_vec();
        }
        let descriptor = String::from_utf8_lossy(&connect.data).into_owned();
        tracing::debug!(%descriptor, version = connect.version, "connect");
        if connect.version < tns::MIN_VERSION {
            // ORA-12520-style refusal: the client is too old for large SDU.
            tns::write_packet(
                &mut self.stream,
                false,
                tns::REFUSE,
                &tns::refuse_body(12520),
            )
            .await?;
            return Ok(false);
        }
        self.service_name = tns::descriptor_value(&descriptor, "SERVICE_NAME")
            .or_else(|| tns::descriptor_value(&descriptor, "SID"))
            .unwrap_or_else(|| "FREEPDB1".into());
        let sdu = connect.sdu.clamp(512, 2 * 1024 * 1024);
        let tdu = connect.tdu.clamp(255, 2 * 1024 * 1024);
        self.sdu = sdu as usize;
        tns::write_packet(
            &mut self.stream,
            false,
            tns::ACCEPT,
            &tns::accept_body(sdu, tdu),
        )
        .await?;
        Ok(true)
    }

    /// Reads DATA packets until one carries the end-of-request flag. Returns
    /// `None` when the client disconnects.
    async fn read_request(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut request = Vec::new();
        loop {
            let Some(packet) = tns::read_packet(&mut self.stream, true).await? else {
                return Ok(None);
            };
            match packet.kind {
                tns::DATA => {
                    let flags = packet.data_flags();
                    if flags & tns::DATA_FLAG_EOF != 0 {
                        return Ok(None);
                    }
                    request.extend_from_slice(packet.data());
                    if flags & tns::DATA_FLAG_END_OF_REQUEST != 0 {
                        return Ok(Some(request));
                    }
                }
                tns::MARKER | tns::CONTROL => {
                    tracing::debug!(kind = packet.kind, "ignoring marker/control packet");
                }
                other => return Err(invalid(format!("unexpected packet type {other}"))),
            }
        }
    }

    fn dispatch(&mut self, request: &[u8]) -> WriteBuf {
        let mut out = WriteBuf::new();
        let mut r = ReadBuf::new(request);
        let result = match request.first() {
            Some(&MSG_PROTOCOL) => {
                write_protocol(&mut out);
                Ok(())
            }
            Some(&MSG_DATA_TYPES) => {
                out.u8(MSG_DATA_TYPES);
                out.u16_be(0);
                Ok(())
            }
            Some(&MSG_FUNCTION) | Some(&MSG_PIGGYBACK) => self.function(&mut r, &mut out),
            other => Err(invalid(format!("unexpected TTC message type {other:?}"))),
        };
        if let Err(e) = result {
            // The request was malformed or uses something we do not support; the
            // whole request has been read, so answering with an error keeps the
            // stream in sync.
            tracing::warn!(error = %e, "could not process request");
            out = WriteBuf::new();
            let status = self.call_status();
            write_error(
                &mut out,
                status,
                0,
                0,
                Some(&OraError::new(
                    3115,
                    "unsupported network datatype or representation",
                )),
            );
        }
        out
    }

    fn function(&mut self, r: &mut ReadBuf, out: &mut WriteBuf) -> io::Result<()> {
        loop {
            let msg_type = r.u8()?;
            let func = r.u8()?;
            r.u8()?; // sequence number
            match (msg_type, func) {
                (MSG_PIGGYBACK, FUNC_CLOSE_CURSORS) => {
                    r.u8()?;
                    let n = r.ub4()?;
                    for _ in 0..n {
                        let id = r.ub4()?;
                        self.cursors.remove(&id);
                    }
                }
                (MSG_PIGGYBACK, other) => {
                    return Err(invalid(format!("unsupported piggyback function {other}")))
                }
                (MSG_FUNCTION, func) => return self.call(func, r, out),
                (other, _) => return Err(invalid(format!("unexpected TTC message type {other}"))),
            }
        }
    }

    fn call(&mut self, func: u8, r: &mut ReadBuf, out: &mut WriteBuf) -> io::Result<()> {
        let logged_in = matches!(self.login, LoginState::LoggedIn);
        match func {
            FUNC_AUTH_PHASE_ONE => self.auth_phase_one(r, out),
            FUNC_AUTH_PHASE_TWO => self.auth_phase_two(r, out),
            _ if !logged_in => {
                write_error(out, 0, 0, 0, Some(&OraError::new(1012, "not logged on")));
                Ok(())
            }
            FUNC_EXECUTE => self.execute(r, out),
            FUNC_REEXECUTE | FUNC_REEXECUTE_AND_FETCH => self.reexecute(r, out),
            FUNC_FETCH => {
                let cursor_id = r.ub4()?;
                let array_size = r.ub4()?;
                self.send_rows(cursor_id, array_size, out);
                Ok(())
            }
            FUNC_COMMIT => {
                match self.db().commit() {
                    Ok(()) => write_status(out, self.call_status()),
                    Err(e) => write_error(out, self.call_status(), 0, 0, Some(&e)),
                }
                Ok(())
            }
            FUNC_ROLLBACK | FUNC_LOGOFF => {
                self.db().rollback();
                write_status(out, 0);
                Ok(())
            }
            FUNC_PING => {
                write_status(out, self.call_status());
                Ok(())
            }
            other => Err(invalid(format!("unsupported function {other}"))),
        }
    }

    fn auth_phase_one(&mut self, r: &mut ReadBuf, out: &mut WriteBuf) -> io::Result<()> {
        let (user, _pairs) = read_auth(r)?;
        let (challenge, reply) = Challenge::new(&self.config.password);
        out.u8(MSG_PARAMETER);
        out.ub2(5);
        out.key_value("AUTH_SESSKEY", &reply.sesskey, 0);
        out.key_value("AUTH_VFR_DATA", &reply.vfr_data, auth::VERIFIER_TYPE_12C);
        out.key_value("AUTH_PBKDF2_CSK_SALT", &reply.csk_salt, 0);
        out.key_value("AUTH_PBKDF2_VGEN_COUNT", &reply.vgen_count, 0);
        out.key_value("AUTH_PBKDF2_SDER_COUNT", &reply.sder_count, 0);
        write_status(out, 0);
        self.login = LoginState::Challenged { user, challenge };
        Ok(())
    }

    fn auth_phase_two(&mut self, r: &mut ReadBuf, out: &mut WriteBuf) -> io::Result<()> {
        let (_, pairs) = read_auth(r)?;
        let LoginState::Challenged { user, challenge } =
            std::mem::replace(&mut self.login, LoginState::None)
        else {
            write_error(
                out,
                0,
                0,
                0,
                Some(&OraError::new(
                    1017,
                    "invalid credential or not authorized; logon denied",
                )),
            );
            return Ok(());
        };
        let get = |k: &str| pairs.get(k).map(String::as_str).unwrap_or("");
        let response = match challenge.verify(
            get("AUTH_SESSKEY"),
            get("AUTH_PASSWORD"),
            &self.config.password,
        ) {
            Ok(response) => response,
            Err(LoginError::InvalidCredentials) => {
                tracing::info!(%user, "login refused: wrong password");
                write_error(
                    out,
                    0,
                    0,
                    0,
                    Some(&OraError::new(
                        1017,
                        "invalid credential or not authorized; logon denied",
                    )),
                );
                return Ok(());
            }
            Err(LoginError::Protocol(msg)) => return Err(invalid(msg.into())),
        };
        tracing::info!(%user, "logged in");
        let service = self.service_name.to_uppercase();
        let version = SERVER_VERSION_NO.to_string();
        let params: [(&str, &str); 12] = [
            ("AUTH_SVR_RESPONSE", &response),
            ("AUTH_VERSION_NO", &version),
            ("AUTH_VERSION_STRING", "- Mock"),
            ("AUTH_SESSION_ID", "1"),
            ("AUTH_SERIAL_NUM", "1"),
            ("AUTH_SC_SERVICE_NAME", &service),
            ("AUTH_SC_DB_DOMAIN", ""),
            ("AUTH_SC_DBUNIQUE_NAME", "MOCK"),
            ("AUTH_SC_REAL_DBUNIQUE_NAME", "MOCK"),
            ("AUTH_INSTANCENAME", "MOCK"),
            ("AUTH_DBNAME", &service),
            ("AUTH_MAX_IDEN_LENGTH", "128"),
        ];
        out.u8(MSG_PARAMETER);
        out.ub2(params.len() as u16);
        for (k, v) in params {
            out.key_value(k, v, 0);
        }
        write_status(out, 0);
        self.login = LoginState::LoggedIn;
        let mut session = self.db.session(&user);
        // The client sets its time zone during login so dates round-trip in local time.
        if let Some(stmt) = pairs.get("AUTH_ALTER_SESSION") {
            let stmt = stmt.trim_end_matches('\0');
            if let Err(e) = session.execute(stmt, &[]) {
                tracing::warn!(%stmt, error = %e, "ignoring login ALTER SESSION");
            }
        }
        self.db_session = Some(session);
        Ok(())
    }

    fn execute(&mut self, r: &mut ReadBuf, out: &mut WriteBuf) -> io::Result<()> {
        let options = r.ub4()?;
        let cursor_id = r.ub4()?;
        let has_sql = r.u8()? == 1;
        r.ub4()?; // sql length
        r.u8()?; // pointer (al8i4)
        r.ub4()?; // al8i4 length
        r.u8()?; // pointer (al8o4)
        r.u8()?; // pointer (al8o4l)
        r.u8()?; // prefetch buffer size
        let num_iters = r.ub4()?;
        r.ub4()?; // max long size
        r.u8()?; // pointer (binds)
        let num_binds = r.ub4()?;
        for _ in 0..5 {
            r.u8()?; // al8pp, al8txn, al8txl, al8kv, al8kvl
        }
        r.u8()?; // pointer (defines)
        let num_defines = r.ub4()?;
        r.ub4()?; // registration id
        r.u8()?; // al8objlist
        r.u8()?; // al8objlen
        r.u8()?; // al8blv pointer
        r.ub4()?; // al8blv
        r.u8()?; // al8dnam pointer
        r.ub4()?; // al8dnaml
        r.ub4()?; // al8regid_msb
        r.u8()?; // al8pidmlrc pointer
        r.ub4()?; // al8pidmlrcbl
        r.u8()?; // al8pidmlrcl pointer
                 // Field version >= 12.2: SQL signature and SQL id; >= 12.2 ext1: chunk ids.
        r.u8()?;
        r.ub4()?;
        r.u8()?;
        r.ub4()?;
        r.u8()?;
        r.u8()?;
        r.ub4()?;
        let sql = if has_sql {
            String::from_utf8(r.bytes_with_length()?.unwrap_or_default())
                .map_err(|_| invalid("SQL is not UTF-8".into()))?
        } else {
            String::new()
        };
        let mut al8i4 = [0u32; 13];
        for v in &mut al8i4 {
            *v = r.ub4()?;
        }
        if options & EXEC_DEFINE != 0 && num_defines > 0 {
            return Err(invalid("column defines are not supported yet".into()));
        }
        let mut bind_types = Vec::new();
        let mut bind_rows = Vec::new();
        let tz = self.db().time_zone_offset();
        if options & EXEC_BIND != 0 && num_binds > 0 {
            bind_types = read_bind_metadata(r, num_binds)?;
            // executeMany sends one row of binds per execution.
            while r.remaining() > 0 {
                bind_rows.push(read_bind_row(r, &bind_types, tz)?);
            }
        }

        let (cursor_id, sql) = if has_sql {
            (0, sql)
        } else {
            match self.cursors.get(&cursor_id) {
                Some(c) => (cursor_id, c.sql.clone()),
                None => {
                    write_error(
                        out,
                        self.call_status(),
                        0,
                        0,
                        Some(&OraError::new(1001, "invalid cursor")),
                    );
                    return Ok(());
                }
            }
        };
        let fetch = if options & EXEC_FETCH != 0 {
            num_iters
        } else {
            0
        };
        let commit = options & EXEC_COMMIT != 0;
        // al8i4[1] is the execution count for DML (executeMany without binds); al8i4[7] marks a query.
        let executions = if al8i4[7] == 1 { 1 } else { al8i4[1] };
        self.run_statement(
            cursor_id, sql, bind_types, bind_rows, executions, fetch, commit, out,
        );
        Ok(())
    }

    fn reexecute(&mut self, r: &mut ReadBuf, out: &mut WriteBuf) -> io::Result<()> {
        let cursor_id = r.ub4()?;
        let num_iters = r.ub4()?;
        let flags1 = r.ub4()?;
        let flags2 = r.ub4()?;
        let Some(cursor) = self.cursors.get(&cursor_id) else {
            write_error(
                out,
                self.call_status(),
                0,
                0,
                Some(&OraError::new(1001, "invalid cursor")),
            );
            return Ok(());
        };
        let (sql, bind_types) = (cursor.sql.clone(), cursor.bind_types.clone());
        // For DML the iteration count is the number of executions; queries run once.
        let executions = if cursor.columns.is_empty() {
            num_iters
        } else {
            1
        };
        let tz = self.db().time_zone_offset();
        let mut bind_rows = Vec::new();
        if !bind_types.is_empty() {
            while r.remaining() > 0 {
                bind_rows.push(read_bind_row(r, &bind_types, tz)?);
            }
        }
        let fetch = if flags1 & 0x20 != 0 { num_iters } else { 0 };
        let commit = flags2 & EXEC_COMMIT_REEXECUTE != 0;
        self.run_statement(
            cursor_id, sql, bind_types, bind_rows, executions, fetch, commit, out,
        );
        Ok(())
    }

    fn db(&mut self) -> &mut DbSession {
        self.db_session.as_mut().expect("logged in")
    }

    /// The end-of-call status: whether a transaction is open.
    fn call_status(&self) -> u32 {
        match &self.db_session {
            Some(s) if s.in_transaction() => CALL_STATUS_TXN_IN_PROGRESS,
            _ => 0,
        }
    }

    /// Executes `sql` once per bind row (or `executions` times without binds) and writes the describe info, the first
    /// `fetch` rows and the closing error/status message.
    #[allow(clippy::too_many_arguments)]
    fn run_statement(
        &mut self,
        cursor_id: u32,
        sql: String,
        bind_types: Vec<u8>,
        bind_rows: Vec<Vec<Value>>,
        executions: u32,
        fetch: u32,
        commit: bool,
        out: &mut WriteBuf,
    ) {
        tracing::debug!(%sql, "execute");
        let runs = if bind_rows.is_empty() {
            executions.max(1) as usize
        } else {
            bind_rows.len()
        };
        let mut result = None;
        let mut rows_affected = 0;
        for i in 0..runs {
            let binds = bind_rows.get(i).map_or(&[][..], Vec::as_slice);
            match self.db().execute(&sql, binds) {
                Ok(r) => {
                    rows_affected += r.rows_affected;
                    result = Some(r);
                }
                Err(e) => {
                    let status = self.call_status();
                    write_error(out, status, cursor_id as u16, rows_affected, Some(&e));
                    return;
                }
            }
        }
        if commit {
            if let Err(e) = self.db().commit() {
                write_error(out, 0, cursor_id as u16, rows_affected, Some(&e));
                return;
            }
        }
        let result = result.expect("at least one execution");
        let cursor_id = if cursor_id == 0 {
            self.next_cursor_id += 1;
            self.next_cursor_id - 1
        } else {
            cursor_id
        };
        if result.is_query {
            write_describe(out, &result.columns);
        }
        self.cursors.insert(
            cursor_id,
            Cursor {
                sql,
                bind_types,
                columns: result.columns,
                pending: result.rows.into(),
                fetched: 0,
            },
        );
        if result.is_query {
            self.send_rows(cursor_id, fetch, out);
        } else {
            let status = self.call_status();
            write_error(out, status, cursor_id as u16, rows_affected, None);
        }
    }

    /// Writes up to `max` pending rows of a cursor, then the end-of-call message:
    /// ORA-01403 when the cursor is exhausted, success otherwise.
    fn send_rows(&mut self, cursor_id: u32, max: u32, out: &mut WriteBuf) {
        let status = self.call_status();
        let Some(cursor) = self.cursors.get_mut(&cursor_id) else {
            write_error(
                out,
                status,
                0,
                0,
                Some(&OraError::new(1001, "invalid cursor")),
            );
            return;
        };
        let n = (max as usize).min(cursor.pending.len());
        if n > 0 {
            out.u8(MSG_ROW_HEADER);
            out.u8(0); // flags
            out.ub2(0); // number of requests
            out.ub4(0); // iteration number
            out.ub4(n as u32); // number of iterations
            out.ub2(0); // buffer length
            out.ub4(0); // bit vector
            out.ub4(0); // rxhrid
        }
        for row in cursor.pending.drain(..n) {
            out.u8(MSG_ROW_DATA);
            for (value, column) in row.iter().zip(&cursor.columns) {
                write_value(out, value, column.sql_type);
            }
        }
        cursor.fetched += n as u64;
        let fetched = cursor.fetched;
        if cursor.pending.is_empty() {
            write_error(
                out,
                status,
                cursor_id as u16,
                fetched,
                Some(&OraError::new(ORA_NO_DATA_FOUND, "no data found")),
            );
        } else {
            write_error(out, status, cursor_id as u16, fetched, None);
        }
    }
}

fn read_auth(r: &mut ReadBuf) -> io::Result<(String, HashMap<String, String>)> {
    r.u8()?; // pointer (user)
    let user_len = r.ub4()?;
    r.ub4()?; // auth mode
    r.u8()?; // pointer (key/value pairs)
    let num_pairs = r.ub4()?;
    r.u8()?;
    r.u8()?;
    let user = if user_len > 0 {
        r.string_with_length()?
    } else {
        String::new()
    };
    let mut pairs = HashMap::new();
    for _ in 0..num_pairs {
        let (k, v, _) = r.key_value()?;
        pairs.insert(k, v);
    }
    Ok((user, pairs))
}

fn read_bind_metadata(r: &mut ReadBuf, n: u32) -> io::Result<Vec<u8>> {
    let mut types = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let ora_type = r.u8()?;
        r.u8()?; // flags
        r.u8()?;
        r.u8()?;
        r.ub4()?; // max size
        r.ub4()?; // max array elements
        r.ub4()?; // cont flags
        let oid_len = r.ub4()?;
        if oid_len > 0 {
            r.bytes_with_length()?;
        }
        r.ub2()?; // version
        r.ub2()?; // charset
        r.u8()?; // charset form
        r.ub4()?; // LOB prefetch length
        r.ub4()?; // oaccolid
        types.push(ora_type);
    }
    Ok(types)
}

/// Reads one row of bind values. `tz_offset_minutes` is the session time zone, used to
/// turn time-zone-aware binds (UTC on the wire) into local date/times.
fn read_bind_row(r: &mut ReadBuf, types: &[u8], tz_offset_minutes: i32) -> io::Result<Vec<Value>> {
    if r.u8()? != MSG_ROW_DATA {
        return Err(invalid("expected bind row".into()));
    }
    types
        .iter()
        .map(|&t| {
            let bytes = r.bytes_with_length()?;
            match (t, bytes) {
                (_, None) => Ok(Value::Null),
                (TYPE_VARCHAR | TYPE_CHAR, Some(b)) => {
                    Ok(Value::varchar(String::from_utf8_lossy(&b)))
                }
                (TYPE_NUMBER | TYPE_BINARY_INTEGER, Some(b)) => {
                    Ok(Value::parse_number(&oranum::decode(&b)).unwrap_or(Value::Null))
                }
                (TYPE_DATE, Some(b)) => Ok(Value::Date(decode_date(&b)?)),
                (TYPE_TIMESTAMP, Some(b)) => Ok(Value::Timestamp(decode_date(&b)?)),
                (TYPE_TIMESTAMP_LTZ | TYPE_TIMESTAMP_TZ, Some(b)) => {
                    let utc = decode_date(&b)?;
                    let local = utc + Duration::minutes(tz_offset_minutes as i64);
                    // Without fractional seconds the client sends the short DATE form.
                    Ok(if b.len() <= 7 {
                        Value::Date(local)
                    } else {
                        Value::Timestamp(local)
                    })
                }
                (other, _) => Err(invalid(format!(
                    "binding type {other} is not supported yet"
                ))),
            }
        })
        .collect()
}

/// Decodes Oracle's 7-byte DATE or 11-byte TIMESTAMP wire format.
fn decode_date(b: &[u8]) -> io::Result<NaiveDateTime> {
    if b.len() < 7 {
        return Err(invalid("short date value".into()));
    }
    let year = (b[0] as i32 - 100) * 100 + b[1] as i32 - 100;
    let nanos = if b.len() >= 11 {
        u32::from_be_bytes([b[7], b[8], b[9], b[10]])
    } else {
        0
    };
    // Time bytes are stored plus one, so zero is never valid.
    let (Some(hour), Some(minute), Some(second)) = (
        b[4].checked_sub(1),
        b[5].checked_sub(1),
        b[6].checked_sub(1),
    ) else {
        return Err(invalid("invalid date value".into()));
    };
    NaiveDate::from_ymd_opt(year, b[2] as u32, b[3] as u32)
        .and_then(|d| d.and_hms_nano_opt(hour as u32, minute as u32, second as u32, nanos))
        .ok_or_else(|| invalid("invalid date value".into()))
}

/// Encodes a date/time as a 7-byte DATE, or an 11-byte TIMESTAMP when it has fractional seconds.
fn encode_date(d: &NaiveDateTime, with_fraction: bool) -> Vec<u8> {
    let year = d.year();
    let mut b = vec![
        (year / 100 + 100) as u8,
        (year % 100 + 100) as u8,
        d.month() as u8,
        d.day() as u8,
        d.hour() as u8 + 1,
        d.minute() as u8 + 1,
        d.second() as u8 + 1,
    ];
    if with_fraction && d.nanosecond() != 0 {
        b.extend_from_slice(&d.nanosecond().to_be_bytes());
    }
    b
}

fn write_protocol(out: &mut WriteBuf) {
    out.u8(MSG_PROTOCOL);
    out.u8(6); // protocol version
    out.u8(0);
    out.raw(b"mock-oracle\0");
    out.u16_le(CHARSET_UTF8);
    out.u8(1); // server flags
    out.u16_le(0); // number of elements
                   // FDO: the client only reads the national character set from it.
    let mut fdo = [0u8; 11];
    fdo[9..11].copy_from_slice(&CHARSET_UTF16.to_be_bytes());
    out.u16_be(fdo.len() as u16);
    out.raw(&fdo);
    let mut compile_caps = [0u8; COMPILE_CAPS_LEN];
    compile_caps[CCAP_FIELD_VERSION] = TTC_FIELD_VERSION;
    out.bytes_with_length(&compile_caps);
    let mut runtime_caps = [0u8; RUNTIME_CAPS_LEN];
    runtime_caps[RCAP_TTC] = RCAP_TTC_32K;
    out.bytes_with_length(&runtime_caps);
}

fn write_status(out: &mut WriteBuf, call_status: u32) {
    out.u8(MSG_STATUS);
    out.ub4(call_status);
    out.ub2(0); // end-to-end sequence number
}

fn write_describe(out: &mut WriteBuf, columns: &[Column]) {
    out.u8(MSG_DESCRIBE_INFO);
    out.u8(0); // chunked bytes (unused)
    out.ub4(columns.iter().map(|c| max_size(c.sql_type)).sum()); // max row size
    out.ub4(columns.len() as u32);
    if !columns.is_empty() {
        out.u8(0);
    }
    for (i, c) in columns.iter().enumerate() {
        let (ora_type, precision, scale, charset, csfrm) = match c.sql_type {
            SqlType::Number { precision, scale } => (TYPE_NUMBER, precision, scale, 0, 0),
            SqlType::Varchar2(_) => (TYPE_VARCHAR, 0, 0, CHARSET_UTF8, 1),
            SqlType::Char(_) => (TYPE_CHAR, 0, 0, CHARSET_UTF8, 1),
            SqlType::Date => (TYPE_DATE, 0, 0, 0, 0),
            SqlType::Timestamp(p) => (TYPE_TIMESTAMP, 0, p as i8, 0, 0),
        };
        let size = max_size(c.sql_type);
        out.u8(ora_type);
        out.u8(0); // flags
        out.u8(precision);
        out.u8(scale as u8);
        out.ub4(size); // max size in bytes
        out.ub4(0); // max array elements
        out.ub8(0); // cont flags
        out.ub4(0); // OID
        out.ub2(0); // version
        out.ub2(charset);
        out.u8(csfrm);
        out.ub4(match c.sql_type {
            SqlType::Varchar2(n) | SqlType::Char(n) => n,
            _ => 0,
        }); // size in chars
        out.ub4(0); // oaccolid
        out.u8(1); // nullable
        out.u8(c.name.len().min(255) as u8);
        out.str_with_ub4_length(&c.name);
        out.ub4(0); // schema
        out.ub4(0); // type name
        out.ub2(i as u16 + 1); // column position
        out.ub4(0); // uds flags
    }
    out.ub4(0); // current date
    out.ub4(0); // dcbflag
    out.ub4(0); // dcbmdbz
    out.ub4(0); // dcbmnpr
    out.ub4(0); // dcbmxpr
    out.ub4(0); // dcbqcky
}

fn max_size(t: SqlType) -> u32 {
    match t {
        SqlType::Number { .. } => 22,
        SqlType::Varchar2(n) | SqlType::Char(n) => n,
        SqlType::Date => 7,
        SqlType::Timestamp(_) => 11,
    }
}

fn write_value(out: &mut WriteBuf, value: &Value, sql_type: SqlType) {
    match (sql_type, value) {
        // A zero-length column carries no bytes at all; the client knows it is NULL.
        (SqlType::Varchar2(0), _) => {}
        (_, Value::Null) => out.u8(0),
        (SqlType::Number { .. }, v) => match oranum::encode_value(v) {
            Some(bytes) => out.bytes_with_length(&bytes),
            None => out.u8(0),
        },
        (SqlType::Date | SqlType::Timestamp(_), Value::Date(d) | Value::Timestamp(d)) => {
            out.bytes_with_length(&encode_date(d, matches!(sql_type, SqlType::Timestamp(_))))
        }
        (_, v) => out.bytes_with_length(v.to_string().as_bytes()),
    }
}

/// Writes the end-of-call ERROR message. `error: None` means success.
fn write_error(
    out: &mut WriteBuf,
    call_status: u32,
    cursor_id: u16,
    row_count: u64,
    error: Option<&OraError>,
) {
    let code = error.map_or(0, |e| e.code);
    out.u8(MSG_ERROR);
    out.ub4(call_status);
    out.ub2(0); // end-to-end sequence number
    out.ub4(0); // current row number
    out.ub2(code.min(u16::MAX as u32) as u16);
    out.ub2(0); // array element error
    out.ub2(0); // array element error
    out.ub2(cursor_id);
    out.sb2(0); // error position
    out.u8(0); // SQL type
    out.u8(0); // fatal
    out.u8(0); // flags
    out.u8(0); // user cursor options
    out.u8(0); // UPI parameter
    out.u8(0); // warning flag
    out.ub4(0); // rowid: rba
    out.ub2(0); // rowid: partition id
    out.u8(0);
    out.ub4(0); // rowid: block number
    out.ub2(0); // rowid: slot number
    out.ub4(0); // OS error
    out.u8(0); // statement number
    out.u8(0); // call number
    out.ub2(0); // padding
    out.ub4(0); // success iterations
    out.ub4(0); // oerrdd
    out.ub2(0); // batch error codes
    out.ub4(0); // batch error offsets
    out.ub2(0); // batch error messages
    out.ub4(code); // error number (extended)
    out.ub8(row_count);
    if let Some(e) = error {
        out.bytes_with_length(format!("{e}\n").as_bytes());
    }
}
