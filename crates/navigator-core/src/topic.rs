//! MQTT 5 topic filter matching and filter checks. Pure functions, no state.

/// Largest filter or topic length that MQTT allows, in bytes.
pub const MAX_LEN: usize = 65_535;

/// True if `filter` is a valid MQTT 5 topic filter.
///
/// - It has 1 to 65 535 bytes and no NUL.
/// - `+` is a whole level. `#` is a whole level and the last level.
/// - `$share/<group>/<filter>` needs a group without wildcards and a valid inner filter.
pub fn is_valid_filter(filter: &str) -> bool {
    if filter.is_empty() || filter.len() > MAX_LEN || filter.contains('\0') {
        return false;
    }
    let filter = match shared_inner(filter) {
        Some(Some(inner)) => inner,
        Some(None) => return false,
        None => filter,
    };
    let mut levels = filter.split('/').peekable();
    while let Some(level) = levels.next() {
        let wildcard = level.contains(['+', '#']);
        if wildcard && level.len() != 1 {
            return false;
        }
        if level == "#" && levels.peek().is_some() {
            return false;
        }
    }
    true
}

/// For a `$share/` filter: `Some(Some(inner))` if the group is valid, `Some(None)` if not.
/// `None` if the filter is not a shared subscription.
fn shared_inner(filter: &str) -> Option<Option<&str>> {
    let rest = filter.strip_prefix("$share/")?;
    Some(match rest.split_once('/') {
        Some((group, inner))
            if !group.is_empty() && !inner.is_empty() && !group.contains(['+', '#']) =>
        {
            Some(inner)
        }
        _ => None,
    })
}

/// True if `topic` matches `filter` (MQTT 5, section 4.7).
///
/// - `+` matches exactly one level (the level can be empty).
/// - `#` matches the parent level and all levels below it, so `a/#` matches `a`.
/// - A topic that starts with `$` is not matched by a filter that starts with `+` or `#`.
/// - `$share/<group>/<filter>` matches like `<filter>`. The `$` rule applies to the inner filter.
/// - An invalid filter matches nothing.
pub fn matches(filter: &str, topic: &str) -> bool {
    if !is_valid_filter(filter) {
        return false;
    }
    let filter = shared_inner(filter).flatten().unwrap_or(filter);
    if topic.starts_with('$') && filter.starts_with(['+', '#']) {
        return false;
    }
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(fl), Some(tl)) if fl == tl => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_table() {
        let yes = [
            ("a/b", "a/b"),
            ("#", "a"),
            ("#", "a/b/c"),
            ("#", "/a"),
            ("a/#", "a"),
            ("a/#", "a/b/c"),
            ("a/+", "a/b"),
            ("a/+", "a/"),
            ("+/+", "/finance"),
            ("+", ""),
            ("+/b", "a/b"),
            ("a/+/c", "a/b/c"),
            ("a//b", "a//b"),
            ("a/+/b", "a//b"),
            ("/a", "/a"),
            ("$SYS/#", "$SYS/broker/uptime"),
            ("$SYS/+/uptime", "$SYS/broker/uptime"),
            ("+/x", "a/x"),
            ("a/b/#", "a/b"),
            ("A/b", "A/b"),
            ("$share/g/a/b", "a/b"),
            ("$share/g/#", "a/b"),
            ("$share/g/+/b", "a/b"),
            ("$share/g/$SYS/#", "$SYS/x"),
        ];
        for (f, t) in yes {
            assert!(matches(f, t), "{f} should match {t}");
        }
        let no = [
            ("a/b", "a"),
            ("a", "a/b"),
            ("a/+", "a"),
            ("a/+", "a/b/c"),
            ("+", "a/b"),
            ("a/b", "A/b"),
            ("#", "$SYS/x"),
            ("+/broker", "$SYS/broker"),
            ("+/#", "$SYS/broker"),
            ("a/#", "b/a"),
            ("a/#/b", "a/x/b"),
            ("a+", "ab"),
            ("a/b#", "a/bc"),
            ("", ""),
            ("a//b", "a/b"),
            ("a/b/", "a/b"),
            ("a/b", "a/b/"),
            ("$share/g/a/b", "a/c"),
            ("$share/g/#", "$SYS/x"),
            ("$share/g/+/x", "$SYS/x"),
            ("$share/g", "g"),
            ("$share/g/", ""),
            ("$share//a", "a"),
            ("$share/+/a", "a"),
        ];
        for (f, t) in no {
            assert!(!matches(f, t), "{f} should not match {t}");
        }
    }

    #[test]
    fn filter_validity() {
        for ok in [
            "#",
            "+",
            "a/#",
            "a/+/b",
            "/",
            "a//b",
            "$SYS/#",
            "+/+",
            "$share/g/a",
            "$share/g/#",
            "$share/g/+/b",
        ] {
            assert!(is_valid_filter(ok), "{ok}");
        }
        for bad in [
            "",
            "a/#/b",
            "a#",
            "a+/b",
            "#/a",
            "a/b+",
            "a\0b",
            "$share/g",
            "$share/g/",
            "$share//a",
            "$share/g+/a",
            "$share/#/a",
            "$share/g/a#",
            "$share/g/#/b",
        ] {
            assert!(!is_valid_filter(bad), "{bad}");
        }
        assert!(!is_valid_filter(&"a".repeat(MAX_LEN + 1)));
        assert!(is_valid_filter(&"a".repeat(MAX_LEN)));
    }
}
