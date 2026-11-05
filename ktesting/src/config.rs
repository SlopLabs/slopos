use slopos_ostd::KVec;

const DEFAULT_ENABLED: bool = false;
const DEFAULT_VERBOSITY: Verbosity = Verbosity::Summary;
const DEFAULT_WARN_MS: u32 = 0;
const DEFAULT_SHUTDOWN: bool = false;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Summary,
    Verbose,
}

impl Verbosity {
    pub fn from_str(value: &str) -> Self {
        if value.eq_ignore_ascii_case("quiet") {
            Verbosity::Quiet
        } else if value.eq_ignore_ascii_case("verbose") {
            Verbosity::Verbose
        } else {
            Verbosity::Summary
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Verbosity::Quiet => "quiet",
            Verbosity::Summary => "summary",
            Verbosity::Verbose => "verbose",
        }
    }
}

impl core::fmt::Display for Verbosity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Which slice of the selected tests this boot runs, from `tests.shard=K/N`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Shard {
    #[default]
    Whole,
    /// The `index`-th (1-based) of `count` slices, `1 <= index <= count`.
    Part { index: u32, count: u32 },
    /// `tests.shard=` was present but not `K/N` with `1 <= K <= N`; every
    /// phase fails rather than silently running everything or nothing.
    Invalid,
}

/// Test-harness runtime configuration. Globs are stored owned so the parsing
/// path can synthesise patterns without juggling lifetimes.
#[derive(Clone, Debug, Default)]
pub struct TestConfig {
    pub enabled: bool,
    pub verbosity: Verbosity,
    pub warn_ms: u32,
    pub shutdown: bool,
    pub run_globs: KVec<KVec<u8>>,
    pub skip_globs: KVec<KVec<u8>>,
    pub shard: Shard,
}

impl Default for Verbosity {
    fn default() -> Self {
        DEFAULT_VERBOSITY
    }
}

fn parse_shard(value: &str) -> Shard {
    let Some((index, count)) = value.split_once('/') else {
        return Shard::Invalid;
    };
    match (index.parse::<u32>(), count.parse::<u32>()) {
        (Ok(index), Ok(count)) if index >= 1 && index <= count => Shard::Part { index, count },
        _ => Shard::Invalid,
    }
}

impl TestConfig {
    pub fn passes_filter(&self, name: &[u8], flags: u32) -> bool {
        if flags & crate::FLAG_EXPLICIT != 0 {
            return self.run_globs.iter().any(|p| p.as_slice() == name);
        }
        let run_match = self.run_globs.is_empty()
            || self
                .run_globs
                .iter()
                .any(|p| crate::filter::glob_match(p.as_slice(), name));
        if !run_match {
            return false;
        }
        let skip_match = self
            .skip_globs
            .iter()
            .any(|p| crate::filter::glob_match(p.as_slice(), name));
        !skip_match
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn match_tests_prefix<'a>(token: &'a str, suffix: &'static str) -> Option<&'a str> {
    token.strip_prefix("tests.")?.strip_prefix(suffix)
}

fn push_owned_glob(target: &mut KVec<KVec<u8>>, pattern: &[u8]) {
    let mut owned = KVec::<u8>::new();
    for &b in pattern {
        if owned.push(b).is_err() {
            return;
        }
    }
    let _ = target.push(owned);
}

pub fn config_from_cmdline(cmdline: Option<&str>) -> TestConfig {
    let mut cfg = TestConfig {
        enabled: DEFAULT_ENABLED,
        verbosity: DEFAULT_VERBOSITY,
        warn_ms: DEFAULT_WARN_MS,
        shutdown: DEFAULT_SHUTDOWN,
        run_globs: KVec::new(),
        skip_globs: KVec::new(),
        shard: Shard::Whole,
    };
    if let Some(cmdline) = cmdline {
        for token in cmdline.split_whitespace() {
            if let Some(value) = token.strip_prefix("tests=") {
                if let Some(enabled) = parse_bool(value) {
                    cfg.enabled = enabled;
                    if !enabled {
                        cfg.shutdown = false;
                    }
                }
            } else if let Some(value) = match_tests_prefix(token, "verbosity=") {
                cfg.verbosity = Verbosity::from_str(value);
            } else if let Some(value) = match_tests_prefix(token, "warn_ms=") {
                if let Ok(parsed) = value.trim_end_matches("ms").parse::<u32>() {
                    cfg.warn_ms = parsed;
                }
            } else if let Some(value) = match_tests_prefix(token, "run=") {
                for piece in value.split(',') {
                    if !piece.is_empty() {
                        push_owned_glob(&mut cfg.run_globs, piece.as_bytes());
                    }
                }
            } else if let Some(value) = match_tests_prefix(token, "skip=") {
                for piece in value.split(',') {
                    if !piece.is_empty() {
                        push_owned_glob(&mut cfg.skip_globs, piece.as_bytes());
                    }
                }
            } else if let Some(value) = match_tests_prefix(token, "shutdown=") {
                if let Some(shutdown) = parse_bool(value) {
                    cfg.shutdown = shutdown;
                }
            } else if let Some(value) = match_tests_prefix(token, "shard=") {
                cfg.shard = parse_shard(value);
            }
        }
    }
    cfg
}
