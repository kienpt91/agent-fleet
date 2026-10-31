//! Token estimation and model profiles.
//!
//! The estimator is a pure function of a byte count:
//!
//! ```text
//! tokens(bytes) = ceil(bytes * safety_percent / (chars_per_token * 100))
//! ```
//!
//! With the defaults (4 characters per token, 125 percent) a text of 4000 bytes counts as 1250
//! tokens. It counts UTF-8 *bytes*, never fewer than characters, so text in scripts that need
//! several bytes per character is estimated high rather than low. It is an estimate: real tokenizers
//! differ by model, and the safety factor exists so that the estimate errs on the large side. It is
//! monotone and sub-additive (`tokens(a + b) <= tokens(a) + tokens(b)`), so summing the estimates
//! of the parts never undercounts the whole.

use crate::error::ContextError;
use crate::layer::LayerId;
use crate::limits::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimator {
    chars_per_token: u32,
    safety_percent: u32,
}

impl Estimator {
    pub fn new(chars_per_token: u32, safety_percent: u32) -> Result<Estimator, ContextError> {
        if !(MIN_CHARS_PER_TOKEN..=MAX_CHARS_PER_TOKEN).contains(&chars_per_token) {
            return Err(ContextError::InvalidProfile(format!(
                "chars_per_token must be between {MIN_CHARS_PER_TOKEN} and {MAX_CHARS_PER_TOKEN}"
            )));
        }
        if !(MIN_SAFETY_PERCENT..=MAX_SAFETY_PERCENT).contains(&safety_percent) {
            return Err(ContextError::InvalidProfile(format!(
                "safety_percent must be between {MIN_SAFETY_PERCENT} and {MAX_SAFETY_PERCENT}"
            )));
        }
        Ok(Estimator {
            chars_per_token,
            safety_percent,
        })
    }

    pub fn chars_per_token(&self) -> u32 {
        self.chars_per_token
    }

    pub fn safety_percent(&self) -> u32 {
        self.safety_percent
    }

    /// Estimated tokens of `bytes` bytes of text.
    pub fn tokens(&self, bytes: usize) -> u64 {
        let numerator = (bytes as u64).saturating_mul(u64::from(self.safety_percent));
        let denominator = u64::from(self.chars_per_token) * 100;
        numerator.div_ceil(denominator)
    }

    pub fn tokens_of(&self, text: &str) -> u64 {
        self.tokens(text.len())
    }
}

impl Default for Estimator {
    fn default() -> Estimator {
        Estimator {
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            safety_percent: DEFAULT_SAFETY_PERCENT,
        }
    }
}

/// How much context a model gets and how its size is estimated: `[context.profile.<name>]` of the
/// configuration file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    name: String,
    max_tokens: u32,
    estimator: Estimator,
}

impl Profile {
    pub fn new(
        name: &str,
        max_tokens: u32,
        chars_per_token: u32,
        safety_percent: u32,
    ) -> Result<Profile, ContextError> {
        if name.is_empty()
            || name.len() > MAX_PROFILE_NAME
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(ContextError::InvalidProfile(format!(
                "a profile name is 1 to {MAX_PROFILE_NAME} characters of a-z, 0-9, `-` and `_`"
            )));
        }
        if !(MIN_MAX_TOKENS..=MAX_MAX_TOKENS).contains(&max_tokens) {
            return Err(ContextError::InvalidProfile(format!(
                "max_tokens of profile {name} must be between {MIN_MAX_TOKENS} and {MAX_MAX_TOKENS}"
            )));
        }
        Ok(Profile {
            name: name.to_string(),
            max_tokens,
            estimator: Estimator::new(chars_per_token, safety_percent)?,
        })
    }

    /// The profile used when nothing else is configured.
    pub fn standard() -> Profile {
        Profile {
            name: "default".to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            estimator: Estimator::default(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    pub fn estimator(&self) -> Estimator {
        self.estimator
    }

    /// The hard budget of one layer in tokens.
    pub fn layer_budget(&self, layer: LayerId) -> u64 {
        let percent = LAYER_PERCENT
            .get(usize::from(layer.number()) - 1)
            .copied()
            .unwrap_or(0);
        u64::from(self.max_tokens) * u64::from(percent) / 100
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_formula_is_the_documented_one() {
        let e = Estimator::default();
        assert_eq!(e.tokens(0), 0);
        assert_eq!(e.tokens(1), 1);
        assert_eq!(
            e.tokens(4),
            2,
            "4 bytes at 4 per token is 1, times 1.25 rounds up"
        );
        assert_eq!(e.tokens(4000), 1250);
        assert_eq!(e.tokens_of("héllo"), e.tokens(6), "bytes, not characters");
        let tight = Estimator::new(2, 100).unwrap();
        assert_eq!(tight.tokens(10), 5);
    }

    #[test]
    fn the_estimator_is_monotone_and_never_undercounts_parts() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2000 {
            let e = Estimator::new((next() % 16) as u32 + 1, (next() % 301) as u32 + 100).unwrap();
            let a = (next() % 100_000) as usize;
            let b = (next() % 100_000) as usize;
            assert!(e.tokens(a.min(b)) <= e.tokens(a.max(b)));
            assert!(e.tokens(a + b) <= e.tokens(a) + e.tokens(b));
        }
    }

    #[test]
    fn estimator_ranges_are_enforced() {
        assert!(Estimator::new(0, 125).is_err());
        assert!(Estimator::new(17, 125).is_err());
        assert!(Estimator::new(4, 99).is_err());
        assert!(Estimator::new(4, 401).is_err());
        assert!(Estimator::new(1, 100).is_ok());
        assert!(Estimator::new(16, 400).is_ok());
    }

    #[test]
    fn profiles_validate_their_name_and_budget() {
        assert!(Profile::new("small-8k_v2", 8192, 4, 125).is_ok());
        for bad in ["", "Big", "a b", "a/b", &"x".repeat(41)] {
            assert!(Profile::new(bad, 8192, 4, 125).is_err(), "{bad:?}");
        }
        assert!(Profile::new("p", MIN_MAX_TOKENS - 1, 4, 125).is_err());
        assert!(Profile::new("p", MAX_MAX_TOKENS + 1, 4, 125).is_err());
        assert!(Profile::new("p", MIN_MAX_TOKENS, 0, 125).is_err());
    }

    #[test]
    fn layer_budgets_follow_the_documented_percentages() {
        let p = Profile::new("p", 10_000, 4, 125).unwrap();
        let budgets: Vec<u64> = LayerId::ALL.iter().map(|l| p.layer_budget(*l)).collect();
        assert_eq!(budgets, vec![1200, 1500, 3500, 5000, 1500, 3500, 1500, 500]);
        assert_eq!(Profile::standard().max_tokens(), DEFAULT_MAX_TOKENS);
        assert_eq!(Profile::standard().name(), "default");
    }
}
