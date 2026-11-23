//! Thin adapter over `mssql-tds` (ADR-0004 decision 1).
//!
//! The driver is pre-1.0 and its API changes between minor releases, so every
//! driver call lives in this file. The rest of the provider sees only
//! [`Client`], [`Value`], [`Param`] and [`DbError`].
//!
//! **Errors are never dropped.** In the driver's default batch mode, an error
//! raised by a later statement of a batch is reported when the response is
//! drained (`close_query`), so every call drains and propagates that result.

use std::fmt;

use migrate_db_core::cancel::CancelToken;
use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{CancelHandle, EncryptionSetting};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::error::Error as TdsError;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

use crate::connstr::{Authentication, ConnectOptions, Encrypt};

/// A column value, reduced to what the provider reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Null,
    Int(i64),
    Text(String),
}

impl Value {
    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }
}

/// A query parameter (sent through `sp_executesql`).
#[derive(Debug, Clone)]
pub(crate) enum Param {
    Text(Option<String>),
    BigInt(Option<i64>),
}

/// One error reported by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerError {
    pub number: u32,
    pub class: i32,
    pub state: u8,
    /// Line relative to the batch.
    pub line: Option<i32>,
    pub procedure: Option<String>,
    pub message: String,
}

/// A driver failure, classified for exit-code mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DbError {
    /// The server rejected a statement or the login.
    Server(Vec<ServerError>),
    /// A per-request timeout fired (the driver sent ATTENTION).
    Timeout,
    /// Cancelled through a cancel handle.
    Cancelled,
    /// Transport-level failure: TCP, TLS, connection closed.
    Connection(String),
    /// Anything else.
    Other(String),
}

impl DbError {
    /// SQL login failed (18456) or the login was rejected for permissions.
    pub(crate) fn is_auth(&self) -> bool {
        matches!(self, Self::Server(errors) if errors.iter().any(|e| matches!(e.number, 18456 | 18452 | 4060 | 916)))
    }

    /// Render with batch-relative lines shifted by `line_offset` to file lines.
    pub(crate) fn describe(&self, line_offset: usize) -> String {
        match self {
            Self::Server(errors) => errors
                .iter()
                .map(|e| {
                    let mut s = format!("Msg {}, Level {}, State {}", e.number, e.class, e.state);
                    if let Some(p) = e.procedure.as_deref().filter(|p| !p.is_empty()) {
                        s.push_str(&format!(", Procedure {p}"));
                    }
                    if let Some(line) = e.line.filter(|l| *l > 0) {
                        let file_line = usize::try_from(line).unwrap_or(0) + line_offset;
                        s.push_str(&format!(", Line {file_line}"));
                    }
                    s.push_str(": ");
                    s.push_str(&e.message);
                    s
                })
                .collect::<Vec<_>>()
                .join("; "),
            Self::Timeout => "timed out (the request was cancelled)".to_owned(),
            Self::Cancelled => "cancelled".to_owned(),
            Self::Connection(m) => format!("connection error: {m}"),
            Self::Other(m) => m.clone(),
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe(0))
    }
}

impl std::error::Error for DbError {}

fn map(e: TdsError) -> DbError {
    match e {
        TdsError::SqlServerError { diagnostics } => DbError::Server(
            diagnostics
                .errors
                .into_iter()
                .map(|e| ServerError {
                    number: e.number,
                    class: e.class,
                    state: e.state,
                    line: e.line_number,
                    procedure: e.proc_name,
                    message: e.message,
                })
                .collect(),
        ),
        TdsError::TimeoutError(_) => DbError::Timeout,
        TdsError::OperationCancelledError(_) => DbError::Cancelled,
        e @ (TdsError::Io(_)
        | TdsError::ConnectionError(_)
        | TdsError::ConnectionClosed(_)
        | TdsError::TlsError(_)
        | TdsError::TlsHandshakeError { .. }) => DbError::Connection(e.to_string()),
        other => DbError::Other(other.to_string()),
    }
}

fn value(v: ColumnValues) -> Value {
    match v {
        ColumnValues::Null => Value::Null,
        ColumnValues::TinyInt(i) => Value::Int(i64::from(i)),
        ColumnValues::SmallInt(i) => Value::Int(i64::from(i)),
        ColumnValues::Int(i) => Value::Int(i64::from(i)),
        ColumnValues::BigInt(i) => Value::Int(i),
        ColumnValues::Bit(b) => Value::Int(i64::from(b)),
        ColumnValues::String(s) => Value::Text(s.to_utf8_string()),
        other => Value::Text(format!("{other:?}")),
    }
}

fn rpc(params: Vec<Param>) -> Vec<RpcParameter> {
    params
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let ty = match p {
                Param::Text(s) => SqlType::NVarcharMax(s.map(SqlString::from_utf8_string)),
                Param::BigInt(n) => SqlType::BigInt(n),
            };
            RpcParameter::new(Some(format!("@p{}", i + 1)), StatusFlags::NONE, ty)
        })
        .collect()
}

/// One open connection.
pub(crate) struct Client {
    inner: TdsClient,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    /// Connect. `access_token` is required for Entra authentication.
    pub(crate) async fn connect(
        options: &ConnectOptions,
        application_name: &str,
        access_token: Option<String>,
    ) -> Result<Self, DbError> {
        let mut ctx = ClientContext::default();
        ctx.database = options.database.clone();
        ctx.application_name = application_name.to_owned();
        ctx.connect_timeout = options.connect_timeout_secs;
        ctx.encryption_options.mode = match options.encrypt {
            Encrypt::Mandatory => EncryptionSetting::On,
            Encrypt::Strict => EncryptionSetting::Strict,
            Encrypt::Optional => EncryptionSetting::PreferOff,
        };
        ctx.encryption_options.trust_server_certificate = options.trust_server_certificate;
        match options.authentication {
            Authentication::SqlPassword => {
                ctx.tds_authentication_method = TdsAuthenticationMethod::Password;
                ctx.user_name = options.user.clone().unwrap_or_default();
                ctx.password = options.password.clone().unwrap_or_default();
            }
            Authentication::Entra => {
                ctx.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
                ctx.access_token = Some(access_token.ok_or_else(|| {
                    DbError::Other("Entra authentication needs an access token".into())
                })?);
            }
        }
        let inner = TdsConnectionProvider {}
            .create_client(ctx, &options.data_source(), None)
            .await
            .map_err(map)?;
        Ok(Self { inner })
    }

    /// Run a batch and drain its whole response. `timeout_secs` bounds the
    /// batch; on expiry the driver cancels it server-side.
    pub(crate) async fn exec(
        &mut self,
        sql: &str,
        timeout_secs: Option<u32>,
    ) -> Result<(), DbError> {
        self.exec_cancellable(sql, timeout_secs, None).await
    }

    /// Like [`exec`](Self::exec), and when `cancel` fires the driver sends a
    /// TDS ATTENTION, so the server stops the batch while the connection
    /// stays usable for the rollback and ledger writes that follow.
    pub(crate) async fn exec_cancellable(
        &mut self,
        sql: &str,
        timeout_secs: Option<u32>,
        cancel: Option<&CancelToken>,
    ) -> Result<(), DbError> {
        let handle = CancelHandle::new();
        let options = ExecuteOptions {
            timeout: timeout_secs,
            cancel: Some(&handle),
            ..ExecuteOptions::default()
        };
        let result = {
            let run = self.inner.execute(sql.to_owned(), options);
            tokio::pin!(run);
            match cancel {
                None => run.await,
                Some(token) => tokio::select! {
                    r = &mut run => r,
                    () = token.cancelled() => {
                        handle.cancel();
                        run.await
                    }
                },
            }
        };
        self.finish(result.map(|_| ())).await
    }

    /// Run a parameterized statement (`@p1`, `@p2`, …) and drain it.
    pub(crate) async fn exec_params(
        &mut self,
        sql: &str,
        params: Vec<Param>,
    ) -> Result<(), DbError> {
        let result = self
            .inner
            .execute_sp_executesql(sql.to_owned(), rpc(params), ())
            .await;
        self.finish(result.map(|_| ())).await
    }

    /// Run a query and collect the rows of its first result set.
    pub(crate) async fn query(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, DbError> {
        let result = self.inner.execute(sql.to_owned(), ()).await;
        self.collect(result.map(|_| ())).await
    }

    /// Parameterized query; rows of the first result set.
    pub(crate) async fn query_params(
        &mut self,
        sql: &str,
        params: Vec<Param>,
    ) -> Result<Vec<Vec<Value>>, DbError> {
        let result = self
            .inner
            .execute_sp_executesql(sql.to_owned(), rpc(params), ())
            .await;
        self.collect(result.map(|_| ())).await
    }

    async fn collect(&mut self, started: Result<(), TdsError>) -> Result<Vec<Vec<Value>>, DbError> {
        if let Err(e) = started {
            return self.finish(Err(e)).await.map(|()| Vec::new());
        }
        let mut rows = Vec::new();
        // A batch like `UPDATE …; SELECT …` starts with the UPDATE's row
        // count: skip to the first result that has rows.
        let positioned = if self.inner.on_rows() {
            Ok(true)
        } else {
            self.inner.advance_to_rows().await
        };
        let positioned = match positioned {
            Ok(p) => p,
            Err(e) => return self.finish(Err(e)).await.map(|()| Vec::new()),
        };
        if positioned {
            loop {
                match self.inner.next_row().await {
                    Ok(Some(row)) => rows.push(row.into_iter().map(value).collect()),
                    Ok(None) => break,
                    Err(e) => return self.finish(Err(e)).await.map(|()| Vec::new()),
                }
            }
        }
        self.finish(Ok(())).await?;
        Ok(rows)
    }

    /// Drain the rest of the response. The first error wins, but the response
    /// is always drained so the connection stays usable.
    async fn finish(&mut self, result: Result<(), TdsError>) -> Result<(), DbError> {
        let drained = self.inner.close_query().await;
        result.map_err(map)?;
        drained.map_err(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_shifts_lines_to_the_file() {
        let e = DbError::Server(vec![ServerError {
            number: 208,
            class: 16,
            state: 1,
            line: Some(2),
            procedure: None,
            message: "Invalid object name 'nope'.".into(),
        }]);
        assert_eq!(
            e.describe(9),
            "Msg 208, Level 16, State 1, Line 11: Invalid object name 'nope'."
        );
        assert!(!e.is_auth());
        let login = DbError::Server(vec![ServerError {
            number: 18456,
            class: 14,
            state: 1,
            line: Some(1),
            procedure: None,
            message: "Login failed".into(),
        }]);
        assert!(login.is_auth());
    }
}
