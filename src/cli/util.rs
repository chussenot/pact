//! Formatting shared by more than one subcommand.
//!
//! Every listing pact prints is built from the same three pieces: a padded
//! table, a relative age, and a note flattened to one line. `lease ls`, `pact
//! log`, `msg inbox`, `msg sent` and `agents` all render that way deliberately,
//! so these live here rather than in whichever command is biggest — a second
//! copy is how two listings drift apart.

use crate::lease::human_secs;

/// Pad every column but the last so a listing lines up without tabs. Listings
/// here are a handful of rows, so a two-pass width scan is plenty.
pub(super) fn table(rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..rows.iter().map(Vec::len).max().unwrap_or(0))
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i + 1 == row.len() {
                        cell.clone()
                    } else {
                        format!("{cell:<width$}", width = widths[i])
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// "4m2s ago" answers the question `pact agents` is asked — is this identity
/// live right now, or archaeology? An RFC3339 stamp does not.
pub(crate) fn since(rfc3339: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => format!(
            "{} ago",
            human_secs((chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
        ),
        Err(_) => rfc3339.to_string(),
    }
}

/// Collapse to a single line and cap at `max` chars. An inbox row must never
/// wrap or leak a multi-paragraph body (pact-rnc.2).
pub(crate) fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!(
            "{}…",
            flat.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

/// Seconds since an RFC3339 stamp, or `None` if it will not parse. Timestamps
/// are compared as parsed instants, never as strings: `bd` writes `…Z` and pact
/// writes `…+00:00`, which sort differently as bytes than as time.
pub(super) fn age_of(at: &str) -> Option<i64> {
    let then = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    Some((chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("a\n\nb  c", 40), "a b c");
        assert_eq!(one_line("abcdef", 4), "abc…");
        // Multi-byte input must not panic or split a char.
        assert_eq!(one_line("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn table_pads_all_but_the_last_column() {
        let out = table(&[
            vec!["a".into(), "x".into()],
            vec!["longer".into(), "y".into()],
        ]);
        assert_eq!(out, "a       x\nlonger  y");
    }
}
