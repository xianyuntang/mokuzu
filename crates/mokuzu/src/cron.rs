//! Scheduled jobs: a prompt that runs on its own at set times, each run in a fresh conversation,
//! and posts its result where the job was made. Only `mokuzu serve` runs jobs, so only Discord
//! conversations get the tool. A run that replies `SILENT` posts nothing, so a repeating job can
//! watch for something and speak only when it happens.

use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, TimeZone as _};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};

use crate::agent::Io;
use crate::db::Job;

/// A run's reply when there is nothing to tell; it is not posted.
pub const SILENT: &str = "NO_REPLY";

/// Format of `at`, read in the configured time zone.
const AT_FORMAT: &str = "%Y-%m-%d %H:%M";

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "cron".to_owned(),
        description: "Schedule a prompt to run on its own later, once or repeatedly; each run \
                      starts a fresh conversation with no history and posts its reply here. \
                      `add` needs `name`, `prompt`, and either `cron` (repeat) or `at` (once). `list` shows \
                      every job; `remove` deletes one by `id` or `name`; `update` finds one the same way and \
                      replaces its `prompt`, its timing (`cron` or `at`), or both; `run` finds one the same \
                      way and runs it once now, as a trial, without changing its schedule. Times are in the user's time zone; \
                      the date is in each message's [sent ...] stamp. To watch for something \
                      (a merge, a price, a notice), add a repeating job whose prompt checks it, \
                      replies NO_REPLY while nothing has changed, and once it happens tells the \
                      user and removes this job."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "remove", "update", "run"]},
                "name": {
                    "type": "string",
                    "description": "For add: a short name for the job, in the user's language, e.g. `Morning news`. For remove, update, and run: the name of the job.",
                },
                "prompt": {
                    "type": "string",
                    "description": "For add and update: what to do on each run, replacing the old one on update. Make it self-contained, in the user's language, and say which language to reply in; the run won't see this conversation.",
                },
                "cron": {
                    "type": "string",
                    "description": "For add and update: five-field cron expression (minute hour day month weekday), e.g. `0 9 * * 1-5`.",
                },
                "at": {
                    "type": "string",
                    "description": "For add and update: one run at this time, `YYYY-MM-DD HH:MM`.",
                },
                "id": {"type": "integer", "description": "For remove, update, and run: the job id from list; wins over `name`."},
            },
            "required": ["action"],
        }),
    }
}

/// A job the cron tool asked for, already validated.
#[derive(Debug, PartialEq, Eq)]
pub struct NewJob {
    pub name: String,
    /// `None` runs once.
    pub schedule: Option<String>,
    pub prompt: String,
    pub next_run: i64,
}

/// A trimmed, non-empty string argument.
fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args[key].as_str().map(str::trim).filter(|v| !v.is_empty())
}

/// Validates an `add` call at time `now`; errors are messages for the model.
pub fn plan(args: &Value, now: DateTime<Tz>) -> Result<NewJob, String> {
    let name = text(args, "name")
        .ok_or("`name` is required for add")?
        .to_owned();
    let prompt = text(args, "prompt")
        .ok_or("`prompt` is required for add")?
        .to_owned();
    let (schedule, next_run) = timing(args, now)?.ok_or("give exactly one of `cron` or `at`")?;
    Ok(NewJob {
        name,
        schedule,
        prompt,
        next_run,
    })
}

/// `job` with an `update` call's changes applied at time `now`; errors are messages for the model.
pub fn revise(job: &Job, args: &Value, now: DateTime<Tz>) -> Result<Job, String> {
    let prompt = text(args, "prompt");
    let timing = timing(args, now)?;
    if prompt.is_none() && timing.is_none() {
        return Err("update needs a new `prompt`, `cron`, or `at`".to_owned());
    }
    let (schedule, next_run) = timing.unwrap_or_else(|| (job.schedule.clone(), job.next_run));
    Ok(Job {
        prompt: prompt.map_or_else(|| job.prompt.clone(), str::to_owned),
        schedule,
        next_run,
        ..job.clone()
    })
}

/// The `cron` or `at` in `args` as `(schedule, next_run)`, `None` if neither is given.
fn timing(args: &Value, now: DateTime<Tz>) -> Result<Option<(Option<String>, i64)>, String> {
    match (text(args, "cron"), text(args, "at")) {
        (None, None) => Ok(None),
        (Some(schedule), None) => Ok(Some((Some(schedule.to_owned()), next_run(schedule, now)?))),
        (None, Some(at)) => Ok(Some((None, future_time("at", at, now)?))),
        (Some(_), Some(_)) => Err("give only one of `cron` or `at`".to_owned()),
    }
}

/// `text`, a `YYYY-MM-DD HH:MM` local time given as argument `name`, in Unix seconds; it must be
/// after `now`.
fn future_time(name: &str, text: &str, now: DateTime<Tz>) -> Result<i64, String> {
    let naive = NaiveDateTime::parse_from_str(text, AT_FORMAT)
        .map_err(|err| format!("`{name}` must look like 2026-10-02 09:00: {err}"))?;
    let at = now
        .timezone()
        .from_local_datetime(&naive)
        .earliest()
        .ok_or(format!(
            "`{name}` does not exist in this time zone (a DST gap)"
        ))?;
    if at <= now {
        return Err(format!(
            "`{name}` is in the past; it is now {}",
            now.format(AT_FORMAT)
        ));
    }
    Ok(at.timestamp())
}

/// A one-off copy of `job` due at `now`: the scheduler runs it on its next tick, in the job's
/// conversation, then deletes it, leaving the original untouched.
pub fn trial(job: &Job, now: i64) -> NewJob {
    NewJob {
        name: format!("{} (trial)", job.name),
        schedule: None,
        prompt: job.prompt.clone(),
        next_run: now,
    }
}

/// When a job with `schedule` and `next` run time fires, for notes.
pub fn when(schedule: Option<&str>, next: i64, zone: Tz) -> String {
    match schedule {
        Some(schedule) => format!("{schedule}, next {}", local_time(next, zone)),
        None => format!("once at {}", local_time(next, zone)),
    }
}

/// The job a `remove`, `update`, or `run` call means: by `id`, or by `name` (ignoring case) when it matches
/// exactly one.
pub fn target<'a>(jobs: &'a [Job], args: &Value, zone: Tz) -> Result<&'a Job, String> {
    let found: Vec<&Job> = if let Some(id) = args["id"].as_i64() {
        jobs.iter().filter(|job| job.id == id).collect()
    } else {
        let name = args["name"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or("`id` or `name` is required for remove, update, and run")?
            .to_lowercase();
        jobs.iter()
            .filter(|job| job.name.to_lowercase() == name)
            .collect()
    };
    match found.as_slice() {
        [job] => Ok(job),
        [] => Err(format!("no such job\n{}", list(jobs, zone))),
        many => Err(format!(
            "{} jobs have that name; use `id`\n{}",
            many.len(),
            list(jobs, zone)
        )),
    }
}

/// The first time after `after` that `schedule` fires, in Unix seconds.
pub fn next_run(schedule: &str, after: DateTime<Tz>) -> Result<i64, String> {
    // Five fields only: a seconds field could fire on every scheduler tick.
    let cron: Cron = CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(schedule)
        .map_err(|err| format!("invalid cron expression {schedule:?}: {err}"))?;
    cron.find_next_occurrence(&after, false)
        .map(|next| next.timestamp())
        .map_err(|err| format!("{schedule:?} never fires: {err}"))
}

/// A Unix time as `YYYY-MM-DD HH:MM` in `zone`.
pub fn local_time(timestamp: i64, zone: Tz) -> String {
    zone.timestamp_opt(timestamp, 0).single().map_or_else(
        || timestamp.to_string(),
        |t| t.format(AT_FORMAT).to_string(),
    )
}

/// Every job, one per line, for `list`.
pub fn list(jobs: &[Job], zone: Tz) -> String {
    if jobs.is_empty() {
        return "no scheduled jobs".to_owned();
    }
    jobs.iter()
        .map(|job| {
            format!(
                "#{} {} [{}] next {} in {}: {}",
                job.id,
                job.name,
                job.schedule.as_deref().unwrap_or("once"),
                local_time(job.next_run, zone),
                job.target,
                job.prompt
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the model is told on a job's run.
pub fn prompt(job: &Job) -> String {
    format!(
        "[Scheduled job #{} {} ({}). You are running on your own in a fresh conversation; nobody is \
         watching live and actions that need approval will be denied. Do the task and reply with \
         the result, in the user's language (from saved facts, else the task's), not this note's. \
         If the task says to stay quiet while nothing has changed and nothing has, reply exactly \
         {SILENT} and nothing else; it is not posted.]\n\n{}",
        job.id,
        job.name,
        job.schedule.as_deref().unwrap_or("once"),
        job.prompt
    )
}

/// Whether a run's last reply means "nothing to say".
pub fn is_silent(reply: &str) -> bool {
    reply.trim() == SILENT
}

/// What a run said, held back until it is known whether to post it.
enum Held {
    Say(String),
    Note(String),
    Ran(String, bool),
}

/// Wraps `inner`, holding back replies and notes until `finish`, which drops them all when the
/// last reply is `SILENT` or there is none. Approvals and files go straight through.
pub struct Quiet<'a, I> {
    inner: &'a mut I,
    held: Vec<Held>,
}

impl<'a, I: Io> Quiet<'a, I> {
    pub fn new(inner: &'a mut I) -> Self {
        Self {
            inner,
            held: Vec::new(),
        }
    }

    /// Posts what was held unless there is no reply or the last one is `SILENT`; true if it
    /// posted.
    pub async fn finish(self) -> Result<bool> {
        let last_reply = self.held.iter().rev().find_map(|held| match held {
            Held::Say(text) => Some(text.as_str()),
            _ => None,
        });
        if last_reply.is_none_or(is_silent) {
            return Ok(false);
        }
        for held in self.held {
            match held {
                Held::Say(text) if !is_silent(&text) => self.inner.say(&text).await?,
                Held::Say(_) => {}
                Held::Note(text) => self.inner.note(&text).await?,
                Held::Ran(output, ok) => self.inner.ran(&output, ok).await?,
            }
        }
        Ok(true)
    }
}

impl<I: Io> Io for Quiet<'_, I> {
    async fn say(&mut self, text: &str) -> Result<()> {
        self.held.push(Held::Say(text.to_owned()));
        Ok(())
    }

    async fn confirm(&mut self, action: &str) -> Result<bool> {
        self.inner.confirm(action).await
    }

    async fn note(&mut self, text: &str) -> Result<()> {
        self.held.push(Held::Note(text.to_owned()));
        Ok(())
    }

    async fn ran(&mut self, output: &str, ok: bool) -> Result<()> {
        self.held.push(Held::Ran(output.to_owned(), ok));
        Ok(())
    }

    async fn send_file(&mut self, name: &str, bytes: Vec<u8>) -> Result<()> {
        self.inner.send_file(name, bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Seen(Vec<String>);

    impl Io for Seen {
        async fn say(&mut self, text: &str) -> Result<()> {
            self.0.push(format!("say {text}"));
            Ok(())
        }
        async fn confirm(&mut self, _action: &str) -> Result<bool> {
            Ok(false)
        }
        async fn note(&mut self, text: &str) -> Result<()> {
            self.0.push(format!("note {text}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn quiet_drops_silent_runs_and_replays_the_rest() {
        let mut seen = Seen::default();
        let mut quiet = Quiet::new(&mut seen);
        quiet.note("⏰ job #3 Watch CI").await.expect("note");
        quiet.say(" NO_REPLY ").await.expect("say");
        assert!(!quiet.finish().await.expect("finish"));
        assert!(seen.0.is_empty());

        let mut quiet = Quiet::new(&mut seen);
        quiet.note("⏰ job #3 Watch CI").await.expect("note");
        assert!(!quiet.finish().await.expect("finish"));
        assert!(seen.0.is_empty());

        let mut quiet = Quiet::new(&mut seen);
        quiet.say("NO_REPLY").await.expect("say");
        quiet.note("⏰ removed #3 Watch CI").await.expect("note");
        quiet.say("PR #12 is green.").await.expect("say");
        assert!(quiet.finish().await.expect("finish"));
        assert_eq!(
            seen.0,
            ["note ⏰ removed #3 Watch CI", "say PR #12 is green."]
        );
    }

    #[test]
    fn prompt_offers_silence() {
        let job = Job {
            id: 3,
            target: "discord:1".to_owned(),
            name: "Watch CI".to_owned(),
            prompt: "check PR #12".to_owned(),
            schedule: Some("*/30 * * * *".to_owned()),
            next_run: 0,
        };
        let text = prompt(&job);
        assert!(text.starts_with("[Scheduled job #3 Watch CI (*/30 * * * *)."));
        assert!(text.contains("reply exactly NO_REPLY"));
        assert!(text.ends_with("check PR #12"));
    }

    fn at(text: &str) -> DateTime<Tz> {
        let naive = NaiveDateTime::parse_from_str(text, AT_FORMAT).expect("time");
        Tz::Asia__Taipei
            .from_local_datetime(&naive)
            .earliest()
            .expect("local")
    }

    #[test]
    fn plan_validates_schedules() {
        let now = at("2026-10-01 08:30");
        let job = plan(
            &json!({"name": " News ", "prompt": " news ", "cron": "0 9 * * *"}),
            now,
        )
        .expect("cron");
        assert_eq!(job.schedule.as_deref(), Some("0 9 * * *"));
        assert_eq!(job.name, "News");
        assert_eq!(job.prompt, "news");
        assert_eq!(job.next_run, at("2026-10-01 09:00").timestamp());

        let once = plan(
            &json!({"name": "Call", "prompt": "call", "at": "2026-10-02 10:15"}),
            now,
        )
        .expect("at");
        assert_eq!(once.schedule, None);
        assert_eq!(once.next_run, at("2026-10-02 10:15").timestamp());

        for bad in [
            json!({"name": "n", "cron": "0 9 * * *"}),
            json!({"prompt": "x", "cron": "0 9 * * *"}),
            json!({"name": "n", "prompt": "x"}),
            json!({"name": "n", "prompt": "x", "cron": "0 9 * * *", "at": "2026-10-02 10:15"}),
            json!({"name": "n", "prompt": "x", "cron": "* * * * * *"}),
            json!({"name": "n", "prompt": "x", "cron": "nope"}),
            json!({"name": "n", "prompt": "x", "at": "2026-09-30 10:00"}),
            json!({"name": "n", "prompt": "x", "at": "tomorrow"}),
        ] {
            assert!(plan(&bad, now).is_err(), "{bad}");
        }
    }

    #[test]
    fn revise_replaces_prompt_or_timing() {
        let now = at("2026-10-01 08:30");
        let job = Job {
            id: 1,
            name: "News".to_owned(),
            target: "discord:1".to_owned(),
            schedule: Some("0 9 * * *".to_owned()),
            prompt: "old".to_owned(),
            next_run: 42,
        };
        let new_prompt = revise(&job, &json!({"prompt": " new "}), now).expect("prompt");
        assert_eq!(new_prompt.prompt, "new");
        assert_eq!(
            (new_prompt.schedule, new_prompt.next_run),
            (job.schedule.clone(), 42)
        );

        let once = revise(&job, &json!({"at": "2026-10-02 10:15"}), now).expect("at");
        assert_eq!(once.prompt, "old");
        assert_eq!(once.schedule, None);
        assert_eq!(once.next_run, at("2026-10-02 10:15").timestamp());
        assert_eq!(once.id, 1);

        assert!(
            revise(&job, &json!({"id": 1}), now).is_err(),
            "nothing to change"
        );
        assert!(revise(&job, &json!({"cron": "nope"}), now).is_err());

        let copy = trial(&job, 100);
        assert_eq!(copy.name, "News (trial)");
        assert_eq!((copy.schedule, copy.next_run), (None, 100));
        assert_eq!(copy.prompt, "old");
    }

    #[test]
    fn target_finds_by_id_or_unique_name() {
        let job = |id, name: &str| Job {
            id,
            name: name.to_owned(),
            target: "discord:1".to_owned(),
            schedule: None,
            prompt: "p".to_owned(),
            next_run: 0,
        };
        let jobs = [job(1, "News"), job(2, "Rent"), job(3, "rent")];
        assert_eq!(target(&jobs, &json!({"id": 2}), Tz::UTC), Ok(&jobs[1]));
        assert_eq!(
            target(&jobs, &json!({"name": " news "}), Tz::UTC),
            Ok(&jobs[0])
        );
        assert!(
            target(&jobs, &json!({"name": "rent"}), Tz::UTC).is_err(),
            "ambiguous"
        );
        assert!(target(&jobs, &json!({"name": "nope"}), Tz::UTC).is_err());
        assert!(target(&jobs, &json!({"id": 9}), Tz::UTC).is_err());
        assert!(target(&jobs, &json!({}), Tz::UTC).is_err());
    }

    #[test]
    fn next_run_is_strictly_after() {
        let nine = at("2026-10-01 09:00");
        assert_eq!(
            next_run("0 9 * * *", nine),
            Ok(at("2026-10-02 09:00").timestamp())
        );
    }
}
