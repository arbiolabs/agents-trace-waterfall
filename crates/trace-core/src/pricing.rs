//! Model list prices, for estimating the cost of a model call whose source reports tokens but no cost.
//! Estimates are marked `CostProvenance::Estimate` and never replace a cost the source reported.
//!
//! Prices are first-party API list prices in USD per million tokens. Models without a known price stay
//! unknown rather than borrowing a near match's.
// ponytail: a built-in table that a release refreshes; read overrides from the config when prices drift
// faster than releases.

use crate::Usage;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// Anthropic's cache rates: writes (5-minute) cost 1.25× input, reads 0.1× unless a model sets its own.
const fn claude(input: f64, output: f64) -> Price {
    Price { input, output, cache_read: input * 0.1, cache_write: input * 1.25 }
}

const fn with_cache_read(p: Price, cache_read: f64) -> Price {
    Price { cache_read, ..p }
}

/// OpenAI bills cached input at its own rate and has no cache-write charge.
const fn openai(input: f64, cached: f64, output: f64) -> Price {
    Price { input, output, cache_read: cached, cache_write: 0. }
}

/// Model family → price. Matched on the normalized model name, exactly or before a `-YYYYMMDD` snapshot date.
const PRICES: &[(&str, Price)] = &[
    ("claude-fable-5-1", with_cache_read(claude(10., 50.), 0.25)),
    ("claude-mythos-5-1", with_cache_read(claude(10., 50.), 0.25)),
    ("claude-fable-5", claude(10., 50.)),
    ("claude-mythos-5", claude(10., 50.)),
    ("claude-opus-5-5", with_cache_read(claude(4., 20.), 0.20)),
    ("claude-opus-5", claude(5., 25.)),
    ("claude-opus-4-8", claude(5., 25.)),
    ("claude-opus-4-7", claude(5., 25.)),
    ("claude-opus-4-6", claude(5., 25.)),
    ("claude-opus-4-5", claude(5., 25.)),
    ("claude-opus-4-1", claude(15., 75.)),
    ("claude-opus-4", claude(15., 75.)),
    ("claude-sonnet-5-5", claude(2., 10.)),
    ("claude-sonnet-5", claude(2., 10.)),
    ("claude-sonnet-4-6", claude(3., 15.)),
    ("claude-sonnet-4-5", claude(3., 15.)),
    ("claude-sonnet-4", claude(3., 15.)),
    ("claude-3-7-sonnet", claude(3., 15.)),
    ("claude-haiku-5-5", claude(0.10, 0.50)),
    ("claude-haiku-4-5", claude(1., 5.)),
    ("claude-3-5-haiku", claude(0.80, 4.)),
    ("gpt-5", openai(1.25, 0.125, 10.)),
    ("gpt-5-codex", openai(1.25, 0.125, 10.)),
    ("gpt-5-mini", openai(0.25, 0.025, 2.)),
    ("gpt-5-nano", openai(0.05, 0.005, 0.40)),
    ("gpt-4.1", openai(2., 0.50, 8.)),
    ("gpt-4.1-mini", openai(0.40, 0.10, 1.60)),
    ("gpt-4o", openai(2.50, 1.25, 10.)),
    ("gpt-4o-mini", openai(0.15, 0.075, 0.60)),
];

/// Above this many prompt tokens (fresh, cached and cache-written input together), these models bill
/// at a higher rate.
const LONG_PROMPT: &[(&str, u64, Price)] = &[("claude-haiku-5-5", 100_000, claude(0.50, 2.50))];

/// `anthropic/claude-opus-4-1-20250805[1m]`, `us.anthropic.claude-sonnet-4-5` → `claude-…`; lowercase,
/// provider and region prefixes and bracketed context tags dropped.
fn normalize(model: &str) -> String {
    let m = model.split('[').next().unwrap_or(model).trim().to_lowercase();
    let m = m.rsplit('/').next().unwrap_or(&m).to_owned();
    // Bedrock writes `us.anthropic.claude-…-v1:0`.
    let m = m.split_once("anthropic.").map_or(m.as_str(), |(_, rest)| rest);
    m.split_once("-v1").map_or(m, |(base, _)| base).trim_end_matches(":0").to_owned()
}

fn lookup<T: Copy>(table: &[(&str, T)], model: &str) -> Option<T> {
    let m = normalize(model);
    table.iter().find(|(name, _)| m == *name || m.strip_prefix(name).and_then(|rest| rest.strip_prefix('-')).is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))).map(|(_, p)| *p)
}

pub fn price(model: &str) -> Option<Price> {
    lookup(PRICES, model)
}

/// The estimated cost of `usage` on `model` in micro-USD. Prices are per million tokens, so tokens × price
/// is already micro-USD.
pub fn estimate_cost(model: &str, usage: &Usage) -> Option<u64> {
    let tokens = |t: Option<u64>| t.unwrap_or(0) as f64;
    if [usage.tokens_input, usage.tokens_output, usage.cache_read, usage.cache_write].iter().all(Option::is_none) {
        return None;
    }
    let prompt = usage.tokens_input.unwrap_or(0) + usage.cache_read.unwrap_or(0) + usage.cache_write.unwrap_or(0);
    // Zero tokens cost nothing on any model, priced or not (Claude Code's local `<synthetic>` messages).
    if prompt + usage.tokens_output.unwrap_or(0) == 0 {
        return Some(0);
    }
    let long = LONG_PROMPT.iter().find(|(name, over, _)| lookup(&[(*name, ())], model).is_some() && prompt > *over).map(|(_, _, p)| *p);
    let p = long.or_else(|| price(model))?;
    let cost = tokens(usage.tokens_input) * p.input + tokens(usage.tokens_output) * p.output + tokens(usage.cache_read) * p.cache_read + tokens(usage.cache_write) * p.cache_write;
    Some(cost.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, read: u64, write: u64) -> Usage {
        Usage { tokens_input: Some(input), tokens_output: Some(output), cache_read: Some(read), cache_write: Some(write), ..Default::default() }
    }

    #[test]
    fn names_normalize_to_their_family_and_unknown_models_stay_unknown() {
        for m in ["claude-opus-4-1-20250805", "anthropic/claude-opus-4-1", "us.anthropic.claude-opus-4-1-20250805-v1:0", "Claude-Opus-4-1[1m]"] {
            assert_eq!(price(m), Some(claude(15., 75.)), "{m}");
        }
        assert_eq!(price("claude-opus-4-5"), Some(claude(5., 25.)), "Opus 4.5 is not Opus 4");
        assert_eq!(price("gpt-5-mini"), Some(openai(0.25, 0.025, 2.)), "gpt-5-mini is not gpt-5");
        assert_eq!(price("gpt-6-luna"), None);
        assert_eq!(price("gpt-5.5"), None, "no borrowing a near match's price");
    }

    #[test]
    fn estimates_price_each_kind_of_token() {
        // 1M fresh in, 100k out, 2M cache reads, 400k cache writes on Opus 4.8: $5 + $2.50 + $1 + $2.50.
        assert_eq!(estimate_cost("claude-opus-4-8", &usage(1_000_000, 100_000, 2_000_000, 400_000)), Some(11_000_000));
        assert_eq!(estimate_cost("claude-opus-5-5", &usage(0, 0, 1_000_000, 0)), Some(200_000), "Opus 5.5 reads its cache at $0.20");
        assert_eq!(estimate_cost("gpt-5", &usage(1_000_000, 0, 1_000_000, 0)), Some(1_375_000));
        assert_eq!(estimate_cost("claude-haiku-5-5", &usage(50_000, 1_000, 0, 0)), Some(5_500));
        assert_eq!(estimate_cost("claude-haiku-5-5", &usage(150_000, 1_000, 0, 0)), Some(77_500), "long prompts bill at the higher rate");
        assert_eq!(estimate_cost("claude-opus-4-8", &Usage::default()), None, "no tokens, no estimate");
        assert_eq!(estimate_cost("unknown-model", &usage(10, 10, 0, 0)), None);
        assert_eq!(estimate_cost("<synthetic>", &usage(0, 0, 0, 0)), Some(0), "zero tokens are free on any model");
    }
}
