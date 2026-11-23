//! SQL Server connection strings (ADO.NET keyword style).
//!
//! `Server=tcp:host,1433;Database=orders;User ID=app;Password=…;Encrypt=Mandatory`
//!
//! Only the keywords the provider needs are accepted. An unknown keyword is an
//! error, so typos don't silently weaken a setting such as `Encrypt`. Values
//! may be wrapped in `"…"`, `'…'` or `{…}` to contain `;`. Error messages never
//! include the password.

use std::fmt;

/// TLS mode (`Encrypt=`), PROVIDERS.md § SQL Server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encrypt {
    /// TDS 7.4 with TLS after pre-login. The default: Azure SQL behind private
    /// endpoints resets TDS 8 handshakes (ADR-0004).
    Mandatory,
    /// TDS 8.0: the whole stream is TLS, including pre-login.
    Strict,
    /// Encrypt only if the server requires it.
    Optional,
}

/// How to authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authentication {
    /// SQL login (`User ID` + `Password`).
    SqlPassword,
    /// Entra ID token from the default credential chain.
    Entra,
}

/// A parsed connection string.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectOptions {
    /// Host name or address.
    pub host: String,
    /// TCP port (default 1433).
    pub port: u16,
    /// Database; required, because the ledger and lock are per database.
    pub database: String,
    /// SQL login name.
    pub user: Option<String>,
    /// SQL login password. Never printed.
    pub password: Option<String>,
    /// TLS mode.
    pub encrypt: Encrypt,
    /// Skip certificate validation (local containers only).
    pub trust_server_certificate: bool,
    /// Login timeout in seconds.
    pub connect_timeout_secs: u32,
    /// Authentication method.
    pub authentication: Authentication,
    /// `Authentication=ActiveDirectoryManagedIdentity`: use only the managed
    /// identity (with `User ID` as a user-assigned client id).
    pub managed_identity_only: bool,
}

impl fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("encrypt", &self.encrypt)
            .field("trust_server_certificate", &self.trust_server_certificate)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("authentication", &self.authentication)
            .field("managed_identity_only", &self.managed_identity_only)
            .finish()
    }
}

/// An invalid connection string. The message never contains the password.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid SQL Server connection string: {0}")]
pub struct ConnStrError(pub String);

fn err(message: impl Into<String>) -> ConnStrError {
    ConnStrError(message.into())
}

impl ConnectOptions {
    /// Parse a connection string. `auth` is the target's `auth` setting
    /// (`sql` | `entra`). If it's absent, `Authentication=` in the string
    /// decides, and SQL login is the default.
    pub fn parse(connection: &str, auth: Option<&str>) -> Result<Self, ConnStrError> {
        let mut server = None;
        let mut database = None;
        let mut user = None;
        let mut password = None;
        let mut encrypt = Encrypt::Mandatory;
        let mut trust = false;
        let mut timeout = 30;
        let mut authentication = None;
        let mut managed_identity_only = false;

        for (key, value) in pairs(connection)? {
            let k = key.to_ascii_lowercase().replace(' ', "");
            match k.as_str() {
                "server" | "datasource" | "address" | "addr" => server = Some(value),
                "database" | "initialcatalog" => database = Some(value),
                "userid" | "uid" | "user" => user = Some(value),
                "password" | "pwd" => password = Some(value),
                "encrypt" => {
                    encrypt = match value.to_ascii_lowercase().as_str() {
                        "mandatory" | "true" | "yes" => Encrypt::Mandatory,
                        "strict" => Encrypt::Strict,
                        "optional" | "false" | "no" => Encrypt::Optional,
                        other => {
                            return Err(err(format!(
                                "Encrypt must be Mandatory, Strict or Optional, not `{other}`"
                            )));
                        }
                    }
                }
                "trustservercertificate" => trust = boolish("TrustServerCertificate", &value)?,
                "connecttimeout" | "connectiontimeout" | "timeout" => {
                    timeout = value.parse().map_err(|_| {
                        err(format!("Connect Timeout must be seconds, not `{value}`"))
                    })?;
                }
                "authentication" => {
                    // The Entra values SSMS / ADO.NET use all map to the one
                    // credential chain (entra.rs), so local and pipeline runs
                    // sign in the same way.
                    let mode = value.to_ascii_lowercase().replace([' ', '-'], "");
                    authentication = Some(match mode.as_str() {
                        "sqlpassword" => Authentication::SqlPassword,
                        "activedirectorydefault"
                        | "activedirectoryintegrated"
                        | "activedirectoryinteractive"
                        | "activedirectoryworkloadidentity" => Authentication::Entra,
                        "activedirectorymanagedidentity" | "activedirectorymsi" => {
                            managed_identity_only = true;
                            Authentication::Entra
                        }
                        "activedirectorypassword" | "activedirectoryserviceprincipal" => {
                            return Err(err(format!(
                                "Authentication={value} would put a secret in the connection string; \
                                 use ActiveDirectoryDefault and supply the identity through az login, \
                                 a managed identity, or AZURE_CLIENT_ID/AZURE_TENANT_ID/AZURE_CLIENT_SECRET"
                            )));
                        }
                        other => {
                            return Err(err(format!(
                                "Authentication must be SqlPassword or an Entra mode (ActiveDirectoryDefault, \
                                 ActiveDirectoryIntegrated, ActiveDirectoryInteractive, \
                                 ActiveDirectoryManagedIdentity), not `{other}`"
                            )));
                        }
                    });
                }
                // Set by migrate-db itself (lock-holder label, SPEC §7 rule 10).
                "applicationname" | "app" => {}
                _ => return Err(err(format!("unknown keyword `{key}`"))),
            }
        }

        let authentication = match auth {
            Some("sql") => Authentication::SqlPassword,
            Some("entra") => Authentication::Entra,
            Some(other) => {
                return Err(err(format!("auth must be `sql` or `entra`, not `{other}`")));
            }
            None => authentication.unwrap_or(Authentication::SqlPassword),
        };
        let server = server.ok_or_else(|| err("missing Server"))?;
        let (host, port) = parse_server(&server)?;
        let database = database
            .filter(|d| !d.is_empty())
            .ok_or_else(|| err("missing Database (the ledger and lock live in one database)"))?;
        if authentication == Authentication::SqlPassword && (user.is_none() || password.is_none()) {
            return Err(err(
                "SQL login needs User ID and Password (or set auth = \"entra\")",
            ));
        }
        if authentication == Authentication::Entra && password.is_some() {
            return Err(err("Entra authentication must not have a Password"));
        }
        Ok(Self {
            host,
            port,
            database,
            user,
            password,
            encrypt,
            trust_server_certificate: trust,
            connect_timeout_secs: timeout,
            authentication,
            managed_identity_only: managed_identity_only && authentication == Authentication::Entra,
        })
    }

    /// The `mssql-tds` data source, e.g. `tcp:host,1433`.
    pub fn data_source(&self) -> String {
        format!("tcp:{},{}", self.host, self.port)
    }
}

fn boolish(key: &str, value: &str) -> Result<bool, ConnStrError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" => Ok(true),
        "false" | "no" => Ok(false),
        other => Err(err(format!("{key} must be true or false, not `{other}`"))),
    }
}

/// `tcp:host,port`, `host,port` or `host` (port 1433). Named instances
/// (`host\instance`) need SQL Browser and aren't supported.
fn parse_server(server: &str) -> Result<(String, u16), ConnStrError> {
    let s = server.trim();
    let s = s
        .strip_prefix("tcp:")
        .or_else(|| s.strip_prefix("TCP:"))
        .unwrap_or(s);
    let (host, port) = match s.split_once(',') {
        Some((h, p)) => (
            h.trim(),
            p.trim()
                .parse()
                .map_err(|_| err(format!("invalid port in Server `{server}`")))?,
        ),
        None => (s, 1433),
    };
    if host.is_empty() {
        return Err(err("Server has no host"));
    }
    if host.contains('\\') {
        return Err(err(
            "named instances (host\\instance) are not supported; use host,port",
        ));
    }
    Ok((host.to_owned(), port))
}

/// Split `k=v;k=v`, honouring `"…"`, `'…'` and `{…}` around values.
fn pairs(s: &str) -> Result<Vec<(String, String)>, ConnStrError> {
    let mut out = Vec::new();
    let mut rest = s;
    loop {
        rest = rest.trim_start_matches([';', ' ', '\t', '\r', '\n']);
        if rest.is_empty() {
            return Ok(out);
        }
        let Some(eq) = rest.find('=') else {
            return Err(err("expected key=value"));
        };
        let key = rest[..eq].trim().to_owned();
        if key.is_empty() {
            return Err(err("empty keyword"));
        }
        rest = rest[eq + 1..].trim_start();
        let close = match rest.chars().next() {
            Some('"') => Some('"'),
            Some('\'') => Some('\''),
            Some('{') => Some('}'),
            _ => None,
        };
        let value = if let Some(close) = close {
            let body = &rest[1..];
            let Some(end) = body.find(close) else {
                return Err(err(format!("unterminated quoted value for `{key}`")));
            };
            let value = body[..end].to_owned();
            rest = &body[end + 1..];
            if !rest.trim_start().is_empty() && !rest.trim_start().starts_with(';') {
                return Err(err(format!(
                    "unexpected text after quoted value for `{key}`"
                )));
            }
            value
        } else {
            let end = rest.find(';').unwrap_or(rest.len());
            let value = rest[..end].trim().to_owned();
            rest = &rest[end..];
            value
        };
        out.push((key, value));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parses_a_typical_sql_login_string() {
        let o = ConnectOptions::parse(
            "Server=tcp:localhost,51433;Database=app;User ID=sa;Password={p;w=d};TrustServerCertificate=true",
            None,
        )
        .unwrap();
        assert_eq!((o.host.as_str(), o.port), ("localhost", 51433));
        assert_eq!(o.database, "app");
        assert_eq!(o.password.as_deref(), Some("p;w=d"));
        assert_eq!(o.encrypt, Encrypt::Mandatory, "mandatory by default");
        assert!(o.trust_server_certificate);
        assert_eq!(o.authentication, Authentication::SqlPassword);
        assert_eq!(o.data_source(), "tcp:localhost,51433");
    }

    #[test]
    fn entra_from_config_or_keyword() {
        let s = "Data Source=x.database.windows.net;Initial Catalog=db";
        let o = ConnectOptions::parse(s, Some("entra")).unwrap();
        assert_eq!((o.port, o.authentication), (1433, Authentication::Entra));
        assert!(!o.trust_server_certificate, "validated by default");
        let o = ConnectOptions::parse(
            &format!("{s};Authentication=Active Directory Default"),
            None,
        )
        .unwrap();
        assert_eq!(o.authentication, Authentication::Entra);
    }

    #[test]
    fn ssms_and_adonet_entra_modes() {
        let base = "Server=tcp:x.database.windows.net,1433;Initial Catalog=db;Authentication=";
        for mode in [
            "ActiveDirectoryIntegrated",
            "Active Directory Integrated",
            "ActiveDirectoryInteractive",
            "ActiveDirectoryDefault",
        ] {
            let o = ConnectOptions::parse(&format!("{base}{mode}"), None).unwrap();
            assert_eq!(o.authentication, Authentication::Entra, "{mode}");
            assert!(!o.managed_identity_only, "{mode}");
        }
        let o = ConnectOptions::parse(
            &format!("{base}ActiveDirectoryManagedIdentity;User ID=mi-client-id"),
            None,
        )
        .unwrap();
        assert!(o.managed_identity_only);
        assert_eq!(o.user.as_deref(), Some("mi-client-id"));
        // Modes that need a secret in the string are refused.
        for mode in ["ActiveDirectoryPassword", "ActiveDirectoryServicePrincipal"] {
            assert!(ConnectOptions::parse(&format!("{base}{mode}"), None).is_err());
        }
        // --auth sql overrides an Entra keyword only with a full SQL login.
        assert!(
            ConnectOptions::parse(&format!("{base}ActiveDirectoryIntegrated"), Some("sql"))
                .is_err()
        );
    }

    #[test]
    fn encrypt_modes() {
        let base = "Server=h;Database=d;Authentication=ActiveDirectoryDefault;Encrypt=";
        for (v, want) in [
            ("Strict", Encrypt::Strict),
            ("true", Encrypt::Mandatory),
            ("no", Encrypt::Optional),
        ] {
            assert_eq!(
                ConnectOptions::parse(&format!("{base}{v}"), None)
                    .unwrap()
                    .encrypt,
                want
            );
        }
    }

    #[test]
    fn rejects_bad_strings_without_leaking_the_password() {
        for (s, auth) in [
            ("Server=h;User ID=u;Password=SECRET", None), // no database
            ("Database=d;User ID=u;Password=SECRET", None), // no server
            ("Server=h;Database=d;User ID=u", None),      // no password
            ("Server=h;Database=d;Encrpyt=no;Password=SECRET", None), // typo
            ("Server=h;Database=d;Encrypt=maybe;Password=SECRET", None),
            ("Server=h\\inst;Database=d;User ID=u;Password=SECRET", None),
            (
                "Server=h,notaport;Database=d;User ID=u;Password=SECRET",
                None,
            ),
            ("Server=h;Database=d;Password=SECRET", Some("entra")),
            (
                "Server=h;Database=d;User ID=u;Password=SECRET",
                Some("kerberos"),
            ),
            ("Server=h;Database=d;Password=\"SECRET", None),
        ] {
            let e = ConnectOptions::parse(s, auth).unwrap_err();
            assert!(!e.to_string().contains("SECRET"), "{s}: {e}");
        }
    }

    #[test]
    fn debug_redacts_the_password() {
        let o =
            ConnectOptions::parse("Server=h;Database=d;User ID=u;Password=SECRET", None).unwrap();
        let dbg = format!("{o:?}");
        assert!(
            !dbg.contains("SECRET") && dbg.contains("<redacted>"),
            "{dbg}"
        );
    }
}
