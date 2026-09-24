//! Scope-token validation. `docs/spec.md` §8, Design v2.1 §8.
//!
//! # credSync validates scope claims; it never authorizes
//!
//! The host decides who may sync what, and says so by minting a short-lived JWT carrying an
//! explicit scope list. This module's entire job is to enforce that decision: check the signature,
//! check it has not expired, and refuse any scope the token does not name. It has no opinion about
//! users, roles, enrolments or tenancy, because every one of those is a domain judgement that
//! belongs to the host.
//!
//! That restatement matters. Tenant isolation already lives in the host's database, usually as RLS.
//! This layer repeats the guarantee at the sync boundary so a bug in one is not a breach on its
//! own.
//!
//! # The algorithm is pinned, and never read from the token
//!
//! The classic JWT break is algorithm confusion: the attacker edits the header and the server
//! obligingly changes how it verifies. Two shapes, both fatal:
//!
//! - `{"alg":"none"}` — the server accepts an unsigned token.
//! - `{"alg":"HS256"}` against a server expecting `RS256` — the attacker signs with HMAC using the
//!   *public* key as the secret, which they have, and the server verifies it happily.
//!
//! Both work by letting the token choose the verification. So a [`Verifier`] is built around
//! exactly one algorithm, fixed when it is constructed, and a token whose header names anything
//! else is refused **before** any signature is checked. The header's `alg` is only ever compared
//! against the pinned value; it never selects anything.
//!
//! For the same reason `kid` is ignored. A key id in an attacker-controlled header that selects
//! which key to verify with is the same bug wearing a different hat.
//!
//! # Expiry is required, not optional
//!
//! Revocation is honoured at expiry, so a token without an `exp` would be a permanent grant. A
//! token that omits it is malformed rather than eternal. For cuts that cannot wait for expiry
//! there is the blocklist — see [`crate::blocklist`].

use credsync_protocol::ScopeId;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

/// Why a request was refused at the authorization boundary.
///
/// Deliberately granular for the operator's log. What reaches the *client* is much coarser — see
/// [`AuthError::client_message`] — because telling an attacker which of these fired is telling
/// them how to make progress.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthError {
    /// The token is not a well-formed JWT, or its claims do not parse.
    Malformed {
        /// What was wrong, for the operator.
        detail: String,
    },

    /// The token's header names an algorithm this verifier does not use.
    ///
    /// Includes `none`. See the module docs: this is refused before the signature is examined.
    WrongAlgorithm {
        /// What the token asked for.
        found: String,
    },

    /// The signature did not verify against the configured key.
    BadSignature,

    /// The token has expired. Ordinary, and the reason TTLs are short.
    Expired,

    /// The token carries no scopes at all, so it grants nothing.
    ///
    /// Refused rather than treated as an empty allowance, because a token that grants nothing is
    /// far more likely to be a minting bug than a deliberate act.
    NoScopes,

    /// A scope was requested that the token does not claim.
    ScopeNotClaimed {
        /// What was asked for.
        requested: String,
    },

    /// The scope is on the server-side blocklist, whatever the token says.
    ScopeBlocked {
        /// The scope that was cut.
        scope: String,
    },
}

impl AuthError {
    /// What the client is told.
    ///
    /// One sentence for every case, on purpose. An error that distinguishes "bad signature" from
    /// "scope not claimed" from "blocked" is an oracle: it lets an attacker with a stolen or
    /// forged token learn which scopes exist and which of their edits got closer. The operator
    /// gets the detail in the log; the client gets a door that looks the same from outside.
    ///
    /// The one exception is expiry, which every honest client hits routinely and must handle by
    /// fetching a new token. Concealing it would break correct clients to inconvenience nobody —
    /// an expired token is one the attacker already knows the expiry of, since `exp` is in the
    /// payload they are holding.
    #[must_use]
    pub const fn client_message(&self) -> &'static str {
        match self {
            Self::Expired => "This session has expired. Please sign in again.",
            _ => "Not authorized for this scope.",
        }
    }

    /// Whether the client should refresh its token and retry.
    ///
    /// Only expiry. Retrying any of the others without a new token produces the same answer.
    #[must_use]
    pub const fn is_expired(&self) -> bool {
        matches!(self, Self::Expired)
    }
}

impl core::fmt::Display for AuthError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed { detail } => write!(f, "malformed scope token: {detail}"),
            Self::WrongAlgorithm { found } => {
                write!(
                    f,
                    "token algorithm '{found}' is not the one this server accepts"
                )
            }
            Self::BadSignature => write!(f, "the token's signature did not verify"),
            Self::Expired => write!(f, "the token has expired"),
            Self::NoScopes => write!(f, "the token claims no scopes"),
            Self::ScopeNotClaimed { requested } => {
                write!(f, "scope '{requested}' is not in the token's claims")
            }
            Self::ScopeBlocked { scope } => write!(f, "scope '{scope}' is blocked"),
        }
    }
}

impl core::error::Error for AuthError {}

/// The claims credSync reads. Everything else in the token is ignored.
#[derive(Debug, Deserialize)]
struct RawClaims {
    /// The scopes this token grants. Required.
    scopes: Vec<String>,
    /// Expiry, seconds since the Unix epoch. Required — see the module docs.
    exp: i64,
    /// Whoever the host says this is. Opaque to credSync; carried for the audit log only.
    #[serde(default)]
    sub: Option<String>,
}

/// A validated token: what the host granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// The scopes this token grants, already validated as scope ids.
    pub scopes: Vec<ScopeId>,
    /// The host's identifier for the bearer. Opaque to credSync.
    pub subject: Option<String>,
    /// Expiry, seconds since the Unix epoch.
    pub expires_at: i64,
}

impl Claims {
    /// Whether this token claims the given scope.
    ///
    /// An exact string match, deliberately. No prefixes, no wildcards, no hierarchy: a token for
    /// `inst:acme` must not reach `inst:acme-corp`, and any prefix rule makes that a one-character
    /// mistake away. If a host wants a bearer to reach many scopes it lists them.
    #[must_use]
    pub fn claims_scope(&self, scope: &ScopeId) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Refuses unless this token claims the scope.
    ///
    /// # Errors
    /// Returns [`AuthError::ScopeNotClaimed`] if the scope is not in the token's list.
    pub fn authorize(&self, scope: &ScopeId) -> Result<(), AuthError> {
        if self.claims_scope(scope) {
            return Ok(());
        }
        Err(AuthError::ScopeNotClaimed {
            requested: scope.as_str().to_owned(),
        })
    }
}

/// Verifies scope tokens against one key and one algorithm.
///
/// Construct it once at start-up and share it. The algorithm is fixed here and nowhere else; see
/// the module docs for why that is the whole defence against algorithm confusion.
pub struct Verifier {
    key: DecodingKey,
    algorithm: Algorithm,
    validation: Validation,
}

impl core::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The key never goes near a log, an error message or a panic message.
        f.debug_struct("Verifier")
            .field("algorithm", &self.algorithm)
            .finish_non_exhaustive()
    }
}

impl Verifier {
    /// Builds a verifier for one key and one algorithm.
    ///
    /// `leeway` is how much clock skew to tolerate on `exp`, in seconds. Some is necessary — the
    /// host minting the token and the server checking it are different machines — but it is a
    /// window in which a revoked token still works, so it is small by default and named here
    /// rather than buried.
    #[must_use]
    pub fn new(key: DecodingKey, algorithm: Algorithm, leeway: u64) -> Self {
        let mut validation = Validation::new(algorithm);
        // Exactly one. `Validation::new` already sets this, and it is restated because the whole
        // security property of this module is that this list has one element.
        validation.algorithms = vec![algorithm];
        validation.leeway = leeway;
        // `exp` is mandatory: a token without one would be a permanent grant.
        validation.required_spec_claims = ["exp"].into_iter().map(String::from).collect();
        validation.validate_exp = true;
        // Neither is used by credSync, and validating a claim nobody sets refuses every real
        // token. The host's audience and issuer rules are the host's business.
        validation.validate_aud = false;

        Self {
            key,
            algorithm,
            validation,
        }
    }

    /// The algorithm this verifier accepts, and the only one it will ever accept.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Verifies a token and returns what it grants.
    ///
    /// # Errors
    /// Returns [`AuthError`] if the token is malformed, signed with the wrong algorithm or key,
    /// expired, or claims no scopes.
    pub fn verify(&self, token: &str) -> Result<Claims, AuthError> {
        // Step one, before anything else touches the token: does the header name our algorithm?
        //
        // `jsonwebtoken` also enforces this, and doing it here as well is deliberate. This is the
        // property the module exists to guarantee, and a defence that lives only inside a
        // dependency is one that a version bump can silently relax. The test suite plants both
        // confusion attacks against *this* check.
        let header = decode_header(token).map_err(|e| AuthError::Malformed {
            detail: format!("unreadable header: {e}"),
        })?;
        if header.alg != self.algorithm {
            return Err(AuthError::WrongAlgorithm {
                found: format!("{:?}", header.alg),
            });
        }

        let data = decode::<RawClaims>(token, &self.key, &self.validation).map_err(map_jwt_err)?;
        let raw = data.claims;

        if raw.scopes.is_empty() {
            return Err(AuthError::NoScopes);
        }

        // A scope in a token still has to be a scope. A token claiming a 4 KB "scope" full of
        // control characters is refused here rather than carried into a SQL parameter.
        let mut scopes = Vec::with_capacity(raw.scopes.len());
        for s in raw.scopes {
            let scope = ScopeId::new(s).map_err(|e| AuthError::Malformed {
                detail: format!("a claimed scope is not a valid scope id: {e}"),
            })?;
            scopes.push(scope);
        }

        Ok(Claims {
            scopes,
            subject: raw.sub,
            expires_at: raw.exp,
        })
    }
}

/// Maps the library's error kinds onto ours, keeping expiry distinguishable and collapsing the
/// rest toward "refused".
fn map_jwt_err(e: jsonwebtoken::errors::Error) -> AuthError {
    use jsonwebtoken::errors::ErrorKind;
    match e.kind() {
        ErrorKind::ExpiredSignature => AuthError::Expired,
        ErrorKind::InvalidSignature => AuthError::BadSignature,
        ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => {
            AuthError::WrongAlgorithm {
                found: "rejected by the decoder".to_owned(),
            }
        }
        other => AuthError::Malformed {
            detail: format!("{other:?}"),
        },
    }
}
