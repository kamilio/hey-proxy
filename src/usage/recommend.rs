use super::{
    AccountUsage, ExpiringWindow, Recommendation, RecommendationCandidate, SCHEMA_VERSION, State,
    Window,
};
use std::cmp::Ordering;
use std::time::UNIX_EPOCH;

/// Format Unix seconds as an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_unix_iso8601(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    // Howard Hinnant civil_from_days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

/// Parse an ISO-8601 / RFC3339 timestamp, HTTP date, or Unix epoch seconds string.
pub fn parse_timestamp(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(secs) = s.parse::<u64>() {
        return (secs > 0 && secs < 253_402_300_800).then_some(secs);
    }
    if let Ok(system_time) = httpdate::parse_http_date(s) {
        return system_time
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
    }
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: u64 = s[11..13].parse().ok()?;
    let minute: u64 = s[14..16].parse().ok()?;
    let second: u64 = s[17..19].parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let mut rest = &s[19..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits = after_dot.bytes().take_while(|c| c.is_ascii_digit()).count();
        rest = &after_dot[digits..];
    }
    let rest = rest.trim();
    let offset_secs: i64 = if rest.is_empty() || rest.eq_ignore_ascii_case("Z") {
        0
    } else if matches!(rest.as_bytes().first(), Some(b'+' | b'-')) {
        let sign = if rest.starts_with('-') { -1i64 } else { 1i64 };
        let tz = &rest[1..];
        let (tz_h, tz_m) = if let Some((h, m)) = tz.split_once(':') {
            (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
        } else if tz.len() == 4 {
            (tz[0..2].parse::<i64>().ok()?, tz[2..4].parse::<i64>().ok()?)
        } else if tz.len() == 2 {
            (tz.parse::<i64>().ok()?, 0)
        } else {
            return None;
        };
        if tz_h > 23 || tz_m > 59 {
            return None;
        }
        sign * (tz_h * 3_600 + tz_m * 60)
    } else {
        return None;
    };
    let unix = days
        .checked_mul(86_400)?
        .checked_add((hour * 3_600 + minute * 60 + second) as i64)?
        .checked_sub(offset_secs)?;
    u64::try_from(unix).ok()
}

pub fn format_duration_short(seconds: u64) -> String {
    if seconds == 0 {
        return "now".into();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

fn window_remaining(window: &Window) -> Option<f64> {
    window
        .remaining_percent
        .filter(|v| v.is_finite() && *v >= 0.0)
        .or_else(|| {
            window
                .used_percent
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|u| (100.0 - u).max(0.0))
        })
}

fn window_used(window: &Window) -> Option<f64> {
    window
        .used_percent
        .filter(|v| v.is_finite() && *v >= 0.0)
        .or_else(|| {
            window
                .remaining_percent
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|r| (100.0 - r).max(0.0))
        })
}

fn to_expiring_window(window: &Window, now_unix: u64) -> ExpiringWindow {
    let reset_ts = window.resets_at.as_deref().and_then(parse_timestamp);
    ExpiringWindow {
        id: window.id.clone(),
        label: window.label.clone(),
        used_percent: window_used(window),
        remaining_percent: window_remaining(window),
        resets_at: window.resets_at.clone(),
        resets_in_seconds: reset_ts.map(|ts| ts.saturating_sub(now_unix)),
    }
}

fn is_active_session_or_fixed_window(window: &Window) -> bool {
    // A 5-hour rolling session window with 0% used hasn't started its countdown yet.
    // Weekly/fixed windows reset on a calendar schedule regardless of used_percent.
    if window.id == "five_hour" {
        window_used(window).unwrap_or(0.0) > 0.0
    } else {
        true
    }
}

fn evaluate_candidate(usage: &AccountUsage, now_unix: u64) -> RecommendationCandidate {
    let reading = &usage.reading;
    let Some(data) = &reading.data else {
        return RecommendationCandidate {
            account: usage.account.clone(),
            state: reading.state,
            available: false,
            exhausted: false,
            using_extra_usage: false,
            effective_remaining_percent: None,
            earliest_expiring_window: None,
            next_reset_at: None,
            next_reset_in_seconds: None,
            error: reading.error.clone(),
        };
    };

    let has_core = data.windows.iter().any(|w| w.group.is_none());
    let core_windows: Vec<&Window> = data
        .windows
        .iter()
        .filter(|w| !has_core || w.group.is_none())
        .collect();

    let mut exhausted_windows = Vec::new();
    let mut min_remaining: Option<f64> = None;
    for w in &core_windows {
        if let Some(rem) = window_remaining(w) {
            if rem <= 0.0 {
                exhausted_windows.push(*w);
            }
            min_remaining = Some(match min_remaining {
                Some(prev) => prev.min(rem),
                None => rem,
            });
        }
    }

    let included_exhausted = !exhausted_windows.is_empty();
    let effective_remaining_percent = if included_exhausted {
        Some(0.0)
    } else {
        min_remaining
    };

    let extra_available = data.extra_usage.as_ref().is_some_and(|extra| {
        if extra.enabled != Some(true) {
            return false;
        }
        if let Some(spend) = &extra.spend {
            if let Some(rem) = spend.remaining {
                return rem > 0.0;
            }
            if spend.limit.is_some_and(|l| l > 0.0) && spend.used.is_some_and(|u| u == 0.0) {
                return true;
            }
        }
        extra.remaining_percent.is_some_and(|rp| rp > 0.0)
    });

    if included_exhausted {
        // When exhausted, the account remains blocked until all exhausted core windows reset.
        let blocking = exhausted_windows
            .iter()
            .copied()
            .max_by_key(|w| {
                w.resets_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .unwrap_or(0)
            })
            .or_else(|| exhausted_windows.first().copied())
            .map(|w| to_expiring_window(w, now_unix));
        let next_reset_at = blocking.as_ref().and_then(|w| w.resets_at.clone());
        let next_reset_in_seconds = blocking.as_ref().and_then(|w| w.resets_in_seconds);
        let available = extra_available && matches!(reading.state, State::Ok | State::Stale);
        return RecommendationCandidate {
            account: usage.account.clone(),
            state: reading.state,
            available,
            exhausted: true,
            using_extra_usage: available,
            effective_remaining_percent: Some(0.0),
            earliest_expiring_window: blocking,
            next_reset_at,
            next_reset_in_seconds,
            error: reading.error.clone(),
        };
    }

    // Choose the active window with remaining quota (> 0%) that expires earliest.
    // Prefer actively ticking windows (e.g. active 5h session with used > 0, or fixed weekly window)
    // over idle 5h windows (where used == 0).
    let select_earliest = |only_active_timers: bool| -> Option<&Window> {
        core_windows
            .iter()
            .copied()
            .filter(|w| window_remaining(w).is_some_and(|r| r > 0.0))
            .filter(|w| !only_active_timers || is_active_session_or_fixed_window(w))
            .filter_map(|w| {
                let ts = w.resets_at.as_deref().and_then(parse_timestamp)?;
                Some((w, ts))
            })
            .min_by(|(wa, tsa), (wb, tsb)| {
                tsa.cmp(tsb).then_with(|| {
                    let ra = window_remaining(wa).unwrap_or(0.0);
                    let rb = window_remaining(wb).unwrap_or(0.0);
                    rb.partial_cmp(&ra).unwrap_or(Ordering::Equal)
                })
            })
            .map(|(w, _)| w)
    };

    let chosen_window = select_earliest(true)
        .or_else(|| select_earliest(false))
        .or_else(|| {
            core_windows
                .iter()
                .copied()
                .filter(|w| window_remaining(w).is_some_and(|r| r > 0.0))
                .min_by(|a, b| {
                    window_remaining(a)
                        .unwrap_or(100.0)
                        .partial_cmp(&window_remaining(b).unwrap_or(100.0))
                        .unwrap_or(Ordering::Equal)
                })
        })
        .map(|w| to_expiring_window(w, now_unix));

    let next_reset_at = chosen_window.as_ref().and_then(|w| w.resets_at.clone());
    let next_reset_in_seconds = chosen_window.as_ref().and_then(|w| w.resets_in_seconds);
    let available = matches!(reading.state, State::Ok | State::Stale)
        && (effective_remaining_percent.is_some_and(|r| r > 0.0) || extra_available);

    RecommendationCandidate {
        account: usage.account.clone(),
        state: reading.state,
        available,
        exhausted: false,
        using_extra_usage: false,
        effective_remaining_percent,
        earliest_expiring_window: chosen_window,
        next_reset_at,
        next_reset_in_seconds,
        error: reading.error.clone(),
    }
}

fn compare_candidates(a: &RecommendationCandidate, b: &RecommendationCandidate) -> Ordering {
    // 1. Available candidates always beat unavailable ones.
    match (a.available, b.available) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }

    if a.available && b.available {
        // 2. Included subscription quota always beats fallback extra spend.
        match (a.using_extra_usage, b.using_extra_usage) {
            (false, true) => return Ordering::Less,
            (true, false) => return Ordering::Greater,
            _ => {}
        }

        // 3. Fresh readings (`Ok`) beat `Stale` readings.
        match (a.state == State::Ok, b.state == State::Ok) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }

        // 4. Earliest expiring window wins ("use what is expiring earliest").
        match (a.next_reset_in_seconds, b.next_reset_in_seconds) {
            (Some(ta), Some(tb)) if ta != tb => return ta.cmp(&tb),
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            _ => {}
        }

        // 5. Tie-breaker: higher effective remaining percentage wins.
        let ra = a.effective_remaining_percent.unwrap_or(0.0);
        let rb = b.effective_remaining_percent.unwrap_or(0.0);
        if (ra - rb).abs() > f64::EPSILON {
            return rb.partial_cmp(&ra).unwrap_or(Ordering::Equal);
        }
    } else {
        // When both are unavailable, rank exhausted accounts by which one unblocks soonest.
        match (a.exhausted, b.exhausted) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        match (a.next_reset_in_seconds, b.next_reset_in_seconds) {
            (Some(ta), Some(tb)) if ta != tb => return ta.cmp(&tb),
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            _ => {}
        }
    }

    a.account
        .provider
        .cmp(&b.account.provider)
        .then_with(|| a.account.id.cmp(&b.account.id))
}

/// Evaluate subscription readings across providers (`codex`, `claude`) and return the smart recommendation.
pub fn recommend(usages: &[AccountUsage], now_unix: u64) -> Recommendation {
    let mut candidates: Vec<RecommendationCandidate> = usages
        .iter()
        .map(|u| evaluate_candidate(u, now_unix))
        .collect();
    candidates.sort_by(compare_candidates);

    let Some(best) = candidates.first().filter(|c| c.available) else {
        let any_exhausted = candidates.iter().any(|c| c.exhausted);
        let (reason, summary) = if candidates.is_empty() {
            (
                "no_accounts".to_owned(),
                "No subscription accounts are configured (`hey-proxy codex-login` or `hey-proxy claude-login`).".to_owned(),
            )
        } else if any_exhausted {
            let unblock_note = candidates
                .iter()
                .filter(|c| c.exhausted)
                .filter_map(|c| {
                    c.next_reset_in_seconds.map(|secs| {
                        format!(
                            "{}/{} resets in {}",
                            c.account.provider,
                            c.account.id,
                            format_duration_short(secs)
                        )
                    })
                })
                .next()
                .unwrap_or_else(|| "waiting for quota reset".into());
            (
                "all_exhausted".to_owned(),
                format!("All configured subscription providers are out of quota ({unblock_note})."),
            )
        } else {
            (
                "no_available_provider".to_owned(),
                "No subscription provider returned a usable quota reading.".to_owned(),
            )
        };
        return Recommendation {
            schema_version: SCHEMA_VERSION,
            recommended_provider: None,
            recommended_account: None,
            reason,
            summary,
            candidates,
        };
    };

    let second = candidates.get(1);
    let (reason, summary) = if best.using_extra_usage {
        (
            "extra_usage_fallback".to_owned(),
            format!(
                "Recommended {} ({}/{}) via enabled extra usage because all included subscription quotas are exhausted.",
                best.account.provider, best.account.provider, best.account.id
            ),
        )
    } else if let Some(other) = second {
        if other.exhausted && !other.available {
            let rem = best
                .effective_remaining_percent
                .map(|r| format!("{r:.1}% left"))
                .unwrap_or_else(|| "available".into());
            let other_reset = other
                .next_reset_in_seconds
                .map(|s| format!(", resets in {}", format_duration_short(s)))
                .unwrap_or_default();
            (
                "other_exhausted".to_owned(),
                format!(
                    "Recommended {} ({rem}); {} is completely out of quota (0.0% left{other_reset}).",
                    best.account.provider, other.account.provider
                ),
            )
        } else if !other.available {
            let rem = best
                .effective_remaining_percent
                .map(|r| format!("{r:.1}% left"))
                .unwrap_or_else(|| "available".into());
            (
                "only_available_provider".to_owned(),
                format!(
                    "Recommended {} ({rem}); {} is currently unavailable ({:?}).",
                    best.account.provider, other.account.provider, other.state
                ),
            )
        } else if let (Some(best_in), Some(other_in)) =
            (best.next_reset_in_seconds, other.next_reset_in_seconds)
            && best_in < other_in
        {
            let win_label = best
                .earliest_expiring_window
                .as_ref()
                .map(|w| w.label.as_str())
                .unwrap_or("quota window");
            let best_rem = best
                .effective_remaining_percent
                .map(|r| format!("{r:.1}% left"))
                .unwrap_or_else(|| "available".into());
            let other_rem = other
                .effective_remaining_percent
                .map(|r| format!("{r:.1}% left"))
                .unwrap_or_else(|| "available".into());
            (
                "earliest_expiring_window".to_owned(),
                format!(
                    "Recommended {}: {win_label} expires earliest in {} ({best_rem}) vs {} in {} ({other_rem}).",
                    best.account.provider,
                    format_duration_short(best_in),
                    other.account.provider,
                    format_duration_short(other_in)
                ),
            )
        } else {
            let best_rem = best
                .effective_remaining_percent
                .map(|r| format!("{r:.1}% left"))
                .unwrap_or_else(|| "available".into());
            (
                "higher_remaining_quota".to_owned(),
                format!("Recommended {} ({best_rem}).", best.account.provider),
            )
        }
    } else {
        let best_rem = best
            .effective_remaining_percent
            .map(|r| format!("{r:.1}% left"))
            .unwrap_or_else(|| "available".into());
        let reset = best
            .next_reset_in_seconds
            .map(|s| format!(", expires in {}", format_duration_short(s)))
            .unwrap_or_default();
        (
            "only_available_provider".to_owned(),
            format!("Recommended {} ({best_rem}{reset}).", best.account.provider),
        )
    };

    Recommendation {
        schema_version: SCHEMA_VERSION,
        recommended_provider: Some(best.account.provider.clone()),
        recommended_account: Some(best.account.id.clone()),
        reason,
        summary,
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Account, ExtraUsage, Reading, SpendLimit, UsageData};

    fn sample_usage(
        provider: &str,
        five_hour_used: f64,
        five_hour_reset: Option<&str>,
        seven_day_used: f64,
        seven_day_reset: Option<&str>,
    ) -> AccountUsage {
        AccountUsage {
            schema_version: SCHEMA_VERSION,
            account: Account {
                provider: provider.into(),
                id: "default".into(),
            },
            reading: Reading {
                state: State::Ok,
                updated_at: Some(1_790_800_000),
                data: Some(UsageData {
                    windows: vec![
                        Window {
                            id: "five_hour".into(),
                            label: "Session · 5 hours".into(),
                            group: None,
                            used_percent: Some(five_hour_used),
                            remaining_percent: Some((100.0 - five_hour_used).max(0.0)),
                            resets_at: five_hour_reset.map(str::to_owned),
                        },
                        Window {
                            id: "seven_day".into(),
                            label: "Weekly · all models".into(),
                            group: None,
                            used_percent: Some(seven_day_used),
                            remaining_percent: Some((100.0 - seven_day_used).max(0.0)),
                            resets_at: seven_day_reset.map(str::to_owned),
                        },
                    ],
                    extra_usage: None,
                }),
                error: None,
                retry_after_seconds: Some(60),
            },
        }
    }

    #[test]
    fn iso8601_formatting_and_parsing_round_trip() {
        let ts = 1_790_803_600u64;
        let iso = format_unix_iso8601(ts);
        assert_eq!(parse_timestamp(&iso), Some(ts));
        assert_eq!(
            parse_timestamp("2026-10-02T18:00:00.123+02:00"),
            Some(parse_timestamp("2026-10-02T16:00:00Z").unwrap())
        );
    }

    #[test]
    fn recommends_provider_with_earliest_expiring_usage() {
        let now = 1_790_800_000u64;
        let claude = sample_usage(
            "claude",
            20.0,
            Some(&format_unix_iso8601(now + 10_800)), // 3h
            30.0,
            Some(&format_unix_iso8601(now + 400_000)),
        );
        let codex = sample_usage(
            "codex",
            35.0,
            Some(&format_unix_iso8601(now + 3_600)), // 1h (expires earlier!)
            25.0,
            Some(&format_unix_iso8601(now + 500_000)),
        );
        let rec = recommend(&[claude, codex], now);
        assert_eq!(rec.recommended_provider.as_deref(), Some("codex"));
        assert_eq!(rec.reason, "earliest_expiring_window");
        assert!(rec.summary.contains("codex"));
    }

    #[test]
    fn skips_exhausted_provider_even_when_its_window_resets_earlier() {
        let now = 1_790_800_000u64;
        // Codex resets in 10 minutes, but is 100% used (0% left - out completely!).
        let codex_out = sample_usage(
            "codex",
            100.0,
            Some(&format_unix_iso8601(now + 600)),
            40.0,
            Some(&format_unix_iso8601(now + 500_000)),
        );
        let claude_ok = sample_usage(
            "claude",
            30.0,
            Some(&format_unix_iso8601(now + 14_400)),
            50.0,
            Some(&format_unix_iso8601(now + 400_000)),
        );
        let rec = recommend(&[codex_out, claude_ok], now);
        assert_eq!(rec.recommended_provider.as_deref(), Some("claude"));
        assert_eq!(rec.reason, "other_exhausted");
        assert!(rec.candidates[1].exhausted);
        assert_eq!(rec.candidates[1].account.provider, "codex");

        // Also when weekly (seven_day) is 100% used even if 5-hour has 80% left!
        let claude_weekly_out = sample_usage(
            "claude",
            20.0,
            Some(&format_unix_iso8601(now + 900)),
            100.0,
            Some(&format_unix_iso8601(now + 86_400)),
        );
        let codex_ok = sample_usage(
            "codex",
            40.0,
            Some(&format_unix_iso8601(now + 7_200)),
            50.0,
            Some(&format_unix_iso8601(now + 300_000)),
        );
        let rec2 = recommend(&[claude_weekly_out, codex_ok], now);
        assert_eq!(rec2.recommended_provider.as_deref(), Some("codex"));
        assert_eq!(rec2.reason, "other_exhausted");
    }

    #[test]
    fn idle_five_hour_windows_compare_weekly_expiration_fairly() {
        let now = 1_790_800_000u64;
        // Both have 0% used in 5h session, codex reports a rolling 5h reset timestamp,
        // while claude's weekly window expires in 12 hours vs codex's 5 days.
        let codex = sample_usage(
            "codex",
            0.0,
            Some(&format_unix_iso8601(now + 18_000)),
            20.0,
            Some(&format_unix_iso8601(now + 5 * 86_400)),
        );
        let claude = sample_usage(
            "claude",
            0.0,
            None,
            40.0,
            Some(&format_unix_iso8601(now + 12 * 3_600)),
        );
        let rec = recommend(&[codex, claude], now);
        assert_eq!(rec.recommended_provider.as_deref(), Some("claude"));
        assert_eq!(rec.reason, "earliest_expiring_window");
    }

    #[test]
    fn falls_back_to_extra_usage_only_when_all_included_quotas_are_out() {
        let now = 1_790_800_000u64;
        let mut claude = sample_usage(
            "claude",
            100.0,
            Some(&format_unix_iso8601(now + 3_600)),
            100.0,
            Some(&format_unix_iso8601(now + 86_400)),
        );
        claude.reading.data.as_mut().unwrap().extra_usage = Some(ExtraUsage {
            enabled: Some(true),
            used_percent: Some(25.0),
            remaining_percent: Some(75.0),
            spend: Some(SpendLimit {
                currency: "USD".into(),
                period: "monthly".into(),
                used: Some(5.0),
                limit: Some(20.0),
                remaining: Some(15.0),
                over_limit: Some(0.0),
                resets_at: None,
            }),
        });
        let codex = sample_usage(
            "codex",
            100.0,
            Some(&format_unix_iso8601(now + 1_800)),
            80.0,
            Some(&format_unix_iso8601(now + 100_000)),
        );
        let rec = recommend(&[codex, claude], now);
        assert_eq!(rec.recommended_provider.as_deref(), Some("claude"));
        assert_eq!(rec.reason, "extra_usage_fallback");
    }
}
