//! Short, readable names for the top-burner rows.

use crate::paths;

/// Longest name returned by [`prettify_project`], in characters, including
/// the trailing "…" of a truncated name.
pub const PROJECT_NAME_CHARS: usize = 24;

/// A short readable name for a Claude Code project directory name, using
/// the current user's home directory. See [`prettify_project_in`].
pub fn prettify_project(name: &str) -> String {
    prettify_project_in(name, &paths::home_dir().to_string_lossy())
}

/// Claude Code's directory name for a project path: `:` `\` `/` space `_`
/// and `.` each become `-` (`C:\Users\Nathan Myers` → `C--Users-Nathan-Myers`).
pub fn encode_project_path(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            ':' | '\\' | '/' | ' ' | '_' | '.' => '-',
            c => c,
        })
        .collect()
}

/// [`prettify_project`] with an explicit home directory (not encoded).
///
/// Claude Code names a project directory after its path with every
/// separator turned into `-`, and real hyphens stay `-`, so the path cannot
/// be decoded exactly. The heuristic:
///
/// 1. Cowork names (`cowork:abcd1234`) pass through.
/// 2. Strip the encoded home directory (matched ignoring ASCII case, and
///    only as a whole path component); the home itself is `~`. Otherwise
///    strip a bare drive prefix (`F--MIMIC-III` → `MIMIC-III`; `F--` alone
///    is `F:`).
/// 3. If what is left holds `---` (a separator followed by a folder that
///    began with `.` or `_`, e.g. `Documents\__tomos`), keep the part after
///    the last `---`; otherwise keep it all.
/// 4. Truncate to [`PROJECT_NAME_CHARS`] with "…".
///
/// So `C--Users-Nathan-Myers-Documents---tomos-claude-usage-monitor` reads
/// `tomos-claude-usage-moni…` and `C--Users-Nathan-Myers-Desktop-gitfiti`
/// reads `Desktop-gitfiti`.
pub fn prettify_project_in(name: &str, home: &str) -> String {
    if name.starts_with("cowork:") {
        return truncate(name);
    }
    let rest = match strip_home(name, &encode_project_path(home)) {
        Some("") => return "~".to_owned(),
        Some(rest) => rest,
        None => match strip_drive(name) {
            Some(("", drive)) => return format!("{drive}:"),
            Some((rest, _)) => rest,
            None => name,
        },
    };
    let rest = match rest.rsplit_once("---") {
        Some((_, last)) if !last.is_empty() => last,
        _ => rest,
    };
    truncate(rest)
}

/// `name` after the encoded home and its trailing `-`, when the home is a
/// whole leading component of `name` (ASCII case ignored).
fn strip_home<'a>(name: &'a str, encoded_home: &str) -> Option<&'a str> {
    if encoded_home.is_empty() {
        return None;
    }
    let head = name.get(..encoded_home.len())?;
    if !head.eq_ignore_ascii_case(encoded_home) {
        return None;
    }
    let tail = &name[encoded_home.len()..];
    if tail.is_empty() {
        Some(tail)
    } else {
        tail.strip_prefix('-')
    }
}

/// `(rest, drive letter)` for a name starting with an encoded drive (`X--`).
fn strip_drive(name: &str) -> Option<(&str, char)> {
    let mut chars = name.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    chars.as_str().strip_prefix("--").map(|rest| (rest, drive))
}

fn truncate(name: &str) -> String {
    if name.chars().count() <= PROJECT_NAME_CHARS {
        return name.to_owned();
    }
    let mut short: String = name.chars().take(PROJECT_NAME_CHARS - 1).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = r"C:\Users\Nathan Myers";

    fn pretty(name: &str) -> String {
        prettify_project_in(name, HOME)
    }

    #[test]
    fn encode_matches_claude_code_dir_names() {
        assert_eq!(encode_project_path(HOME), "C--Users-Nathan-Myers");
        assert_eq!(
            encode_project_path(r"C:\Users\Nathan Myers\Documents\__tomos\claude-usage-monitor"),
            "C--Users-Nathan-Myers-Documents---tomos-claude-usage-monitor"
        );
        assert_eq!(
            encode_project_path("/home/bob/.config/x_y"),
            "-home-bob--config-x-y"
        );
    }

    #[test]
    fn this_machines_names() {
        assert_eq!(
            pretty("C--Users-Nathan-Myers-Documents---tomos-claude-usage-monitor"),
            "tomos-claude-usage-moni…"
        );
        assert_eq!(
            pretty(
                "C--Users-Nathan-Myers-Documents---tomos-3---Tomos-Firmware-01-blakey-pico-test"
            ),
            "Tomos-Firmware-01-blake…"
        );
        assert_eq!(
            pretty("C--Users-Nathan-Myers-Desktop-gitfiti"),
            "Desktop-gitfiti"
        );
        assert_eq!(
            pretty("C--Users-Nathan-Myers-Documents-ccswap"),
            "Documents-ccswap"
        );
        assert_eq!(
            pretty("C--Users-Nathan-Myers-Documents---tomos-x"),
            "tomos-x"
        );
    }

    #[test]
    fn home_itself_and_bare_drives() {
        assert_eq!(pretty("C--Users-Nathan-Myers"), "~");
        assert_eq!(pretty("C--"), "C:");
        assert_eq!(pretty("F--MIMIC-III"), "MIMIC-III");
    }

    #[test]
    fn home_match_ignores_drive_case_but_needs_a_whole_component() {
        assert_eq!(pretty("c--users-nathan-myers-Desktop-x"), "Desktop-x");
        // "Nathan-Myers2" is a different folder: only the drive is stripped.
        assert_eq!(pretty("C--Users-Nathan-Myers2-x"), "Users-Nathan-Myers2-x");
    }

    #[test]
    fn unix_home() {
        let name = "-home-bob-src-widget";
        assert_eq!(prettify_project_in(name, "/home/bob"), "src-widget");
        assert_eq!(prettify_project_in("-home-bob", "/home/bob"), "~");
        // Another user's tree is left as it is.
        assert_eq!(prettify_project_in("-srv-app", "/home/bob"), "-srv-app");
    }

    #[test]
    fn trailing_separator_keeps_the_remainder() {
        assert_eq!(pretty("C--Users-Nathan-Myers-odd---"), "odd---");
    }

    #[test]
    fn cowork_and_short_names_pass_through() {
        assert_eq!(pretty("cowork:abcd1234"), "cowork:abcd1234");
        assert_eq!(pretty("widget"), "widget");
        assert_eq!(pretty(""), "");
    }

    #[test]
    fn truncates_by_characters_with_ellipsis() {
        let exact = "a".repeat(PROJECT_NAME_CHARS);
        assert_eq!(pretty(&exact), exact);
        let long = "é".repeat(PROJECT_NAME_CHARS + 1);
        let got = pretty(&long);
        assert_eq!(got.chars().count(), PROJECT_NAME_CHARS);
        assert_eq!(got, "é".repeat(PROJECT_NAME_CHARS - 1) + "…");
    }

    #[test]
    fn empty_home_strips_only_drives() {
        assert_eq!(prettify_project_in("C--Users-x", ""), "Users-x");
    }

    #[test]
    fn real_home_is_used_by_default() {
        let home = paths::home_dir().to_string_lossy().into_owned();
        if home.is_empty() {
            return; // no home directory in this environment
        }
        let name = format!("{}-proj", encode_project_path(&home));
        assert_eq!(prettify_project(&name), "proj");
    }
}
