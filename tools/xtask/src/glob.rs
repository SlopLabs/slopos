//! Shell-style patterns, as the gate allowlists under `scripts/gates/` have
//! always been written: `*`, `?`, `[...]` with ranges and `!`/`^` negation,
//! and `\` escaping the next character. Matched against the whole text.

pub fn matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some((pi, ti));
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                '[' => {
                    if let Some((hit, next)) = class(&p, pi, t[ti])
                        && hit
                    {
                        pi = next;
                        ti += 1;
                        continue;
                    }
                    if class(&p, pi, t[ti]).is_none() && t[ti] == '[' {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
                '\\' if pi + 1 < p.len() => {
                    if p[pi + 1] == t[ti] {
                        pi += 2;
                        ti += 1;
                        continue;
                    }
                }
                c if c == t[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((sp, st)) => {
                pi = sp + 1;
                ti = st + 1;
                star = Some((sp, st + 1));
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// A bracket expression at `p[start]`: whether `c` is in it and where the
/// pattern resumes, or `None` when the `[` opens no closed class.
fn class(p: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = i < p.len() && (p[i] == '!' || p[i] == '^');
    if negate {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((hit != negate, i + 1));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            hit |= p[i] <= c && c <= p[i + 2];
            i += 3;
        } else {
            hit |= p[i] == c;
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn stars_and_questions() {
        assert!(matches("*", ""));
        assert!(matches("*find_fde*", "_ZN9unwinding8find_fde17h"));
        assert!(matches("_*start", "_start"));
        assert!(matches("_*start", "_kernel_start"));
        assert!(!matches("_*start", "start"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(matches("*a*b*c*", "xxaxxbxxcxx"));
        assert!(!matches("*a*b*c*", "xxcxxbxxaxx"));
    }

    #[test]
    fn classes_ranges_negation_and_escapes() {
        assert!(matches("x[0-9]y", "x5y"));
        assert!(!matches("x[0-9]y", "xay"));
        assert!(matches("x[!0-9]y", "xay"));
        assert!(matches("x[]]y", "x]y"));
        assert!(matches("a\\*b", "a*b"));
        assert!(!matches("a\\*b", "axb"));
        assert!(matches("[unclosed", "[unclosed"));
    }
}
