//! Entra ID access tokens for Azure SQL (ADR-0004, SPEC §10.3).
//!
//! One credential chain everywhere (SPEC §10.0 parity), tried in order:
//!
//! 1. **Workload identity**, when `AZURE_FEDERATED_TOKEN_FILE` is set
//!    (AKS, federated pipeline identities).
//! 2. **Client secret**, when `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and
//!    `AZURE_CLIENT_SECRET` are all set.
//! 3. **Developer tools**: the Azure CLI login (`az login`), then the Azure
//!    Developer CLI. This also covers Azure DevOps `AzureCLI@2` tasks.
//! 4. **Managed identity** (self-hosted agents, Azure VMs). A user-assigned
//!    identity is chosen with `AZURE_CLIENT_ID`.
//!
//! `MIGRATE_DB_AZURE_CREDENTIAL` = `workload-identity` | `client-secret` |
//! `cli` | `managed-identity` pins a single source. Tokens are never logged
//! or put in error messages.

use std::sync::Arc;

use azure_core::credentials::{Secret, TokenCredential};
use azure_identity::{
    ClientSecretCredential, DeveloperToolsCredential, ManagedIdentityCredential,
    ManagedIdentityCredentialOptions, UserAssignedId, WorkloadIdentityCredential,
};

/// Token scope for Azure SQL Database / Managed Instance.
pub const SQL_SCOPE: &str = "https://database.windows.net/.default";

/// Variable that pins one credential source.
pub const PIN_VAR: &str = "MIGRATE_DB_AZURE_CREDENTIAL";

/// How long to wait for the managed identity endpoint.
const MANAGED_IDENTITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to wait for any other source (`az` can be slow on first use).
const OTHER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// One credential source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `AZURE_FEDERATED_TOKEN_FILE` + client and tenant id.
    WorkloadIdentity,
    /// `AZURE_TENANT_ID` + `AZURE_CLIENT_ID` + `AZURE_CLIENT_SECRET`.
    ClientSecret,
    /// `az login` / `azd auth login`.
    DeveloperTools,
    /// Azure managed identity.
    ManagedIdentity,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::WorkloadIdentity => "workload identity",
            Self::ClientSecret => "client secret (AZURE_CLIENT_SECRET)",
            Self::DeveloperTools => "Azure CLI / Azure Developer CLI login",
            Self::ManagedIdentity => "managed identity",
        }
    }

    /// Parse a `MIGRATE_DB_AZURE_CREDENTIAL` value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "workload-identity" | "workload" => Some(Self::WorkloadIdentity),
            "client-secret" | "secret" => Some(Self::ClientSecret),
            "cli" | "az" | "developer-tools" => Some(Self::DeveloperTools),
            "managed-identity" | "msi" => Some(Self::ManagedIdentity),
            _ => None,
        }
    }
}

/// Which sources to try, and the managed identity to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Sources in order.
    pub sources: Vec<Source>,
    /// User-assigned managed identity client id, if any.
    pub managed_identity_client_id: Option<String>,
}

/// Decide the chain from the environment (`lookup`) and an optional hint
/// from the connection string (`ActiveDirectoryManagedIdentity`, with
/// `User ID` as the user-assigned client id). Pure, so it is unit-tested.
pub fn plan(
    lookup: &dyn Fn(&str) -> Option<String>,
    managed_identity_hint: Option<Option<String>>,
) -> Result<Plan, String> {
    let set = |k: &str| lookup(k).is_some_and(|v| !v.is_empty());
    let client_id = lookup("AZURE_CLIENT_ID").filter(|v| !v.is_empty());
    if let Some(user_id) = managed_identity_hint {
        return Ok(Plan {
            sources: vec![Source::ManagedIdentity],
            managed_identity_client_id: user_id.or(client_id),
        });
    }
    if let Some(pin) = lookup(PIN_VAR).filter(|v| !v.is_empty()) {
        let source = Source::parse(&pin).ok_or_else(|| {
            format!(
                "{PIN_VAR} must be workload-identity, client-secret, cli or managed-identity, not `{pin}`"
            )
        })?;
        return Ok(Plan {
            sources: vec![source],
            managed_identity_client_id: client_id,
        });
    }
    let mut sources = Vec::new();
    if set("AZURE_FEDERATED_TOKEN_FILE") {
        sources.push(Source::WorkloadIdentity);
    }
    if set("AZURE_TENANT_ID") && set("AZURE_CLIENT_ID") && set("AZURE_CLIENT_SECRET") {
        sources.push(Source::ClientSecret);
    }
    sources.push(Source::DeveloperTools);
    sources.push(Source::ManagedIdentity);
    Ok(Plan {
        sources,
        managed_identity_client_id: client_id,
    })
}

fn credential(
    source: Source,
    plan: &Plan,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> azure_core::Result<Arc<dyn TokenCredential>> {
    Ok(match source {
        Source::WorkloadIdentity => WorkloadIdentityCredential::new(None)?,
        Source::ClientSecret => {
            let get = |k: &str| lookup(k).unwrap_or_default();
            ClientSecretCredential::new(
                &get("AZURE_TENANT_ID"),
                get("AZURE_CLIENT_ID"),
                Secret::new(get("AZURE_CLIENT_SECRET")),
                None,
            )?
        }
        Source::DeveloperTools => DeveloperToolsCredential::new(None)?,
        Source::ManagedIdentity => {
            ManagedIdentityCredential::new(Some(ManagedIdentityCredentialOptions {
                user_assigned_id: plan
                    .managed_identity_client_id
                    .clone()
                    .map(UserAssignedId::ClientId),
                ..Default::default()
            }))?
        }
    })
}

/// A token for Azure SQL, from the first source that provides one. The
/// error lists every source tried and why it failed (no secrets).
pub async fn sql_token(managed_identity_hint: Option<Option<String>>) -> Result<String, String> {
    let lookup = |k: &str| std::env::var(k).ok();
    let plan = plan(&lookup, managed_identity_hint)?;
    let mut failures = Vec::new();
    for &source in &plan.sources {
        let attempt = async {
            let cred = credential(source, &plan, &lookup)?;
            cred.get_token(&[SQL_SCOPE], None).await
        };
        // The SDK retries the managed identity endpoint for well over a
        // minute when there is none (a laptop, a hosted agent). A real one
        // answers in milliseconds, so bound the wait.
        let limit = match source {
            Source::ManagedIdentity => MANAGED_IDENTITY_TIMEOUT,
            _ => OTHER_TIMEOUT,
        };
        match tokio::time::timeout(limit, attempt).await {
            Ok(Ok(token)) => return Ok(token.token.secret().to_owned()),
            Ok(Err(e)) => {
                failures.push(format!("{}: {}", source.name(), first_line(&e.to_string())))
            }
            Err(_) => failures.push(format!(
                "{}: no response within {}s",
                source.name(),
                limit.as_secs()
            )),
        }
    }
    Err(format!(
        "no Entra ID credential produced a token for Azure SQL; tried {}. \
         Locally, run `az login`; in a pipeline, run inside an AzureCLI task or use a managed or \
         workload identity",
        failures.join("; ")
    ))
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn default_chain_on_a_laptop_is_cli_then_managed_identity() {
        let p = plan(&env(&[]), None).unwrap();
        assert_eq!(p.sources, [Source::DeveloperTools, Source::ManagedIdentity]);
    }

    #[test]
    fn pipeline_variables_put_their_sources_first() {
        let p = plan(
            &env(&[
                ("AZURE_FEDERATED_TOKEN_FILE", "/var/run/token"),
                ("AZURE_TENANT_ID", "t"),
                ("AZURE_CLIENT_ID", "c"),
                ("AZURE_CLIENT_SECRET", "s"),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(
            p.sources,
            [
                Source::WorkloadIdentity,
                Source::ClientSecret,
                Source::DeveloperTools,
                Source::ManagedIdentity
            ]
        );
        assert_eq!(p.managed_identity_client_id.as_deref(), Some("c"));
        // A partial client-secret set is not used.
        let p = plan(&env(&[("AZURE_CLIENT_SECRET", "s")]), None).unwrap();
        assert!(!p.sources.contains(&Source::ClientSecret));
    }

    #[test]
    fn pin_and_connection_string_hint() {
        let p = plan(&env(&[(PIN_VAR, "cli")]), None).unwrap();
        assert_eq!(p.sources, [Source::DeveloperTools]);
        assert!(plan(&env(&[(PIN_VAR, "kerberos")]), None).is_err());
        // ActiveDirectoryManagedIdentity with User ID = client id.
        let p = plan(&env(&[(PIN_VAR, "cli")]), Some(Some("mi-client".into()))).unwrap();
        assert_eq!(p.sources, [Source::ManagedIdentity]);
        assert_eq!(p.managed_identity_client_id.as_deref(), Some("mi-client"));
    }
}
