//! Pattern matching for the globs of `input.devices` and `layer_rules`.

/// Whether a device name matches a pattern from `input.devices`.
///
/// `*` matches any (possibly empty) run of characters and `?` exactly one;
/// everything else is literal. Matching ignores case: device names capitalise
/// inconsistently (`… 06CB:CE58 Touchpad`) and patterns are written in
/// lowercase. Character classes (`[abc]`) are not supported: device names do
/// not need them, and treating `[` as text beats failing to match silently.
pub fn device_matches(pattern: &str, name: &str) -> bool {
    return glob_matches(pattern, name);
}

/// The matcher behind [`device_matches`], also used for layer namespaces.
///
/// Case-insensitive, same syntax. Layer namespaces are case sensitive in
/// theory, but written consistently enough (`waybar`, `swaync`) that the
/// friendlier matching wins.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_lowercase().chars().collect::<Vec<_>>();
    let name = name.to_lowercase().chars().collect::<Vec<_>>();

    let mut p = 0;
    let mut n = 0;
    // Where to resume after a `*` that matched nothing yet: the pattern
    // position after the star and the name position it starts matching at.
    let mut star = None;

    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p + 1, n));
            p += 1;
        } else if let Some((after_star, from)) = star {
            // Backtrack: let the star eat one more character.
            p = after_star;
            n = from + 1;
            star = Some((after_star, from + 1));
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    return p == pattern.len();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_have_to_match_exactly() {
        assert!(device_matches(
            "AT Translated Set 2 keyboard",
            "AT Translated Set 2 keyboard"
        ));
        assert!(!device_matches("keyboard", "AT Translated Set 2 keyboard"));
    }

    #[test]
    fn star_matches_any_run() {
        assert!(device_matches(
            "*touchpad*",
            "SYNA8004:00 06CB:CE58 Touchpad"
        ));
        assert!(device_matches(
            "*Touchpad*",
            "SYNA8004:00 06CB:CE58 Touchpad"
        ));
        assert!(device_matches("*", "anything at all"));
        assert!(device_matches("keyboard*", "keyboard"));
        assert!(device_matches("*keyboard", "keyboard"));
        assert!(!device_matches("*mouse*", "SYNA8004:00 06CB:CE58 Touchpad"));
    }

    #[test]
    fn matching_ignores_case() {
        assert!(device_matches("*TOUCHPAD*", "Synaptics touchpad"));
        assert!(device_matches(
            "at translated set 2 keyboard",
            "AT Translated Set 2 keyboard"
        ));
    }

    #[test]
    fn question_mark_matches_one_character() {
        assert!(device_matches("usb-????", "usb-0000"));
        assert!(!device_matches("usb-????", "usb-000"));
        assert!(!device_matches("usb-????", "usb-00000"));
    }

    #[test]
    fn stars_can_share_characters() {
        // The classic backtracking case: the first star must give characters
        // back so the rest of the pattern can match.
        assert!(device_matches("*a*b", "aXXbYYb"));
        assert!(device_matches("*ab*ab", "abXab"));
        assert!(!device_matches("*ab*ab", "abXa"));
    }

    #[test]
    fn empty_pattern_matches_only_an_empty_name() {
        assert!(device_matches("", ""));
        assert!(!device_matches("", "x"));
        assert!(device_matches("**", ""));
    }
}
