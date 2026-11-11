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

/// A topic filter that was checked and cut into levels once. Use it when one filter matches
/// many topics. [`matches`] does the same work for one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    /// The filter without the `$share/<group>/` prefix.
    inner: Box<str>,
}

impl Filter {
    /// `None` if `filter` is not valid (see [`is_valid_filter`]).
    pub fn new(filter: &str) -> Option<Self> {
        inner(filter).map(|inner| Self {
            inner: inner.into(),
        })
    }

    /// The first level of the filter (after `$share/<group>/`). It is `+` or `#` for a wildcard.
    pub fn first_level(&self) -> &str {
        self.inner.split('/').next().unwrap_or_default()
    }

    /// True if the first level is `+` or `#`: the filter can match a topic with any first level.
    pub fn starts_with_wildcard(&self) -> bool {
        self.inner.starts_with(['+', '#'])
    }

    /// True if `topic` matches the filter. Same rules as [`matches`].
    pub fn matches(&self, topic: &str) -> bool {
        match_inner(&self.inner, topic)
    }
}

/// The filter without a `$share/<group>/` prefix. `None` if the filter is not valid.
fn inner(filter: &str) -> Option<&str> {
    is_valid_filter(filter).then(|| shared_inner(filter).flatten().unwrap_or(filter))
}

/// True if `topic` matches `filter` (MQTT 5, section 4.7).
///
/// - `+` matches exactly one level (the level can be empty).
/// - `#` matches the parent level and all levels below it, so `a/#` matches `a`.
/// - A topic that starts with `$` is not matched by a filter that starts with `+` or `#`.
/// - `$share/<group>/<filter>` matches like `<filter>`. The `$` rule applies to the inner filter.
/// - An invalid filter matches nothing.
pub fn matches(filter: &str, topic: &str) -> bool {
    inner(filter).is_some_and(|f| match_inner(f, topic))
}

/// `filter` is valid and has no `$share/` prefix.
fn match_inner(filter: &str, topic: &str) -> bool {
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

    /// A small deterministic generator (xorshift64), so the test needs no `rand` crate.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[(self.next() % items.len() as u64) as usize]
        }
    }

    /// The slow rule from the MQTT text, level by level, for a valid filter.
    fn brute(filter: &str, topic: &str) -> bool {
        let f: Vec<&str> = filter.split('/').collect();
        let t: Vec<&str> = topic.split('/').collect();
        if topic.starts_with('$') && matches!(f[0], "+" | "#") {
            return false;
        }
        for (i, fl) in f.iter().enumerate() {
            match *fl {
                "#" => return true,
                "+" if i < t.len() => {}
                l if i < t.len() && l == t[i] => {}
                _ => return false,
            }
        }
        f.len() == t.len()
    }

    #[test]
    fn filter_type_equals_the_function_and_brute_force() {
        let words = ["a", "b", "", "$SYS", "+", "#", "x+", "a#"];
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let build = |rng: &mut Rng, wild: bool| {
            let n = 1 + rng.next() % 4;
            let levels: Vec<&str> = (0..n)
                .map(|_| {
                    if wild {
                        rng.pick(&words)
                    } else {
                        rng.pick(&words[..4])
                    }
                })
                .collect();
            let base = levels.join("/");
            match rng.next() % 5 {
                0 => format!("$share/g/{base}"),
                1 => format!("$share/+/{base}"),
                _ => base,
            }
        };
        let (mut valid, mut hits) = (0, 0);
        for _ in 0..20_000 {
            let filter = build(&mut rng, true);
            let topic = build(&mut rng, false);
            let topic = topic.trim_start_matches("$share/g/").to_owned();
            let parsed = Filter::new(&filter);
            assert_eq!(parsed.is_some(), is_valid_filter(&filter), "{filter}");
            let got = matches(&filter, &topic);
            assert_eq!(parsed.as_ref().is_some_and(|f| f.matches(&topic)), got);
            if let Some(f) = &parsed {
                valid += 1;
                let inner = shared_inner(&filter).flatten().unwrap_or(&filter);
                assert_eq!(got, brute(inner, &topic), "{filter} {topic}");
                assert_eq!(f.first_level(), inner.split('/').next().expect("a level"));
                hits += usize::from(got);
            } else {
                assert!(!got);
            }
        }
        assert!(
            valid > 2_000 && hits > 200,
            "weak test data: {valid} {hits}"
        );
    }
}
