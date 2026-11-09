//! Secrets and credentials (§8.0, C3). A [`Secret`] prints `***`, has no
//! `Serialize`, and is zeroed on drop.

use std::fmt;

/// A secret value: a password or a token.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }

    /// The value, for the one place that sends it (a request header).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zero bytes are valid UTF-8, so the String stays valid.
        unsafe { self.0.as_bytes_mut() }.fill(0);
    }
}

/// What a request authenticates with. The user name is not a secret but is
/// redacted from errors all the same (a 516 message names it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub user: Option<String>,
    pub password: Option<Secret>,
}

/// Why credentials could not be read (the OS secret store refused, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialError;

/// The host's credential source, called per request. The Studio reads the
/// Keychain (cached per session); a browser connection returns `None` (C4).
pub trait CredentialProvider: Send + Sync {
    fn credentials(&self) -> Result<Option<Credentials>, CredentialError>;
}

/// No credentials: a loopback server's default user, or a same-origin
/// proxy that adds auth server-side.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCredentials;

impl CredentialProvider for NoCredentials {
    fn credentials(&self) -> Result<Option<Credentials>, CredentialError> {
        Ok(None)
    }
}

/// Fixed credentials held in memory (tests, and hosts that read their own
/// settings at startup).
#[derive(Debug, Clone)]
pub struct StaticCredentials(pub Credentials);

impl CredentialProvider for StaticCredentials {
    fn credentials(&self) -> Result<Option<Credentials>, CredentialError> {
        Ok(Some(self.0.clone()))
    }
}
