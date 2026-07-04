/// Minimal glob matching for select/unselect groups: `*` matches any
/// sequence (including empty), `?` matches exactly one character.
/// Case-sensitive, like MC.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None; // (pattern idx after '*', name idx it consumed up to)
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi + 1, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            // backtrack: let the last '*' swallow one more character
            star = Some((sp, sn + 1));
            pi = sp;
            ni = sn + 1;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn literals_and_wildcards() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.rs.bak"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*.tar.*", "x.tar.gz"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(glob_match("readme", "readme"));
        assert!(!glob_match("readme", "README"));
        assert!(!glob_match("", "x"));
        assert!(glob_match("", ""));
    }
}
