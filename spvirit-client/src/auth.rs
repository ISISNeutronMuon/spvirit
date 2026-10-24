use crate::types::PvGetOptions;

pub fn resolved_authnz_user(opts: &PvGetOptions) -> String {
    opts.authnz_user.clone().unwrap_or_else(default_authnz_user)
}

pub fn resolved_authnz_host(opts: &PvGetOptions) -> String {
    opts.authnz_host.clone().unwrap_or_else(default_authnz_host)
}

pub fn default_authnz_user() -> String {
    std::env::var("PVA_AUTHNZ_USER")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

pub fn default_authnz_host() -> String {
    std::env::var("PVA_AUTHNZ_HOST")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::env::var("HOST"))
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// The `ca`-method credentials a PVA handshake presents.
///
/// Resolving these once, up front, is what keeps the crate's three handshake
/// sites from disagreeing about where credentials come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthnzCreds {
    pub user: String,
    pub host: String,
}

impl AuthnzCreds {
    /// Configured values where present, environment-derived defaults otherwise.
    pub fn from_options(opts: &PvGetOptions) -> Self {
        Self {
            user: resolved_authnz_user(opts),
            host: resolved_authnz_host(opts),
        }
    }

    /// Environment-derived defaults only. For call sites that genuinely have
    /// no options in hand.
    pub fn from_env() -> Self {
        Self {
            user: default_authnz_user(),
            host: default_authnz_host(),
        }
    }
}

impl Default for AuthnzCreds {
    fn default() -> Self {
        Self::from_env()
    }
}
