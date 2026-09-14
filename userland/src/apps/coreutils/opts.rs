//! `getopt(3)`'s grammar over `&[&[u8]]`: clustered flags, an attached or
//! separate option-argument, `--` ending the options, and a bare `-` left as
//! an operand. A long option is reported rather than interpreted, since only a
//! handful of tools take one.

/// One parsed element of the option list.
pub enum Opt<'a> {
    /// A short flag with no argument.
    Flag(u8),
    /// A short flag and its argument.
    Value(u8, &'a [u8]),
    /// `--name` or `--name=value`, split at the first `=`.
    Long(&'a [u8], Option<&'a [u8]>),
    /// A flag outside the spec.
    Unknown(u8),
    /// A flag in the spec whose argument is absent.
    Missing(u8),
}

/// Iterates the option part of `argv`, then hands back the operands.
///
/// `spec` is `getopt(3)`'s: a letter, followed by `:` when it takes an
/// argument. `argv[0]` is the command name and is skipped.
pub struct Opts<'a> {
    argv: &'a [&'a [u8]],
    spec: &'static str,
    /// Index into `argv` of the word being read.
    word: usize,
    /// Byte offset inside a cluster, `0` when no cluster is open.
    byte: usize,
    ended: bool,
}

impl<'a> Opts<'a> {
    pub fn new(argv: &'a [&'a [u8]], spec: &'static str) -> Self {
        Self {
            argv,
            spec,
            word: 1,
            byte: 0,
            ended: false,
        }
    }

    fn takes_value(&self, flag: u8) -> bool {
        let bytes = self.spec.as_bytes();
        match bytes.iter().position(|&b| b == flag) {
            Some(i) => bytes.get(i + 1) == Some(&b':'),
            None => false,
        }
    }

    fn known(&self, flag: u8) -> bool {
        self.spec.as_bytes().contains(&flag)
    }

    /// The operands left after the options. Valid once [`Opts::next`] has
    /// returned `None`; before that it is the not-yet-parsed tail.
    pub fn operands(&self) -> &'a [&'a [u8]] {
        &self.argv[self.word.min(self.argv.len())..]
    }
}

impl<'a> Iterator for Opts<'a> {
    type Item = Opt<'a>;

    fn next(&mut self) -> Option<Opt<'a>> {
        if self.ended {
            return None;
        }
        loop {
            let word = *self.argv.get(self.word)?;

            if self.byte == 0 {
                if word == b"--" {
                    self.word += 1;
                    self.ended = true;
                    return None;
                }
                if word.len() >= 2 && word[0] == b'-' && word[1] == b'-' {
                    self.word += 1;
                    let body = &word[2..];
                    return Some(match body.iter().position(|&b| b == b'=') {
                        Some(eq) => Opt::Long(&body[..eq], Some(&body[eq + 1..])),
                        None => Opt::Long(body, None),
                    });
                }
                if word.len() < 2 || word[0] != b'-' {
                    self.ended = true;
                    return None;
                }
                self.byte = 1;
            }

            let flag = word[self.byte];
            self.byte += 1;
            let cluster_done = self.byte >= word.len();
            if cluster_done {
                self.byte = 0;
                self.word += 1;
            }

            if !self.known(flag) {
                return Some(Opt::Unknown(flag));
            }
            if !self.takes_value(flag) {
                return Some(Opt::Flag(flag));
            }

            // Attached (`-n5`) beats separate (`-n 5`), as getopt has it.
            if !cluster_done {
                let value = &word[self.byte..];
                self.byte = 0;
                self.word += 1;
                return Some(Opt::Value(flag, value));
            }
            return match self.argv.get(self.word) {
                Some(value) => {
                    self.word += 1;
                    Some(Opt::Value(flag, value))
                }
                None => Some(Opt::Missing(flag)),
            };
        }
    }
}

/// Parse a decimal count, as an option-argument or an operand.
pub fn parse_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(value)
}

/// Parse a decimal count with an optional sign.
pub fn parse_i64(bytes: &[u8]) -> Option<i64> {
    match bytes.first() {
        Some(b'-') => parse_u64(&bytes[1..]).and_then(|v| i64::try_from(v).ok().map(|v| -v)),
        Some(b'+') => parse_u64(&bytes[1..]).and_then(|v| i64::try_from(v).ok()),
        _ => parse_u64(bytes).and_then(|v| i64::try_from(v).ok()),
    }
}

/// Parse an octal mode (`644`, `0755`), for `-m`.
pub fn parse_octal(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 6 {
        return None;
    }
    let mut value: u32 = 0;
    for &b in bytes {
        if !(b'0'..=b'7').contains(&b) {
            return None;
        }
        value = value * 8 + (b - b'0') as u32;
    }
    Some(value)
}
