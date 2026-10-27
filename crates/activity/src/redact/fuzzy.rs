//! Approximate keyword matching, so a misspelled keyword (`PASSWROD`,
//! `AUHT`) still marks what follows it as a secret.

/// Edit distance between `a` and `b`, ignoring ASCII case: insertions,
/// deletions, substitutions and swaps of two adjacent characters each
/// count as one edit (optimal string alignment distance).
pub(crate) fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().map(|c| c.to_ascii_uppercase()).collect();
    let b: Vec<char> = b.chars().map(|c| c.to_ascii_uppercase()).collect();
    let width = b.len() + 1;
    let mut d = vec![0usize; (a.len() + 1) * width];
    for (i, row) in d.chunks_mut(width).enumerate() {
        row[0] = i;
    }
    for (j, cell) in d.iter_mut().take(width).enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[(i - 1) * width + j] + 1)
                .min(d[i * width + j - 1] + 1)
                .min(d[(i - 1) * width + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[(i - 2) * width + j - 2] + 1);
            }
            d[i * width + j] = best;
        }
    }
    d[a.len() * width + b.len()]
}

/// True when `word` is at most `max` edits away from `target`.
pub(crate) fn within(word: &str, target: &str, max: usize) -> bool {
    word.chars().count().abs_diff(target.chars().count()) <= max && distance(word, target) <= max
}

/// True for words that are, or look like a misspelling of, `PASSWORD` or
/// `PASSWD` (`PASWORD`, `PASSWROD`, `OLD_PASSWORD`, ...).
pub(crate) fn like_password(word: &str) -> bool {
    let upper = word.to_ascii_uppercase();
    upper.contains("PASSW")
        || upper.contains("PASWD")
        || within(&upper, "PASSWORD", 2)
        || within(&upper, "PASSWD", 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(distance("PASSWORD", "password"), 0);
        assert_eq!(distance("PASWORD", "PASSWORD"), 1);
        assert_eq!(distance("PASSWROD", "PASSWORD"), 1);
        assert_eq!(distance("AUHT", "AUTH"), 1);
        assert_eq!(distance("", "ACL"), 3);
        assert!(within("SETUSR", "SETUSER", 2));
        assert!(!within("GET", "AUTH", 2));
    }

    #[test]
    fn password_lookalikes() {
        for word in [
            "password",
            "PASWORD",
            "passwrod",
            "PASSWD",
            "old_password",
            "pasword",
        ] {
            assert!(like_password(word), "{word}");
        }
        for word in ["pass", "word", "search_path", "user", "LOGIN"] {
            assert!(!like_password(word), "{word}");
        }
    }
}
