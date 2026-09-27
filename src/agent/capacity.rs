//! Detection of agent capacity / quota exhaustion.
//!
//! Coding-agent CLIs fail in very different ways when they cannot take more
//! work. Sometimes the account is out of quota (Codex prints a human-readable
//! "usage limit" message), sometimes the provider is overloaded or out of
//! capacity (Anthropic's `overloaded_error`, a `503 Service Unavailable`, a
//! "server is busy" note), and sometimes the CLI has hit a concurrency limit.
//! There is no shared machine-readable signal, so the dispatcher classifies a
//! failed run as *capacity-limited* by matching the output against a
//! conservative list of phrases.
//!
//! Matching is deliberately restricted to *failed* runs (see
//! [`super::command::CommandAgent`]): an agent that merely mentions "rate
//! limit" or "capacity" while editing code and still succeeds must not be
//! treated as exhausted.

use std::time::Duration;

use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

/// Built-in phrases that indicate an agent has exhausted its quota, hit a
/// rate/concurrency limit, or that its provider is temporarily overloaded.
/// Compared case-insensitively as substrings.
pub const DEFAULT_CAPACITY_MARKERS: &[&str] = &[
    // Quota / usage limit.
    "usage limit",
    "hit your limit",
    "hit your usage limit",
    "reached your limit",
    "reached your quota",
    "rate limit",
    "rate-limit",
    "rate_limit",
    "rate_limit_error",
    "ratelimit",
    "rate limited",
    "too many requests",
    "quota exceeded",
    "exceeded your quota",
    "exceeded the quota",
    "exceeded your current quota",
    "quota has been reached",
    "out of quota",
    "insufficient quota",
    "insufficient_quota",
    "weekly limit",
    "daily limit",
    "monthly limit",
    "usage cap",
    "individual quota reached",
    "5-hour limit reached",
    "limit will reset",
    "credit balance",
    "out of credits",
    "purchase more credits",
    "billing hard limit",
    "insufficient balance",
    // Provider capacity / overload.
    "capacity limit",
    "at capacity",
    "reached capacity",
    "capacity exceeded",
    "no capacity",
    "out of capacity",
    "insufficient capacity",
    "overloaded",
    "overloaded_error",
    "server is busy",
    "server busy",
    "service unavailable",
    "temporarily unavailable",
    "high demand",
    "resource exhausted",
    "resource_exhausted",
    "too many concurrent",
    "concurrency limit",
    "concurrent request",
    "concurrent session",
    "maximum concurrent",
    "session limit",
    "max sessions",
    "429 status code",
    "http 429",
    "error code: 429",
    // Generic transient "try later" hint; harmless because it is only checked
    // on a failed run.
    "try again later",
];

/// Whether `text` looks like a capacity or quota message.
///
/// `extra` lets operators add phrases for a CLI whose wording is not covered
/// by [`DEFAULT_CAPACITY_MARKERS`]; it is matched case-insensitively too.
pub fn is_capacity_limited(text: &str, extra: &[String]) -> bool {
    let lower = text.to_ascii_lowercase();
    DEFAULT_CAPACITY_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
        || extra.iter().any(|marker| {
            let marker = marker.trim().to_ascii_lowercase();
            !marker.is_empty() && lower.contains(&marker)
        })
}

/// Read an explicit retry window from a failed agent's response.
///
/// Provider messages without a usable duration or reset time leave the choice
/// to the configured fallback. Times without a timezone use the bot host's
/// local timezone, matching how interactive CLIs display local reset times.
pub fn retry_after(text: &str, now: DateTime<Utc>) -> Option<Duration> {
    let lower = text.to_ascii_lowercase();
    for cue in [
        "retry after",
        "retry-after",
        "retry_after",
        "try again after",
        "retry in",
        "try again in",
        "resets in",
        "reset in",
        "available in",
    ] {
        for (offset, _) in lower.match_indices(cue) {
            if let Some(duration) = parse_duration(
                &lower[offset + cue.len()..],
                matches!(cue, "retry after" | "retry-after" | "retry_after"),
            ) {
                return Some(duration);
            }
        }
    }
    for cue in [
        "reset_at",
        "resets at",
        "reset at",
        "retry at",
        "try again at",
    ] {
        for (offset, _) in lower.match_indices(cue) {
            let rest = text[offset + cue.len()..].trim_start_matches([' ', ':', '=', '\"']);
            let timestamp = rest
                .split(|c: char| c.is_whitespace() || c == '\"' || c == ',' || c == '}')
                .next()?;
            if let Ok(reset) = DateTime::parse_from_rfc3339(timestamp)
                && let Ok(duration) = reset.signed_duration_since(now).to_std()
                && valid_retry(duration)
            {
                return Some(duration);
            }
            if let Some(duration) = parse_local_reset(rest, now) {
                return Some(duration);
            }
        }
    }
    for cue in ["resets ", "reset "] {
        for (offset, _) in lower.match_indices(cue) {
            if let Some(duration) = parse_clock_reset(&text[offset + cue.len()..], now) {
                return Some(duration);
            }
        }
    }
    None
}

fn parse_clock(text: &str) -> Option<NaiveTime> {
    let clock = text.split_whitespace().next()?.trim_end_matches('.');
    let split = clock
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b':')
        .count();
    let (digits, suffix) = clock.split_at(split);
    let suffix = if suffix.is_empty() {
        text.split_whitespace().nth(1)?.trim_end_matches('.')
    } else {
        suffix
    };
    let (hour, minute) = match digits.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None => (digits.parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&hour) {
        return None;
    }
    let hour = match suffix.to_ascii_lowercase().as_str() {
        "am" => hour % 12,
        "pm" => hour % 12 + 12,
        _ => return None,
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

fn parse_clock_reset(text: &str, now: DateTime<Utc>) -> Option<Duration> {
    let time = parse_clock(text)?;
    let zone = text
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(name, _)| name.parse::<Tz>());
    let reset = match zone {
        Some(Ok(zone)) => {
            let local_now = now.with_timezone(&zone);
            let date = local_now.date_naive();
            let today = zone.from_local_datetime(&date.and_time(time)).single()?;
            let next = if today > local_now {
                today
            } else {
                zone.from_local_datetime(&date.succ_opt()?.and_time(time))
                    .single()?
            };
            next.to_utc()
        }
        Some(Err(_)) => return None,
        None => {
            let local_now = now.with_timezone(&Local);
            let date = local_now.date_naive();
            let today = Local.from_local_datetime(&date.and_time(time)).single()?;
            let next = if today > local_now {
                today
            } else {
                Local
                    .from_local_datetime(&date.succ_opt()?.and_time(time))
                    .single()?
            };
            next.to_utc()
        }
    };
    reset
        .signed_duration_since(now)
        .to_std()
        .ok()
        .filter(|duration| valid_retry(*duration))
}

fn parse_local_reset(text: &str, now: DateTime<Utc>) -> Option<Duration> {
    let parts: Vec<_> = text.split_whitespace().take(5).collect();
    if parse_clock(text).is_some() {
        return parse_clock_reset(text, now);
    }
    let local_now = now.with_timezone(&Local);
    let day = parts.get(1)?.trim_end_matches(',');
    let day = day
        .trim_end_matches("st")
        .trim_end_matches("nd")
        .trim_end_matches("rd")
        .trim_end_matches("th");
    let input = format!(
        "{} {} {} {} {}",
        parts[0],
        day,
        parts.get(2)?.trim_end_matches(','),
        parts.get(3)?.trim_end_matches(','),
        parts.get(4)?.trim_end_matches(['.', ','])
    );
    let naive = ["%b %d %Y %I:%M %p", "%B %d %Y %I:%M %p"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(&input, format).ok())?;
    let reset = Local.from_local_datetime(&naive).single()?;
    reset
        .signed_duration_since(local_now)
        .to_std()
        .ok()
        .filter(|duration| valid_retry(*duration))
}

fn parse_duration(text: &str, bare_seconds: bool) -> Option<Duration> {
    let mut rest = text.trim_start_matches([' ', ':', '=', '\"']);
    let mut seconds = 0_u64;
    let mut found = false;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',' || c == ':');
        if let Some(after) = rest.strip_prefix("and ") {
            rest = after;
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            break;
        }
        let value = rest[..digits].parse::<u64>().ok()?;
        if rest.as_bytes().get(digits) == Some(&b'.') {
            return None;
        }
        rest = rest[digits..].trim_start();
        let unit_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        let unit = &rest[..unit_len];
        let multiplier: u64 = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 604_800,
            "" if bare_seconds && !found => 1,
            _ => break,
        };
        seconds = seconds.checked_add(value.checked_mul(multiplier)?)?;
        found = true;
        rest = &rest[unit_len..];
    }
    let duration = Duration::from_secs(seconds);
    (found && valid_retry(duration)).then_some(duration)
}

fn valid_retry(duration: Duration) -> bool {
    // Keep malformed or implausibly distant provider hints from creating an
    // effectively permanent cooldown (or overflowing `Instant`).
    !duration.is_zero() && duration <= Duration::from_secs(366 * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-28T12:00:00Z")
            .unwrap()
            .to_utc()
    }

    #[test]
    fn real_codex_limit_response() {
        // Reported by `codex exec`: https://github.com/openai/codex/issues/38603
        let response = "ERROR: You've hit your usage limit. Try again at Sep 13th, 2026 7:11 PM.";
        let local_now = Local
            .with_ymd_and_hms(2026, 9, 13, 18, 11, 0)
            .single()
            .unwrap();
        assert!(is_capacity_limited(response, &[]));
        assert_eq!(
            retry_after(response, local_now.to_utc()),
            Some(Duration::from_secs(3_600))
        );
    }

    #[test]
    fn real_antigravity_limit_response() {
        // Antigravity CLI: https://github.com/google-antigravity/antigravity-cli/issues/789
        let response = "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 143h57m55s.";
        assert!(is_capacity_limited(response, &[]));
        assert_eq!(
            retry_after(response, now()),
            Some(Duration::from_secs(143 * 3_600 + 57 * 60 + 55))
        );
    }

    #[test]
    fn real_claude_code_limit_response() {
        // `claude -p`: https://github.com/anthropics/claude-code/issues/84690
        let response = "You've hit your session limit · resets 5am (Asia/Tokyo)";
        let now = DateTime::parse_from_rfc3339("2026-09-28T00:00:00Z")
            .unwrap()
            .to_utc();
        assert!(is_capacity_limited(response, &[]));
        assert_eq!(
            retry_after(response, now),
            Some(Duration::from_secs(20 * 3_600))
        );

        // Older Claude Code display: https://github.com/anthropics/claude-code/issues/6392
        let older = "5-hour limit reached ∙ resets 5am";
        let local_now = Local
            .with_ymd_and_hms(2026, 9, 28, 3, 0, 0)
            .single()
            .unwrap();
        assert!(is_capacity_limited(older, &[]));
        assert_eq!(
            retry_after(older, local_now.to_utc()),
            Some(Duration::from_secs(2 * 3_600))
        );
    }

    #[test]
    fn real_pi_limit_response_for_oneshot_and_rpc_backends() {
        // Pi's visible provider failure: https://github.com/earendil-works/pi/issues/1038
        // Both forge-bot Pi adapters depend on provider text, which varies by provider.
        let response = "Error: 429 status code (no body)";
        assert!(is_capacity_limited(response, &[]));
        assert_eq!(retry_after(response, now()), None);
    }

    #[test]
    fn real_kimi_limit_response() {
        // Kimi CLI: https://github.com/MoonshotAI/kimi-cli/issues/901
        let response = "LLM provider error: Error code: 429 - {'error': {'message': \"We're receiving too many requests at the moment. Please wait a moment and try again.\", 'type': 'rate_limit_reached_error'}}";
        assert!(is_capacity_limited(response, &[]));
        assert_eq!(retry_after(response, now()), None);
    }

    #[test]
    fn parses_relative_retry_hints() {
        assert_eq!(
            retry_after("Rate limit. Retry after 90 seconds", now()),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            retry_after("Usage limit. Try again in 1h 30m", now()),
            Some(Duration::from_secs(5_400))
        );
        assert_eq!(
            retry_after("Limit resets in 2 hours and 15 minutes", now()),
            Some(Duration::from_secs(8_100))
        );
        assert_eq!(
            retry_after("Rate limit. Retry after: 120", now()),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            retry_after("429 Too Many Requests; Retry-After: 120", now()),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            retry_after("Monthly usage limit. Try again in 31 days", now()),
            Some(Duration::from_secs(31 * 86_400))
        );
    }

    #[test]
    fn parses_timezone_aware_reset_hints() {
        assert_eq!(
            retry_after("Usage limit. Resets at 2026-09-28T13:00:00+00:00", now()),
            Some(Duration::from_secs(3_600))
        );
        assert_eq!(
            retry_after(
                "rate_limit_error: {\"reset_at\":\"2026-09-28T20:30:00+08:00\"}",
                now()
            ),
            Some(Duration::from_secs(1_800))
        );
    }

    #[test]
    fn parses_codex_local_reset_times() {
        let local_now = Local
            .with_ymd_and_hms(2026, 9, 28, 12, 0, 0)
            .single()
            .unwrap();
        let now = local_now.to_utc();
        assert_eq!(
            retry_after(
                "You've hit your usage limit. Try again at Sep 28th, 2026 1:30 PM.",
                now
            ),
            Some(Duration::from_secs(5_400))
        );
        assert_eq!(
            retry_after(
                "Usage limit. Try again at September 28, 2026, 1:30 PM.",
                now
            ),
            Some(Duration::from_secs(5_400))
        );
        assert_eq!(
            retry_after("Usage limit. Try again at 1:30 PM.", now),
            Some(Duration::from_secs(5_400))
        );
        let now_after = local_now.with_hour(14).unwrap().to_utc();
        assert_eq!(
            retry_after("Usage limit. Try again at 1:30 PM.", now_after),
            Some(Duration::from_secs(23 * 3_600 + 30 * 60))
        );
    }

    #[test]
    fn ignores_ambiguous_expired_and_implausible_hints() {
        assert_eq!(
            retry_after("Usage limit. Resets at 3pm (Unknown/Zone)", now()),
            None
        );
        assert_eq!(
            retry_after("Usage limit. Resets at 2026-09-28T11:00:00Z", now()),
            None
        );
        assert_eq!(
            retry_after("Rate limit. Retry after 999999999999 seconds", now()),
            None
        );
        assert_eq!(
            retry_after("Rate limit. Retry after 1.5 hours", now()),
            None
        );
        assert_eq!(retry_after("Usage limit. Try again later", now()), None);
    }

    #[test]
    fn detects_quota_and_rate_limit_messages() {
        assert!(is_capacity_limited(
            "You've hit your usage limit. Upgrade to Pro or try again later.",
            &[]
        ));
        assert!(is_capacity_limited("429 Too Many Requests", &[]));
        assert!(is_capacity_limited(
            "stream error: rate_limit_error: quota exceeded",
            &[]
        ));
        assert!(is_capacity_limited(
            "Claude AI usage limit reached. Your limit will reset at 3pm.",
            &[]
        ));
        assert!(is_capacity_limited("You exceeded your current quota", &[]));
    }

    #[test]
    fn detects_capacity_and_overload_messages() {
        assert!(is_capacity_limited(
            "The model is currently overloaded. Please try again later.",
            &[]
        ));
        assert!(is_capacity_limited("overloaded_error: Overloaded", &[]));
        assert!(is_capacity_limited("503 Service Unavailable", &[]));
        assert!(is_capacity_limited(
            "reached capacity limit for concurrent sessions",
            &[]
        ));
        assert!(is_capacity_limited(
            "you have exceeded the maximum number of concurrent requests",
            &[]
        ));
    }

    #[test]
    fn does_not_treat_pool_exhaustion_as_provider_capacity() {
        // The pooled adapter waiting for one of its own agents is a busy
        // gateway, not the provider refusing work, so it must not mark the
        // agent unavailable.
        assert!(!is_capacity_limited(
            "timed out waiting for an idle pi agent",
            &[]
        ));
        assert!(!is_capacity_limited("waiting for an idle agent", &[]));
    }

    #[test]
    fn ignores_unrelated_failures() {
        assert!(!is_capacity_limited(
            "compile error: cannot find crate",
            &[]
        ));
        assert!(!is_capacity_limited("test failed: expected 1, got 2", &[]));
        assert!(!is_capacity_limited("", &[]));
    }

    #[test]
    fn extra_markers_are_matched_case_insensitively() {
        let extra = vec!["No remaining tokens".to_owned()];
        assert!(is_capacity_limited(
            "error: no remaining tokens today",
            &extra
        ));
        assert!(!is_capacity_limited("error: something else", &extra));
        // Blank extra markers never match everything.
        assert!(!is_capacity_limited(
            "error: something else",
            &["".to_owned()]
        ));
    }
}
