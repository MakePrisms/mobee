use std::fmt;

/// A full Git commit object id advertised by a seller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitOid(String);

impl CommitOid {
    /// Parses a full SHA-1 or SHA-256 Git object id.
    pub fn parse(value: impl Into<String>) -> Result<Self, DeliveryError> {
        let value = value.into();
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DeliveryError::InvalidCommitOid);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the canonical lowercase object id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Git delivery fields advertised by a result event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDelivery {
    repo: String,
    branch: String,
    commit_oid: CommitOid,
}

impl GitDelivery {
    /// Creates an advertised git delivery from result-event fields.
    pub fn new(
        repo: impl Into<String>,
        branch: impl Into<String>,
        commit_oid: CommitOid,
    ) -> Result<Self, DeliveryError> {
        let repo = repo.into();
        let branch = branch.into();
        if repo.is_empty() {
            return Err(DeliveryError::MissingRepo);
        }
        if branch.is_empty()
            || branch.starts_with('-')
            || branch.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(DeliveryError::InvalidBranch);
        }
        Ok(Self {
            repo,
            branch,
            commit_oid,
        })
    }

    /// Returns the repository locator carried by the event.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Returns the advertised branch name.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Returns the advertised full commit object id.
    pub fn commit_oid(&self) -> &CommitOid {
        &self.commit_oid
    }
}

/// Proof that the advertised branch tip was fetched into buyer custody.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedDelivery {
    commit_oid: CommitOid,
}

impl VerifiedDelivery {
    /// Binds a fetched tip to the advertisement or refuses a mismatch.
    pub fn from_fetched_tip(
        advertised: &GitDelivery,
        fetched_tip: CommitOid,
    ) -> Result<Self, DeliveryError> {
        if &fetched_tip != advertised.commit_oid() {
            return Err(DeliveryError::TipMismatch {
                expected: advertised.commit_oid().clone(),
                actual: fetched_tip,
            });
        }
        Ok(Self {
            commit_oid: advertised.commit_oid().clone(),
        })
    }

    /// Returns the verified commit object id used by payment and receipt binding.
    pub fn commit_oid(&self) -> &CommitOid {
        &self.commit_oid
    }
}

/// Injected buyer-side delivery verification effect.
pub trait DeliveryVerifier {
    /// Fetches and verifies a delivery before payment intent is persisted.
    fn verify(&mut self, delivery: &GitDelivery) -> Result<VerifiedDelivery, DeliveryError>;
}

/// Fail-closed delivery verification errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    InvalidCommitOid,
    MissingRepo,
    InvalidBranch,
    Transport(crate::delivery_transport::TransportRefuse),
    GitUnavailable,
    /// A required child process (e.g. `git`) could not be spawned. Names the program
    /// and the `io::ErrorKind` so a missing binary is not misreported as "git unavailable".
    GitSpawnFailed {
        program: &'static str,
        kind: std::io::ErrorKind,
    },
    GitCommandFailed(&'static str),
    MissingFetchedTip,
    TipMismatch {
        expected: CommitOid,
        actual: CommitOid,
    },
    MissingCommitObject,
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommitOid => {
                formatter.write_str("delivery commit oid must be 40 or 64 hex characters")
            }
            Self::MissingRepo => formatter.write_str("delivery repository is missing"),
            Self::InvalidBranch => formatter.write_str("delivery branch is invalid"),
            Self::Transport(refuse) => write!(formatter, "{refuse}"),
            Self::GitUnavailable => formatter.write_str("git executable is unavailable"),
            Self::GitSpawnFailed { program, kind } => {
                write!(formatter, "failed to spawn {program}: {kind:?}")
            }
            Self::GitCommandFailed(operation) => write!(formatter, "git {operation} failed"),
            Self::MissingFetchedTip => {
                formatter.write_str("git fetch did not produce one commit tip")
            }
            Self::TipMismatch { expected, actual } => {
                write!(
                    formatter,
                    "fetched tip {actual} does not match advertised {expected}"
                )
            }
            Self::MissingCommitObject => {
                formatter.write_str("fetched commit object is not in buyer custody")
            }
        }
    }
}

impl std::error::Error for DeliveryError {}

impl From<crate::delivery_transport::TransportRefuse> for DeliveryError {
    fn from(value: crate::delivery_transport::TransportRefuse) -> Self {
        Self::Transport(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_oid_requires_full_hex() {
        assert_eq!(
            CommitOid::parse("abc"),
            Err(DeliveryError::InvalidCommitOid)
        );
        assert_eq!(
            CommitOid::parse("z".repeat(40)),
            Err(DeliveryError::InvalidCommitOid)
        );
        assert_eq!(
            CommitOid::parse("A".repeat(40)).expect("full oid").as_str(),
            "a".repeat(40)
        );
        assert_eq!(
            CommitOid::parse("B".repeat(64))
                .expect("full sha256 oid")
                .as_str(),
            "b".repeat(64)
        );
    }

    #[test]
    fn verified_delivery_refuses_a_different_fetched_tip() {
        let advertised = GitDelivery::new(
            "repo",
            "work",
            CommitOid::parse("1".repeat(40)).expect("advertised oid"),
        )
        .expect("delivery");

        assert!(matches!(
            VerifiedDelivery::from_fetched_tip(
                &advertised,
                CommitOid::parse("2".repeat(40)).expect("fetched oid")
            ),
            Err(DeliveryError::TipMismatch { .. })
        ));
    }
}
