//! Counting the mailbox: per-day volume, and who the mail is with.
//!
//! Everything here reads. Nothing in this module can change mailbox state, so
//! the aggregates are free to scan widely. Days are the *caller's* days: the
//! frontend hands over its current UTC offset the same way the search compiler
//! takes one, because a chart bucketed in UTC draws a calendar nobody lives in.

use std::collections::{HashMap, HashSet};

use super::validation::split_recipient_list;
use super::*;

const DAY_MS: i64 = 86_400_000;
/// Same bound, and the same reason, as `search.rs`: the frontend sends a fixed
/// offset, and no real zone sits further out than fourteen hours.
const MAX_TIMEZONE_OFFSET_MINUTES: i32 = 14 * 60;
/// The window is capped rather than the row count, so a returned series is
/// never a truncated one. Five years is already more heatmap than a screen can
/// show, and one row per day keeps the payload proportional to the window.
const MAX_STAT_DAYS: i64 = 5 * 366;
/// Ranked lists are about the handful of people you actually trade mail with.
const DEFAULT_TOP_ADDRESSES: i64 = 10;
const MAX_TOP_ADDRESSES: i64 = 25;
/// Senders group in SQL, but recipients live inside a header, so they can only
/// be counted by reading rows out. This bounds the reading...
const MAX_ADDRESS_SCAN_ROWS: i64 = 200_000;
/// ...and this bounds the ledger those rows build. A list-heavy mailbox has a
/// long tail of addresses seen exactly once; a top-ten list never needs them,
/// and an unbounded map would let one IPC call spend memory without limit.
const MAX_TRACKED_ADDRESSES: usize = 20_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailStatsInput {
    /// How many days back the window reaches, counting today. Omitted means
    /// every day there is mail for, up to `MAX_STAT_DAYS`.
    pub days: Option<i64>,
    pub account_id: Option<String>,
    #[serde(default)]
    pub timezone_offset_minutes: i32,
    pub top_limit: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayVolume {
    /// Days since the Unix epoch in the caller's zone. An integer index rather
    /// than a date string: it is what the charts do arithmetic on, and it keeps
    /// a five-year payload small.
    pub day: i64,
    pub received: i64,
    pub sent: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressCount {
    pub address: String,
    /// The display name from the most recent message, so a ranked row reads as
    /// a person. Empty when the mail only ever carried a bare address.
    pub name: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailStats {
    /// Inclusive day bounds of the window every chart draws, so the frontend
    /// can lay out a full grid without inferring the range from the rows.
    pub start_day: i64,
    pub end_day: i64,
    /// Only days that carried mail. The frontend fills the gaps; sending zeros
    /// for an empty mailbox's five years would be mostly zeros.
    pub days: Vec<DayVolume>,
    pub top_senders: Vec<AddressCount>,
    pub top_recipients: Vec<AddressCount>,
    pub received_total: i64,
    pub sent_total: i64,
    /// Set when there is older mail than the window could reach, so the screen
    /// can say "all time" honestly.
    pub truncated: bool,
}

impl MuxStore {
    pub fn mail_stats(&self, input: MailStatsInput) -> Result<MailStats, StoreError> {
        self.mail_stats_at(input, now_ms())
    }

    /// Split from `mail_stats` so the tests can hold the clock still; day
    /// bucketing is entirely a function of `now_ms` and the offset.
    fn mail_stats_at(&self, input: MailStatsInput, now_ms: i64) -> Result<MailStats, StoreError> {
        let timezone_offset_ms = validate_timezone_offset(input.timezone_offset_minutes)?;
        let account_id = exact_account_id(input.account_id.as_deref())?;
        let top_limit = input
            .top_limit
            .unwrap_or(DEFAULT_TOP_ADDRESSES)
            .clamp(1, MAX_TOP_ADDRESSES);
        let end_day = local_day(now_ms, timezone_offset_ms);
        let requested_days = match input.days {
            None => None,
            Some(days) if (1..=MAX_STAT_DAYS).contains(&days) => Some(days),
            Some(_) => {
                return Err(StoreError::Validation(format!(
                    "days must be between 1 and {MAX_STAT_DAYS}, or omitted for all time"
                )))
            }
        };

        let oldest_day = self
            .earliest_message_at(account_id.as_deref())?
            .map(|instant| local_day(instant, timezone_offset_ms));
        let earliest_reachable = end_day.saturating_sub(MAX_STAT_DAYS - 1);
        let (start_day, truncated) = match (requested_days, oldest_day) {
            (Some(days), _) => (end_day.saturating_sub(days - 1), false),
            // A store with no mail still has a window: today, drawn empty.
            (None, None) => (end_day, false),
            (None, Some(oldest)) => (oldest.max(earliest_reachable), oldest < earliest_reachable),
        };
        let start_day = start_day.min(end_day);

        // Buckets are counted as an offset from the window's own first day, so
        // the numerator SQLite divides can never be negative and its division
        // truncates toward the floor, which is what a day bucket means.
        let start_ms = day_start_ms(start_day, timezone_offset_ms);
        let end_exclusive_ms = day_start_ms(end_day.saturating_add(1), timezone_offset_ms);
        let window = Window {
            start_ms,
            end_exclusive_ms,
            account_id: account_id.clone(),
        };

        let days = self.day_volumes(&window, start_day)?;
        let received_total = days.iter().map(|day| day.received).sum();
        let sent_total = days.iter().map(|day| day.sent).sum();
        let stats = MailStats {
            start_day,
            end_day,
            days,
            top_senders: self.top_senders(&window, top_limit)?,
            top_recipients: self.top_recipients(&window, top_limit)?,
            received_total,
            sent_total,
            truncated,
        };
        // Stats are a bounded aggregate in the same size class as the activity
        // feed, so they answer to the same budget. There is no stats-specific
        // limit because the caps above already bound every list it carries.
        ensure_serialized_budget(&stats, IpcPayloadKind::Activity)?;
        Ok(stats)
    }

    fn earliest_message_at(&self, account_id: Option<&str>) -> Result<Option<i64>, StoreError> {
        let mut conditions = vec!["m.remote_deleted = 0"];
        let mut values: Vec<Value> = Vec::new();
        if let Some(account_id) = account_id {
            conditions.push("t.account_id = ?1");
            values.push(Value::Text(account_id.to_string()));
        }
        let sql = format!(
            "SELECT MIN(m.sent_at)
             FROM messages m
             JOIN threads t ON t.id = m.thread_id
             WHERE {}",
            conditions.join(" AND ")
        );
        Ok(self
            .connection
            .query_row(&sql, params_from_iter(values.iter()), |row| {
                row.get::<_, Option<i64>>(0)
            })?)
    }

    fn day_volumes(&self, window: &Window, start_day: i64) -> Result<Vec<DayVolume>, StoreError> {
        let sql = format!(
            "SELECT (m.sent_at - ?1) / {DAY_MS} AS day_offset,
                    SUM(CASE WHEN m.is_from_me = 0 THEN 1 ELSE 0 END) AS received,
                    SUM(CASE WHEN m.is_from_me = 1 THEN 1 ELSE 0 END) AS sent
             FROM messages m
             JOIN threads t ON t.id = m.thread_id
             WHERE {}
             GROUP BY day_offset
             ORDER BY day_offset",
            window.conditions()
        );
        Ok(self
            .connection
            .prepare(&sql)?
            .query_map(params_from_iter(window.values().iter()), |row| {
                Ok(DayVolume {
                    day: start_day.saturating_add(row.get::<_, i64>(0)?),
                    received: row.get(1)?,
                    sent: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Who writes to you. Received mail carries the address in its own column,
    /// so SQLite can do the grouping.
    fn top_senders(&self, window: &Window, limit: i64) -> Result<Vec<AddressCount>, StoreError> {
        let mut values = window.values();
        values.push(Value::Integer(limit));
        // The bare `sender_name` beside a single MAX() is SQLite's documented
        // way to read a column off the row that produced the maximum: here, the
        // name the sender was last seen under.
        let sql = format!(
            "SELECT LOWER(m.sender_email) AS address, m.sender_name, COUNT(*) AS volume,
                    MAX(m.sent_at)
             FROM messages m
             JOIN threads t ON t.id = m.thread_id
             WHERE {} AND m.is_from_me = 0 AND m.sender_email <> ''
             GROUP BY address
             ORDER BY volume DESC, address ASC
             LIMIT ?{}",
            window.conditions(),
            values.len()
        );
        Ok(self
            .connection
            .prepare(&sql)?
            .query_map(params_from_iter(values.iter()), |row| {
                Ok(AddressCount {
                    address: row.get(0)?,
                    name: row.get::<_, String>(1)?.trim().to_string(),
                    count: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Who you write to. `recipients` is a raw header, so this reads rows out
    /// and splits them; the To line is deliberately the whole story, because cc
    /// and bcc are copies of a message addressed to someone else.
    fn top_recipients(&self, window: &Window, limit: i64) -> Result<Vec<AddressCount>, StoreError> {
        let mut values = window.values();
        values.push(Value::Integer(MAX_ADDRESS_SCAN_ROWS));
        let sql = format!(
            "SELECT m.recipients
             FROM messages m
             JOIN threads t ON t.id = m.thread_id
             WHERE {} AND m.is_from_me = 1 AND m.recipients <> ''
             ORDER BY m.sent_at DESC, m.id DESC
             LIMIT ?{}",
            window.conditions(),
            values.len()
        );
        let mut ledger = AddressLedger::default();
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        while let Some(row) = rows.next()? {
            ledger.count_header(&row.get::<_, String>(0)?);
        }
        Ok(ledger.ranked(limit))
    }
}

/// The one place the account filter and the day bounds are spelled out, so
/// every aggregate in this module counts over exactly the same set of messages.
struct Window {
    start_ms: i64,
    end_exclusive_ms: i64,
    account_id: Option<String>,
}

impl Window {
    fn conditions(&self) -> String {
        let mut conditions = vec![
            "m.remote_deleted = 0".to_string(),
            "m.sent_at >= ?1".to_string(),
            "m.sent_at < ?2".to_string(),
        ];
        if self.account_id.is_some() {
            conditions.push("t.account_id = ?3".to_string());
        }
        conditions.join(" AND ")
    }

    fn values(&self) -> Vec<Value> {
        let mut values = vec![
            Value::Integer(self.start_ms),
            Value::Integer(self.end_exclusive_ms),
        ];
        if let Some(account_id) = &self.account_id {
            values.push(Value::Text(account_id.clone()));
        }
        values
    }
}

#[derive(Default)]
struct AddressLedger {
    counts: HashMap<String, (i64, String)>,
    full: bool,
}

impl AddressLedger {
    fn count_header(&mut self, header: &str) {
        // A stored header that does not parse is one message's worth of loss.
        // Refusing the whole aggregate over it would blank every chart on the
        // screen because one message arrived malformed years ago.
        let Ok(mailboxes) = split_recipient_list(header) else {
            return;
        };
        // A header may name the same person twice; that is still one message to
        // them, and a ranking that says otherwise is counting headers, not mail.
        let mut seen = HashSet::new();
        for mailbox in mailboxes {
            let Some((address, name)) = mailbox_address_and_name(mailbox) else {
                continue;
            };
            if !seen.insert(address.clone()) {
                continue;
            }
            if let Some(entry) = self.counts.get_mut(&address) {
                entry.0 += 1;
            } else if self.full || self.counts.len() >= MAX_TRACKED_ADDRESSES {
                // Rows arrive newest first, so a full ledger keeps counting the
                // addresses it already knows and stops admitting the tail. The
                // heavy hitters are already in it.
                self.full = true;
            } else {
                self.counts.insert(address, (1, name));
            }
        }
    }

    fn ranked(self, limit: i64) -> Vec<AddressCount> {
        let mut ranked = self
            .counts
            .into_iter()
            .map(|(address, (count, name))| AddressCount {
                address,
                name,
                count,
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.address.cmp(&right.address))
        });
        ranked.truncate(limit.max(0) as usize);
        ranked
    }
}

/// Pulls the address and display name out of one mailbox. The splitting above
/// reuses `validation::split_recipient_list`, which is the hard half and knows
/// about quoted commas; this half exists because the validator's own reader
/// answers "is this a mailbox?" and never hands the address back.
fn mailbox_address_and_name(mailbox: &str) -> Option<(String, String)> {
    let (name, address) = match (mailbox.find('<'), mailbox.rfind('>')) {
        (Some(open), Some(close)) if open < close => {
            (mailbox[..open].trim(), mailbox[open + 1..close].trim())
        }
        _ => ("", mailbox.trim()),
    };
    if address.is_empty() || !address.contains('@') {
        return None;
    }
    let name = name.trim().trim_matches('"').trim();
    Some((address.to_lowercase(), name.to_string()))
}

fn validate_timezone_offset(timezone_offset_minutes: i32) -> Result<i64, StoreError> {
    if !(-MAX_TIMEZONE_OFFSET_MINUTES..=MAX_TIMEZONE_OFFSET_MINUTES)
        .contains(&timezone_offset_minutes)
    {
        return Err(StoreError::Validation(format!(
            "Timezone offset must be between -{MAX_TIMEZONE_OFFSET_MINUTES} and {MAX_TIMEZONE_OFFSET_MINUTES} minutes"
        )));
    }
    Ok(i64::from(timezone_offset_minutes) * 60_000)
}

/// The local day an instant falls in, counted from the epoch. Euclidean
/// division so days stay whole on both sides of 1970 instead of collapsing
/// toward it, matching the local-midnight arithmetic in `search.rs`.
fn local_day(instant_ms: i64, timezone_offset_ms: i64) -> i64 {
    instant_ms
        .saturating_add(timezone_offset_ms)
        .div_euclid(DAY_MS)
}

fn day_start_ms(day: i64, timezone_offset_ms: i64) -> i64 {
    day.saturating_mul(DAY_MS)
        .saturating_sub(timezone_offset_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Midday rather than midnight, so a fixture an hour either side of "now"
    /// stays on the same day in UTC and the timezone test below is the only
    /// place a day boundary is in play.
    const NOW_MS: i64 = 1_767_268_800_000; // 2026-01-01T12:00:00Z
    const HOUR_MS: i64 = 3_600_000;

    fn store_with(rows: &str) -> (tempfile::TempDir, MuxStore) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("stats.db");
        let store = MuxStore::open(&path, false).expect("current schema created");
        store
            .connection
            .execute_batch(
                "INSERT INTO accounts(id, name, email, color, provider) VALUES
                   ('acc_a', 'A', 'a@example.test', '#111111', 'imap'),
                   ('acc_b', 'B', 'b@example.test', '#222222', 'imap');
                 INSERT INTO threads(
                   id, account_id, subject, participants, snippet, latest_at, message_count,
                   remote_in_inbox, remote_unread, remote_starred, has_attachment,
                   has_invite, has_link, has_from_me
                 ) VALUES
                   (1, 'acc_a', 'A thread', 'p', 's', 0, 1, 1, 0, 0, 0, 0, 0, 0),
                   (2, 'acc_b', 'B thread', 'p', 's', 0, 1, 1, 0, 0, 0, 0, 0, 0);",
            )
            .expect("accounts and threads");
        if !rows.is_empty() {
            store.connection.execute_batch(rows).expect("message rows");
        }
        (directory, store)
    }

    /// `sender_email`/`recipients` carry the addresses the aggregates read;
    /// every other message column is inert here.
    fn message(id: i64, thread: i64, sent_at: i64, from_me: i64, sender: &str, to: &str) -> String {
        format!(
            "INSERT INTO messages(
               id, thread_id, sender_name, sender_email, recipients, sent_at,
               body_text, is_from_me
             ) VALUES({id}, {thread}, 'Name {id}', '{sender}', '{to}', {sent_at}, 'b', {from_me});"
        )
    }

    fn stats(store: &MuxStore, input: MailStatsInput) -> MailStats {
        store.mail_stats_at(input, NOW_MS).expect("stats")
    }

    fn window(days: Option<i64>, timezone_offset_minutes: i32) -> MailStatsInput {
        MailStatsInput {
            days,
            account_id: None,
            timezone_offset_minutes,
            top_limit: None,
        }
    }

    #[test]
    fn day_buckets_follow_the_callers_timezone_rather_than_utc() {
        // 23:30 UTC the evening before "now". The same stored instant is
        // yesterday's mail in UTC and today's in a zone ninety minutes ahead.
        let late = NOW_MS - 12 * HOUR_MS - 30 * 60_000;
        let (_directory, store) =
            store_with(&message(1, 1, late, 0, "s@example.test", "me@x.test"));

        let utc = stats(&store, window(Some(30), 0));
        assert_eq!(utc.days.len(), 1);
        assert_eq!(utc.days[0].day, utc.end_day - 1);

        let ahead = stats(&store, window(Some(30), 90));
        assert_eq!(ahead.days.len(), 1);
        assert_eq!(ahead.end_day, utc.end_day);
        assert_eq!(ahead.days[0].day, ahead.end_day);
    }

    #[test]
    fn received_and_sent_split_on_is_from_me_and_skip_deleted_mail() {
        let (_directory, store) = store_with(&format!(
            "{}{}{}{}",
            message(1, 1, NOW_MS - HOUR_MS, 0, "s@example.test", "me@x.test"),
            message(2, 1, NOW_MS - 2 * HOUR_MS, 0, "s@example.test", "me@x.test"),
            message(3, 1, NOW_MS - 3 * HOUR_MS, 1, "me@x.test", "s@example.test"),
            "UPDATE messages SET remote_deleted = 1 WHERE id = 2;"
        ));
        let all = stats(&store, window(Some(30), 0));
        assert_eq!((all.received_total, all.sent_total), (1, 1));
        assert_eq!(all.days.len(), 1);
        assert_eq!((all.days[0].received, all.days[0].sent), (1, 1));
        assert!(all.top_senders.iter().all(|row| row.count == 1));
    }

    #[test]
    fn sparse_days_report_only_the_days_that_carried_mail() {
        let (_directory, store) = store_with(&format!(
            "{}{}",
            message(1, 1, NOW_MS - HOUR_MS, 0, "s@example.test", "me@x.test"),
            message(
                2,
                1,
                NOW_MS - 5 * 24 * HOUR_MS,
                0,
                "s@example.test",
                "me@x.test"
            ),
        ));
        let all = stats(&store, window(Some(30), 0));
        assert_eq!(all.end_day - all.start_day, 29);
        assert_eq!(
            all.days.iter().map(|day| day.day).collect::<Vec<_>>(),
            vec![all.end_day - 5, all.end_day]
        );
    }

    #[test]
    fn top_senders_rank_received_mail_and_fold_address_case() {
        let (_directory, store) = store_with(&format!(
            "{}{}{}{}",
            message(1, 1, NOW_MS - HOUR_MS, 0, "Loud@Example.test", "me@x.test"),
            message(
                2,
                1,
                NOW_MS - 2 * HOUR_MS,
                0,
                "loud@example.test",
                "me@x.test"
            ),
            message(
                3,
                1,
                NOW_MS - 3 * HOUR_MS,
                0,
                "quiet@example.test",
                "me@x.test"
            ),
            // Mail from me must never appear as a sender you hear from.
            message(
                4,
                1,
                NOW_MS - 4 * HOUR_MS,
                1,
                "me@x.test",
                "quiet@example.test"
            ),
        ));
        let all = stats(&store, window(Some(30), 0));
        assert_eq!(
            all.top_senders
                .iter()
                .map(|row| (row.address.as_str(), row.count))
                .collect::<Vec<_>>(),
            vec![("loud@example.test", 2), ("quiet@example.test", 1)]
        );
        // The name comes off the newest of the two, which is message 1.
        assert_eq!(all.top_senders[0].name, "Name 1");
    }

    #[test]
    fn top_recipients_split_quoted_headers_and_count_a_repeat_once() {
        let (_directory, store) = store_with(&format!(
            "{}{}{}",
            message(
                1,
                1,
                NOW_MS - HOUR_MS,
                1,
                "me@x.test",
                "\"Example, Dana\" <Dana@Example.test>, bob@example.test"
            ),
            message(
                2,
                1,
                NOW_MS - 2 * HOUR_MS,
                1,
                "me@x.test",
                "dana@example.test, Dana <dana@example.test>"
            ),
            // Received mail says nothing about who you write to.
            message(
                3,
                1,
                NOW_MS - 3 * HOUR_MS,
                0,
                "bob@example.test",
                "me@x.test"
            ),
        ));
        let all = stats(&store, window(Some(30), 0));
        assert_eq!(
            all.top_recipients
                .iter()
                .map(|row| (row.address.as_str(), row.count))
                .collect::<Vec<_>>(),
            vec![("dana@example.test", 2), ("bob@example.test", 1)]
        );
        assert_eq!(all.top_recipients[0].name, "Example, Dana");
    }

    #[test]
    fn a_malformed_stored_header_costs_one_message_not_the_whole_aggregate() {
        let (_directory, store) = store_with(&format!(
            "{}{}",
            message(
                1,
                1,
                NOW_MS - HOUR_MS,
                1,
                "me@x.test",
                "unclosed <angle@example.test"
            ),
            message(
                2,
                1,
                NOW_MS - 2 * HOUR_MS,
                1,
                "me@x.test",
                "good@example.test"
            ),
        ));
        let all = stats(&store, window(Some(30), 0));
        assert_eq!(all.sent_total, 2);
        assert_eq!(
            all.top_recipients
                .iter()
                .map(|row| row.address.as_str())
                .collect::<Vec<_>>(),
            vec!["good@example.test"]
        );
    }

    #[test]
    fn all_time_reaches_the_oldest_message_and_admits_when_it_cannot() {
        let (_directory, store) = store_with(&message(
            1,
            1,
            NOW_MS - 400 * 24 * HOUR_MS,
            0,
            "s@example.test",
            "me@x.test",
        ));
        let reachable = stats(&store, window(None, 0));
        assert_eq!(reachable.end_day - reachable.start_day, 400);
        assert!(!reachable.truncated);

        store
            .connection
            .execute_batch(&message(
                2,
                1,
                NOW_MS - 4_000 * 24 * HOUR_MS,
                0,
                "s@example.test",
                "me@x.test",
            ))
            .expect("older message");
        let capped = stats(&store, window(None, 0));
        assert_eq!(capped.start_day, capped.end_day - (MAX_STAT_DAYS - 1));
        assert!(capped.truncated);
    }

    #[test]
    fn an_empty_store_still_returns_a_window() {
        let (_directory, store) = store_with("");
        let all = stats(&store, window(None, 0));
        assert_eq!(all.start_day, all.end_day);
        assert!(all.days.is_empty());
        assert!(all.top_senders.is_empty() && all.top_recipients.is_empty());
        assert_eq!((all.received_total, all.sent_total), (0, 0));
        assert!(!all.truncated);
    }

    #[test]
    fn an_account_filter_counts_only_that_accounts_mail() {
        let (_directory, store) = store_with(&format!(
            "{}{}",
            message(1, 1, NOW_MS - HOUR_MS, 0, "s@example.test", "me@x.test"),
            message(2, 2, NOW_MS - HOUR_MS, 0, "s@example.test", "me@x.test"),
        ));
        let scoped = stats(
            &store,
            MailStatsInput {
                days: Some(30),
                account_id: Some("acc_b".into()),
                timezone_offset_minutes: 0,
                top_limit: None,
            },
        );
        assert_eq!(scoped.received_total, 1);
        assert_eq!(stats(&store, window(Some(30), 0)).received_total, 2);
    }

    #[test]
    fn out_of_range_input_is_refused_rather_than_clamped_into_a_wrong_chart() {
        let (_directory, store) = store_with("");
        for days in [0, -1, MAX_STAT_DAYS + 1] {
            let error = store
                .mail_stats_at(window(Some(days), 0), NOW_MS)
                .expect_err("day window is bounded");
            assert!(matches!(error, StoreError::Validation(_)), "{error}");
        }
        for offset in [-841, 841] {
            let error = store
                .mail_stats_at(window(Some(30), offset), NOW_MS)
                .expect_err("timezone offset is bounded");
            assert!(matches!(error, StoreError::Validation(_)), "{error}");
        }
        let error = store
            .mail_stats_at(
                MailStatsInput {
                    days: Some(30),
                    account_id: Some(String::new()),
                    timezone_offset_minutes: 0,
                    top_limit: None,
                },
                NOW_MS,
            )
            .expect_err("an empty account id is not a filter");
        assert!(matches!(error, StoreError::Validation(_)), "{error}");
    }

    #[test]
    fn top_limit_is_bounded_at_both_ends() {
        let rows = (1..=30)
            .map(|id| {
                message(
                    id,
                    1,
                    NOW_MS - id * HOUR_MS,
                    0,
                    &format!("sender{id}@example.test"),
                    "me@x.test",
                )
            })
            .collect::<String>();
        let (_directory, store) = store_with(&rows);
        let asked_for_none = stats(
            &store,
            MailStatsInput {
                days: Some(30),
                account_id: None,
                timezone_offset_minutes: 0,
                top_limit: Some(0),
            },
        );
        assert_eq!(asked_for_none.top_senders.len(), 1);
        let asked_for_everything = stats(
            &store,
            MailStatsInput {
                days: Some(30),
                account_id: None,
                timezone_offset_minutes: 0,
                top_limit: Some(i64::MAX),
            },
        );
        assert_eq!(
            asked_for_everything.top_senders.len(),
            MAX_TOP_ADDRESSES as usize
        );
    }

    #[test]
    fn mailbox_addresses_lose_their_decoration_but_keep_their_name() {
        assert_eq!(
            mailbox_address_and_name("\"Dana X\" <Dana@Example.test>"),
            Some(("dana@example.test".into(), "Dana X".into()))
        );
        assert_eq!(
            mailbox_address_and_name("bare@example.test"),
            Some(("bare@example.test".into(), String::new()))
        );
        assert_eq!(mailbox_address_and_name("Nobody <>"), None);
        assert_eq!(mailbox_address_and_name("not-an-address"), None);
    }
}
