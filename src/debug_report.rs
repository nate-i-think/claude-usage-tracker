//! Pure formatting helpers for `claude-usage-cli debug`: masking secrets,
//! redacting API keys, aligned tables and local timestamps. No function here
//! prints a secret except through [`mask`].

use chrono::{DateTime, TimeZone, Utc};

/// Characters kept at each end by [`mask`].
const MASK_KEEP: usize = 6;

/// As the Python debug.py `_mask` (removed): `"<none>"` for no value; all
/// `*` when 15 characters or fewer; else the first and last 6 characters
/// around "…" plus the length.
pub fn mask(value: Option<&str>) -> String {
    let Some(v) = value else {
        return "<none>".to_owned();
    };
    let chars: Vec<char> = v.chars().collect();
    let n = chars.len();
    if n <= MASK_KEEP * 2 + 3 {
        return "*".repeat(n);
    }
    let head: String = chars[..MASK_KEEP].iter().collect();
    let tail: String = chars[n - MASK_KEEP..].iter().collect();
    format!("{head}…{tail} ({n} chars)")
}

/// A Unix timestamp as "YYYY-MM-DD HH:MM" in `tz`; empty when out of range.
pub fn absolute_in<Tz: TimeZone>(ts: f64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    DateTime::<Utc>::from_timestamp(ts as i64, 0)
        .map(|t| t.with_timezone(tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Left-aligned columns separated by two spaces; the last column is not
/// padded. Widths count characters.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| -> String {
        let last = cells.len().saturating_sub(1);
        let mut s = String::new();
        for (i, cell) in cells.into_iter().enumerate() {
            if i == last {
                s.push_str(cell);
            } else {
                let pad = widths[i] - cell.chars().count();
                s.push_str(cell);
                s.push_str(&" ".repeat(pad + 2));
            }
        }
        s.trim_end().to_owned()
    };
    let mut out = vec![line(headers.to_vec())];
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    out.push(line(rule.iter().map(String::as_str).collect()));
    out.extend(
        rows.iter()
            .map(|r| line(r.iter().map(String::as_str).collect())),
    );
    out.join("\n")
}

/// `sk-ant-[\w-]{6,}` → `sk-ant-***`. The Python debug.py used `\w{6,}`, which misses
/// real keys such as `sk-ant-api03-…` (the hyphen ends the match early).
pub fn redact_api_keys(s: &str) -> String {
    const PREFIX: &str = "sk-ant-";
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(PREFIX) {
        out.push_str(&rest[..i]);
        let after = &rest[i + PREFIX.len()..];
        let word = after
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after.len());
        if after[..word].chars().count() >= 6 {
            out.push_str("sk-ant-***");
        } else {
            out.push_str(&rest[i..i + PREFIX.len() + word]);
        }
        rest = &after[word..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const NOW: f64 = 1_800_000_000.0;

    #[test]
    fn mask_matches_debug_py() {
        assert_eq!(mask(None), "<none>");
        assert_eq!(mask(Some("")), "");
        assert_eq!(mask(Some("abcdefghijklmno")), "***************");
        assert_eq!(mask(Some("abcdefghijklmnop")), "abcdef…klmnop (16 chars)");
        // Counts characters, not bytes, and never splits one.
        assert_eq!(mask(Some(&"é".repeat(20))), "éééééé…éééééé (20 chars)");
    }

    #[test]
    fn mask_never_reveals_the_middle() {
        let secret = "sk-ant-sid01-THEMIDDLEISSECRET-xyz123";
        let m = mask(Some(secret));
        assert!(!m.contains("THEMIDDLEISSECRET"), "{m}");
    }

    #[test]
    fn absolute_time_in_zone() {
        let utc = FixedOffset::east_opt(0).unwrap();
        assert_eq!(absolute_in(NOW, &utc), "2027-01-15 08:00");
        assert_eq!(absolute_in(f64::MAX, &utc), "");
    }

    #[test]
    fn table_aligns_columns() {
        let t = table(
            &["a", "bb", "c"],
            &[
                vec!["xxx".into(), "y".into(), "z".into()],
                vec!["".into(), "é".into(), "".into()],
            ],
        );
        assert_eq!(t, "a    bb  c\n---  --  -\nxxx  y   z\n     é");
    }

    #[test]
    fn redacts_api_keys() {
        let s = "oops sk-ant-api03-SECRETSECRET and sk-ant-ab end";
        let r = redact_api_keys(s);
        assert_eq!(r, "oops sk-ant-*** and sk-ant-ab end");
        assert!(!r.contains("SECRET"));
        assert_eq!(
            redact_api_keys("Bearer sk-ant-oat01-FIXTURE-SECRET"),
            "Bearer sk-ant-***"
        );
    }
}
