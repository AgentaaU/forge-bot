//! Token consumption rolled up per thread and per repository, split by model.
//!
//! The figures come from the run records the session store already persists:
//! each finished run carries the model its agent reported and the prompt-cache
//! accounting its adapter could read. Nothing is kept in a second registry, so
//! the page is rebuilt from the same state the status page uses.
//!
//! Runs whose adapter reported no usage are still counted as runs, but they add
//! no tokens. Runs persisted before a model was recorded are grouped under
//! [`UNKNOWN_MODEL`].

use std::collections::BTreeMap;

use serde::Serialize;

use super::status::escape_html;
use super::store::{RunRecord, Session};

/// Label for runs whose agent did not report a model.
pub const UNKNOWN_MODEL: &str = "unknown";

/// Totals for a set of runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// Agent runs recorded, whether or not the provider reported tokens.
    pub runs: u64,
    /// Runs whose adapter reported prompt-token usage.
    pub reported_runs: u64,
    /// Prompt tokens billed (cache hits plus fresh input).
    pub prompt_tokens: u64,
    /// Part of `prompt_tokens` served from the provider's prompt cache.
    pub cached_tokens: u64,
}

impl Usage {
    fn add(&mut self, run: &RunRecord) {
        self.runs += 1;
        if let Some(cache) = run.cache {
            self.reported_runs += 1;
            self.prompt_tokens += cache.prompt_tokens;
            self.cached_tokens += cache.cached_tokens;
        }
    }

    /// Percentage of prompt tokens served from cache, if any were reported.
    pub fn hit_rate(&self) -> Option<f64> {
        (self.prompt_tokens > 0)
            .then(|| 100.0 * self.cached_tokens as f64 / self.prompt_tokens as f64)
    }
}

/// One aggregated row: a thread or repository paired with one model.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// Conversation key for thread rows, repository name for repository rows.
    pub key: String,
    pub repository: String,
    /// Canonical web location of the thread, for thread rows.
    pub location: Option<String>,
    pub model: String,
    #[serde(flatten)]
    pub usage: Usage,
}

/// Everything the statistics page shows.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Statistics {
    /// Totals per model across every recorded run.
    pub models: Vec<(String, Usage)>,
    /// One row per repository and model.
    pub repositories: Vec<Row>,
    /// One row per thread and model.
    pub threads: Vec<Row>,
}

/// Aggregate sessions into per-model totals, repository rows and thread rows.
///
/// `location_of` maps a session's stored location to the URL shown to the
/// operator, so public links match the status page.
pub fn build(sessions: &[Session], location_of: impl Fn(&str) -> String) -> Statistics {
    let mut models: BTreeMap<String, Usage> = BTreeMap::new();
    let mut repos: BTreeMap<(String, String), Usage> = BTreeMap::new();
    let mut threads: BTreeMap<(String, String), (String, String, Usage)> = BTreeMap::new();

    for session in sessions {
        let thread_location = location_of(&session.location);
        for run in &session.runs {
            let model = run
                .model
                .clone()
                .unwrap_or_else(|| UNKNOWN_MODEL.to_owned());
            models.entry(model.clone()).or_default().add(run);
            repos
                .entry((session.repository.clone(), model.clone()))
                .or_default()
                .add(run);
            threads
                .entry((session.key.clone(), model))
                .or_insert_with(|| {
                    (
                        session.repository.clone(),
                        thread_location.clone(),
                        Usage::default(),
                    )
                })
                .2
                .add(run);
        }
    }

    let mut statistics = Statistics {
        models: models.into_iter().collect(),
        repositories: repos
            .into_iter()
            .map(|((repository, model), usage)| Row {
                key: repository.clone(),
                repository,
                location: None,
                model,
                usage,
            })
            .collect(),
        threads: threads
            .into_iter()
            .map(|((key, model), (repository, location, usage))| Row {
                key,
                repository,
                location: Some(location),
                model,
                usage,
            })
            .collect(),
    };
    sort_rows(&mut statistics.repositories);
    sort_rows(&mut statistics.threads);
    statistics.models.sort_by(|a, b| {
        b.1.prompt_tokens
            .cmp(&a.1.prompt_tokens)
            .then(a.0.cmp(&b.0))
    });
    statistics
}

/// Heaviest consumers first; ties fall back to name so the order is stable.
fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        b.usage
            .prompt_tokens
            .cmp(&a.usage.prompt_tokens)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.model.cmp(&b.model))
    });
}

/// Insert thousands separators into a count, e.g. `1234567` → `1,234,567`.
pub fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Render a hit rate as `92.5%`, or an em dash when nothing was reported.
pub fn format_hit_rate(usage: &Usage) -> String {
    match usage.hit_rate() {
        Some(rate) => format!("{rate:.1}%"),
        None => "—".to_owned(),
    }
}

/// Render the statistics page from the aggregated figures.
pub fn render_html(stats: &Statistics) -> String {
    let models = if stats.models.is_empty() {
        empty_row(6, "No runs recorded yet.")
    } else {
        stats
            .models
            .iter()
            .map(|(model, usage)| format!("<tr>{}</tr>\n", usage_cells(model, usage)))
            .collect()
    };
    let repositories = if stats.repositories.is_empty() {
        empty_row(7, "No repositories yet.")
    } else {
        stats
            .repositories
            .iter()
            .map(|row| {
                format!(
                    "<tr><td>{repo}</td>{cells}</tr>\n",
                    repo = escape_html(&row.repository),
                    cells = usage_cells(&row.model, &row.usage),
                )
            })
            .collect()
    };
    let threads = if stats.threads.is_empty() {
        empty_row(8, "No threads yet.")
    } else {
        stats
            .threads
            .iter()
            .map(|row| {
                format!(
                    "<tr><td><a href=\"{href}\">{key}</a></td><td class=\"repo\">{repo}</td>{cells}</tr>\n",
                    href = escape_html(row.location.as_deref().unwrap_or_default()),
                    key = escape_html(&row.key),
                    repo = escape_html(&row.repository),
                    cells = usage_cells(&row.model, &row.usage),
                )
            })
            .collect()
    };
    format!(
        include_str!("../../web/statistics.html"),
        models = models,
        repositories = repositories,
        threads = threads,
    )
}

fn empty_row(columns: usize, text: &str) -> String {
    format!("<tr class=\"empty\"><td colspan=\"{columns}\">{text}</td></tr>\n")
}

/// Model, run and token cells for one row. Callers wrap them in `<tr>` and add
/// any leading thread or repository cells.
fn usage_cells(model: &str, usage: &Usage) -> String {
    format!(
        "<td>{model}</td><td>{runs}</td><td>{reported}</td><td>{prompt}</td><td>{cached}</td><td>{rate}</td>",
        model = escape_html(model),
        runs = group_digits(usage.runs),
        reported = group_digits(usage.reported_runs),
        prompt = group_digits(usage.prompt_tokens),
        cached = group_digits(usage.cached_tokens),
        rate = format_hit_rate(usage),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TokenUsage;
    use chrono::Utc;
    use uuid::Uuid;

    fn run(model: Option<&str>, usage: Option<(u64, u64)>) -> RunRecord {
        RunRecord {
            job_id: Uuid::new_v4(),
            agent: "pi".into(),
            message: None,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            success: Some(true),
            summary: None,
            model: model.map(str::to_owned),
            cache: usage.map(|(prompt_tokens, cached_tokens)| TokenUsage {
                prompt_tokens,
                cached_tokens,
            }),
        }
    }

    fn session(key: &str, repository: &str, runs: Vec<RunRecord>) -> Session {
        let now = Utc::now();
        Session {
            key: key.into(),
            repository: repository.into(),
            location: format!("http://forge.local/{repository}/issues/1"),
            agent: "pi".into(),
            created_at: now,
            updated_at: now,
            runs,
        }
    }

    #[test]
    fn splits_tokens_by_thread_repository_and_model() {
        let sessions = vec![
            session(
                "forgejo:a/x:issue:1",
                "a/x",
                vec![
                    run(Some("m1"), Some((1_000, 900))),
                    run(Some("m2"), Some((500, 0))),
                    run(Some("m1"), None),
                ],
            ),
            session(
                "forgejo:a/x:issue:2",
                "a/x",
                vec![run(Some("m1"), Some((200, 100)))],
            ),
            session("forgejo:b/y:pr:3", "b/y", vec![run(None, Some((10, 5)))]),
        ];

        let stats = build(&sessions, |location| location.to_owned());

        let m1 = stats.models.iter().find(|(m, _)| m == "m1").unwrap();
        assert_eq!(m1.1.runs, 3);
        assert_eq!(m1.1.reported_runs, 2);
        assert_eq!(m1.1.prompt_tokens, 1_200);
        assert_eq!(m1.1.cached_tokens, 1_000);
        assert_eq!(stats.models[0].0, "m1", "heaviest model first");

        let repo_x_m1 = stats
            .repositories
            .iter()
            .find(|r| r.repository == "a/x" && r.model == "m1")
            .unwrap();
        assert_eq!(repo_x_m1.usage.prompt_tokens, 1_200);
        assert_eq!(repo_x_m1.usage.runs, 3);

        let thread_1_m1 = stats
            .threads
            .iter()
            .find(|r| r.key == "forgejo:a/x:issue:1" && r.model == "m1")
            .unwrap();
        assert_eq!(thread_1_m1.usage.prompt_tokens, 1_000);
        assert_eq!(thread_1_m1.usage.runs, 2);
        assert_eq!(
            thread_1_m1.location.as_deref(),
            Some("http://forge.local/a/x/issues/1")
        );

        let unknown = stats
            .threads
            .iter()
            .find(|r| r.model == UNKNOWN_MODEL)
            .unwrap();
        assert_eq!(unknown.key, "forgejo:b/y:pr:3");
        assert_eq!(unknown.usage.prompt_tokens, 10);
    }

    #[test]
    fn location_mapping_is_applied_to_threads() {
        let sessions = vec![session("k", "a/x", vec![run(Some("m"), Some((1, 0)))])];
        let stats = build(&sessions, |l| l.replace("forge.local", "public.example"));
        assert_eq!(
            stats.threads[0].location.as_deref(),
            Some("http://public.example/a/x/issues/1")
        );
    }

    #[test]
    fn no_sessions_gives_empty_statistics() {
        let stats = build(&[], |l| l.to_owned());
        assert!(stats.models.is_empty());
        assert!(stats.repositories.is_empty());
        assert!(stats.threads.is_empty());
    }

    #[test]
    fn groups_digits_and_formats_hit_rate() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(1_234_567), "1,234,567");

        let usage = Usage {
            runs: 1,
            reported_runs: 1,
            prompt_tokens: 40,
            cached_tokens: 37,
        };
        assert_eq!(format_hit_rate(&usage), "92.5%");
        assert_eq!(format_hit_rate(&Usage::default()), "—");
    }
}
