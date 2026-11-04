/// Centralized error type with user-friendly messages and suggestions.
///
/// Most CLI errors are surfaced as `anyhow::Error`; this enum exists for the
/// few cases where we want a stable shape the caller can match on (config
/// failures and "not found" lookups with optional suggestions).
#[derive(Debug)]
pub enum CommandError {
    Config(String),
    NotFound {
        what: String,
        suggestion: Option<String>,
    },
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::Config(msg) => write!(f, "Configuration error: {}", msg),
            CommandError::NotFound { what, suggestion } => {
                write!(f, "Not found: {}", what)?;
                if let Some(s) = suggestion {
                    write!(f, "\n  💡 Did you mean: {}?", s)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for CommandError {}

impl CommandError {
    /// Create a "not found" error with a suggestion for the user.
    pub fn not_found_with_suggestion(
        what: impl Into<String>,
        suggestion: impl Into<String>,
    ) -> Self {
        Self::NotFound {
            what: what.into(),
            suggestion: Some(suggestion.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_error_display() {
        let err = CommandError::Config("invalid config".to_string());
        assert!(err.to_string().contains("Configuration error"));
        assert!(err.to_string().contains("invalid config"));
    }

    #[test]
    fn test_not_found_with_suggestion() {
        let err = CommandError::not_found_with_suggestion(
            "provider 'zai'",
            "takka config init --provider zai",
        );
        let msg = err.to_string();
        assert!(msg.contains("Not found"));
        assert!(msg.contains("Did you mean"));
    }

    #[test]
    fn test_not_found_without_suggestion() {
        let err = CommandError::NotFound {
            what: "model 'foo'".to_string(),
            suggestion: None,
        };
        assert!(err.to_string().contains("Not found"));
        assert!(!err.to_string().contains("Did you mean"));
    }
}
