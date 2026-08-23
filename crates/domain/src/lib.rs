//! JiuYue pure domain logic.
//!
//! Zero-IO layer: sequence allocation abstraction, message lifecycle state
//! machine, and recall-window policy. Database/Redis adapters live in the
//! server crate; this crate must stay free of sqlx/redis dependencies.

/// Domain error type for all pure logic in this crate.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// A state transition was requested that the state machine forbids.
    #[error("illegal transition: {0}")]
    IllegalTransition(&'static str),
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_compiles() {
        assert!(true);
    }
}
