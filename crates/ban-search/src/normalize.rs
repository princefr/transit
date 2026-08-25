//! French-friendly text normalization for BAN search.

/// Fold common French/Latin accents (same spirit as transit stop search).
#[allow(dead_code)]
pub fn strip_accents(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' | 'À' | 'Á' | 'Â' | 'Ä' | 'Ã' | 'Å' => out.push('a'),
            'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => out.push('e'),
            'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => out.push('i'),
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' | 'Ò' | 'Ó' | 'Ô' | 'Ö' | 'Õ' => out.push('o'),
            'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => out.push('u'),
            'ý' | 'ÿ' | 'Ý' | 'Ÿ' => out.push('y'),
            'ç' | 'Ç' => out.push('c'),
            'ñ' | 'Ñ' => out.push('n'),
            'œ' | 'Œ' => {
                out.push('o');
                out.push('e');
            }
            'æ' | 'Æ' => {
                out.push('a');
                out.push('e');
            }
            other => {
                for ch in other.to_lowercase() {
                    out.push(ch);
                }
            }
        }
    }
    out
}

/// Normalize for matching: accents, case, punctuation → spaces, collapse whitespace.
pub fn normalize_query(s: &str) -> String {
    let s = strip_accents(s);
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c);
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Expand common French street abbreviations for matching.
#[allow(dead_code)]
pub fn expand_street_abbrev(s: &str) -> String {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut out = Vec::with_capacity(tokens.len());
    for t in tokens {
        let e = match t {
            "r" | "r." => "rue",
            "av" | "av." | "ave" => "avenue",
            "bd" | "bd." | "boul" | "boul." => "boulevard",
            "pl" | "pl." => "place",
            "imp" | "imp." => "impasse",
            "all" | "all." => "allee",
            "ch" | "ch." | "che" => "chemin",
            "rte" | "rte." => "route",
            "st" => "saint",
            "ste" => "sainte",
            "fg" | "fg." => "faubourg",
            "sq" | "sq." => "square",
            "crs" => "cours",
            "qu" | "qu." => "quai",
            other => other,
        };
        out.push(e);
    }
    out.join(" ")
}

/// Parse optional leading house number + rest of query.
/// `"12 bis rue de rivoli"` → `(Some(("12", "bis")), "rue de rivoli")`
pub fn split_number_prefix(q: &str) -> (Option<(String, String)>, String) {
    let q = q.trim();
    let mut parts = q.split_whitespace();
    let Some(first) = parts.next() else {
        return (None, String::new());
    };
    if !first.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return (None, normalize_query(q));
    }
    // first is number-like (12, 12b, 12bis)
    let (num, rep_from_first) = split_num_rep(first);
    let mut rest: Vec<&str> = parts.collect();
    let mut rep = rep_from_first;
    if rep.is_empty() {
        if let Some(second) = rest.first().copied() {
            let s = second.to_ascii_lowercase();
            if matches!(
                s.as_str(),
                "bis" | "ter" | "quater" | "a" | "b" | "c" | "d" | "e" | "f"
            ) {
                rep = s;
                rest.remove(0);
            }
        }
    }
    let street = normalize_query(&rest.join(" "));
    (Some((num, rep)), street)
}

/// Bounded Levenshtein distance check: true if `a` and `b` differ by at most
/// `max_d` edits (insert / delete / substitute). Early-exits once the budget is
/// exceeded so autocomplete stays cheap.
pub fn edit_distance_leq(a: &str, b: &str, max_d: usize) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max_d {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > max_d {
            return false;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()] <= max_d
}

fn split_num_rep(tok: &str) -> (String, String) {
    let bytes = tok.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let num = tok[..i].to_string();
    let rep = tok[i..].trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
    (num, rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_number() {
        let (n, rest) = split_number_prefix("12 rue de Rivoli");
        assert_eq!(n, Some(("12".into(), "".into())));
        assert!(rest.contains("rivoli"));
    }

    #[test]
    fn expands_bd() {
        assert_eq!(expand_street_abbrev("bd haussmann"), "boulevard haussmann");
    }

    #[test]
    fn edit_distance_budget() {
        assert!(edit_distance_leq("rivolli", "rivoli", 1));
        assert!(edit_distance_leq("elysees", "elysee", 1));
        assert!(!edit_distance_leq("elysees", "rivoli", 2));
        assert!(edit_distance_leq("champs", "champ", 2));
        assert!(!edit_distance_leq("abc", "abcd", 0));
        assert!(edit_distance_leq("", "", 0));
    }
}
