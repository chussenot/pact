use anyhow::{bail, Result};

/// Resolve the calling agent's identity: `--agent` flag, then `PACT_AGENT` env var.
/// Never guesses or generates one.
pub fn resolve_agent(flag: Option<&str>) -> Result<String> {
    let raw = match flag {
        Some(v) => v.to_string(),
        None => match std::env::var("PACT_AGENT") {
            Ok(v) => v,
            Err(_) => bail!("no agent identity: pass --agent <name> or set PACT_AGENT"),
        },
    };
    validate(&raw)?;
    Ok(raw)
}

/// `[a-z0-9][a-z0-9-]{1,31}`
pub fn validate(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let len_ok = (2..=32).contains(&bytes.len());
    let first_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let rest_ok = bytes[1.min(bytes.len())..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if len_ok && first_ok && rest_ok {
        Ok(())
    } else {
        bail!("invalid agent name {name:?}: must match [a-z0-9][a-z0-9-]{{1,31}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        assert!(validate("claude-1").is_ok());
        assert!(validate("a1").is_ok());
        assert!(validate(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn rejects_invalid_names() {
        assert!(validate("a").is_err()); // too short
        assert!(validate(&"a".repeat(33)).is_err()); // too long
        assert!(validate("Claude").is_err()); // uppercase
        assert!(validate("-claude").is_err()); // leading dash
        assert!(validate("claude_1").is_err()); // underscore
    }

    #[test]
    fn flag_takes_precedence_over_env() {
        std::env::set_var("PACT_AGENT", "env-agent");
        assert_eq!(resolve_agent(Some("flag-agent")).unwrap(), "flag-agent");
        std::env::remove_var("PACT_AGENT");
    }
}
