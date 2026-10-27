//! Todos: follow-ups the agent writes itself, one list per person, each remembering the
//! conversation it was written in. Under `mokuzu serve` a heartbeat wakes every `HEARTBEAT` to work
//! through them, and stays silent unless something needs the user.

use std::time::Duration;

use anyhow::Result;
use chrono::DateTime;
use chrono_tz::Tz;
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};

use crate::agent::Io;
use crate::cron;
use crate::db::Todo;

/// How often the heartbeat checks open todos.
pub const HEARTBEAT: Duration = Duration::from_secs(30 * 60);

/// The heartbeat's reply when nothing needs the user; it is not posted.
pub const SILENT: &str = "NO_REPLY";

/// Characters of one todo, progress notes included.
const CONTENT_LIMIT: usize = 1_000;

/// Characters of the todo list in the system prompt; past it, older todos are left out.
// ponytail: fixed budget; make it configurable if people hit it.
const SNAPSHOT_LIMIT: usize = 3_000;

/// Stable guidance for the system prompt; the todos themselves come from `snapshot`.
pub const GUIDANCE: &str = "\
# Todos
You keep a todo list for the person sending the current message. When they ask you to remind \
them, follow up, check back, or do something later, or when you promise later work yourself, add \
it with the todo tool in that same response, with a `due` time when one is known. Write each todo \
so it makes sense alone, with the details needed to act on it. While working one, replace its \
content with what is left and what you found; mark it done once finished or no longer wanted. \
Every 30 minutes, and when a todo's due time comes, you wake on your own to work through open \
todos and tell the user only what needs them; a due time is the reminder, so a todo with one \
needs no scheduled job. Never say you noted something unless the todo call succeeded.";

/// A change the todo tool asked for, already validated against the current todos.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    Add {
        content: String,
        due: Option<i64>,
    },
    Update {
        id: i64,
        content: String,
        due: Option<i64>,
    },
    Done(i64),
}

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "todo".to_owned(),
        description: "Add, update, or finish a todo for the person sending the current message. \
                      `add` needs `content`; `update` needs `id` and a new `content` (the whole \
                      new text, progress included), `due`, or both; `done` needs `id` and deletes \
                      it. Open todos are listed in your instructions."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "update", "done"]},
                "content": {
                    "type": "string",
                    "description": "For add and update: what to do, self-contained, in the user's language.",
                },
                "due": {
                    "type": "string",
                    "description": "For add and update: when it is due, `YYYY-MM-DD HH:MM` in the user's time zone. For update, `none` clears it.",
                },
                "id": {"type": "integer", "description": "For update and done: the todo's id."},
            },
            "required": ["action"],
        }),
    }
}

/// Validates a todo tool call against `todos` at time `now`; errors are messages for the model.
pub fn plan(todos: &[Todo], args: &Value, now: DateTime<Tz>) -> Result<Edit, String> {
    let content = args["content"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    if content.is_some_and(|c| c.chars().count() > CONTENT_LIMIT) {
        return Err(format!(
            "`content` is over {CONTENT_LIMIT} characters; keep only what is left to do"
        ));
    }
    let due = args["due"]
        .as_str()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let parse_due = |due: &str| cron::future_time("due", due, now);
    match args["action"].as_str().unwrap_or_default() {
        "add" => Ok(Edit::Add {
            content: content.ok_or("`content` is required for add")?.to_owned(),
            due: due.map(parse_due).transpose()?,
        }),
        "update" => {
            let todo = target(todos, args)?;
            if content.is_none() && due.is_none() {
                return Err("update needs a new `content`, `due`, or both".to_owned());
            }
            Ok(Edit::Update {
                id: todo.id,
                content: content.unwrap_or(&todo.content).to_owned(),
                due: match due {
                    None => todo.due,
                    Some("none") => None,
                    Some(due) => Some(parse_due(due)?),
                },
            })
        }
        "done" => Ok(Edit::Done(target(todos, args)?.id)),
        other => Err(format!(
            "unknown action {other:?}; use add, update, or done"
        )),
    }
}

/// The todo an `update` or `done` call names by `id`.
fn target<'a>(todos: &'a [Todo], args: &Value) -> Result<&'a Todo, String> {
    let id = args["id"]
        .as_i64()
        .ok_or("`id` is required for update and done")?;
    todos
        .iter()
        .find(|todo| todo.id == id)
        .ok_or_else(|| format!("no todo #{id}; open todos are in your instructions"))
}

/// One todo as a line: `#id [due …] content`.
fn line(todo: &Todo, zone: Tz) -> String {
    match todo.due {
        Some(due) => format!(
            "#{} [due {}] {}",
            todo.id,
            cron::local_time(due, zone),
            todo.content
        ),
        None => format!("#{} {}", todo.id, todo.content),
    }
}

/// The open todos as they appear in the system prompt, newest kept when over budget.
pub fn snapshot(todos: &[Todo], zone: Tz) -> String {
    if todos.is_empty() {
        return "Open todos: none.".to_owned();
    }
    let mut lines = Vec::new();
    let mut used = 0;
    for todo in todos.iter().rev() {
        let line = line(todo, zone);
        used += line.chars().count();
        if used > SNAPSHOT_LIMIT {
            break;
        }
        lines.push(line);
    }
    let left_out = todos.len() - lines.len();
    lines.reverse();
    let mut text = format!("Open todos:\n{}", lines.join("\n"));
    if left_out > 0 {
        text.push_str(&format!(
            "\n({left_out} older todos not shown; finish or merge some)"
        ));
    }
    text
}

/// One line telling the user what changed.
pub fn describe(edit: &Edit, todos: &[Todo]) -> String {
    match edit {
        Edit::Add { content, .. } => format!("📝 todo: {content}"),
        Edit::Update { content, .. } => format!("📝 todo updated: {content}"),
        Edit::Done(id) => format!(
            "✅ done: {}",
            todos
                .iter()
                .find(|t| t.id == *id)
                .map_or("", |t| t.content.as_str())
        ),
    }
}

/// What the model is told on a heartbeat run over `todos`, all written in one conversation.
pub fn heartbeat_prompt(todos: &[Todo], now: DateTime<Tz>) -> String {
    let list: Vec<String> = todos
        .iter()
        .map(|todo| {
            let mut line = line(todo, now.timezone());
            if todo.due.is_some_and(|due| due <= now.timestamp()) {
                line.push_str(" (due now)");
            }
            line
        })
        .collect();
    format!(
        "[Heartbeat at {now}. You woke on your own to check the todos written in this \
         conversation; nobody is watching live and actions that need approval will be denied.\n\
         Work each one you can make progress on with your tools now. Update its content with what \
         you found and what is left, or mark it done. A todo that is only a reminder is done once \
         you remind the user, so mark it done rather than reminding again next time. Then \
         message the user only if something is \
         due, finished, or needs them, in their language (from saved facts, else the todos'), not \
         this note's. Otherwise reply exactly {SILENT} and nothing else; it is not posted.]\n\n{}",
        list.join("\n"),
        now = now.format("%Y-%m-%d %H:%M"),
    )
}

/// Whether `todo`'s due time falls after `after`, up to and including `now`.
pub fn came_due(todo: &Todo, after: i64, now: i64) -> bool {
    todo.due.is_some_and(|due| after < due && due <= now)
}

/// Whether a heartbeat's last reply means "nothing to say".
pub fn is_silent(reply: &str) -> bool {
    reply.trim() == SILENT
}

/// What a heartbeat run said, held back until it is known whether to post it.
enum Held {
    Say(String),
    Note(String),
    Ran(String, bool),
}

/// Wraps `inner`, holding back replies and notes until `finish`, which drops them all when the
/// last reply is `SILENT`. Approvals and files go straight through.
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

    /// Posts what was held unless the last reply is `SILENT`; true if it posted.
    pub async fn finish(self) -> Result<bool> {
        let last_reply = self.held.iter().rev().find_map(|held| match held {
            Held::Say(text) => Some(text.as_str()),
            _ => None,
        });
        if last_reply.is_some_and(is_silent) {
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
    use chrono::TimeZone as _;

    use super::*;

    fn now() -> DateTime<Tz> {
        Tz::Asia__Taipei
            .with_ymd_and_hms(2026, 10, 28, 9, 0, 0)
            .single()
            .expect("time")
    }

    fn todo(id: i64, content: &str, due: Option<i64>) -> Todo {
        Todo {
            id,
            user: Some(7),
            target: "discord:1".to_owned(),
            content: content.to_owned(),
            due,
        }
    }

    #[test]
    fn plan_validates_calls() {
        let todos = [todo(1, "send the report", None)];
        let tomorrow = now().timestamp() + 86_400;
        assert_eq!(
            plan(
                &todos,
                &json!({"action": "add", "content": " call mom ", "due": "2026-10-29 09:00"}),
                now()
            ),
            Ok(Edit::Add {
                content: "call mom".to_owned(),
                due: Some(tomorrow)
            })
        );
        assert!(plan(&todos, &json!({"action": "add"}), now()).is_err());
        assert!(
            plan(
                &todos,
                &json!({"action": "add", "content": "x", "due": "2026-10-27 09:00"}),
                now()
            )
            .is_err(),
            "due in the past"
        );
        assert!(
            plan(
                &todos,
                &json!({"action": "add", "content": "x", "due": "tomorrow"}),
                now()
            )
            .is_err()
        );
        assert_eq!(
            plan(
                &todos,
                &json!({"action": "update", "id": 1, "due": "2026-10-29 09:00"}),
                now()
            ),
            Ok(Edit::Update {
                id: 1,
                content: "send the report".to_owned(),
                due: Some(tomorrow)
            })
        );
        let dated = [todo(1, "send the report", Some(tomorrow))];
        assert_eq!(
            plan(
                &dated,
                &json!({"action": "update", "id": 1, "due": "none"}),
                now()
            ),
            Ok(Edit::Update {
                id: 1,
                content: "send the report".to_owned(),
                due: None
            })
        );
        assert!(plan(&todos, &json!({"action": "update", "id": 1}), now()).is_err());
        assert!(plan(&todos, &json!({"action": "done", "id": 2}), now()).is_err());
        assert_eq!(
            plan(&todos, &json!({"action": "done", "id": 1}), now()),
            Ok(Edit::Done(1))
        );
    }

    #[test]
    fn heartbeat_prompt_marks_due_todos() {
        let past = now().timestamp() - 60;
        let prompt = heartbeat_prompt(
            &[
                todo(1, "send the report", Some(past)),
                todo(2, "watch the PR", None),
            ],
            now(),
        );
        assert!(prompt.contains("#1 [due 2026-10-28 08:59] send the report (due now)"));
        assert!(prompt.contains("#2 watch the PR"));
        assert!(is_silent(" NO_REPLY\n") && !is_silent("NO_REPLY, but the PR merged"));
        let due = todo(1, "x", Some(100));
        assert!(came_due(&due, 99, 100) && !came_due(&due, 100, 130) && !came_due(&due, 60, 99));
        assert!(!came_due(&todo(2, "x", None), 0, 200));
    }

    /// Records what reaches the user.
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
        quiet.note("📝 todo updated: x").await.expect("note");
        quiet.say(" NO_REPLY ").await.expect("say");
        assert!(!quiet.finish().await.expect("finish"));
        assert!(seen.0.is_empty());

        let mut quiet = Quiet::new(&mut seen);
        quiet.say("NO_REPLY").await.expect("say");
        quiet.note("✅ done: x").await.expect("note");
        quiet.say("The report is due now.").await.expect("say");
        assert!(quiet.finish().await.expect("finish"));
        assert_eq!(seen.0, ["note ✅ done: x", "say The report is due now."]);
    }

    #[test]
    fn snapshot_keeps_newest_within_budget() {
        let long = "x".repeat(CONTENT_LIMIT);
        let todos: Vec<Todo> = (1..=5).map(|id| todo(id, &long, None)).collect();
        let text = snapshot(&todos, Tz::UTC);
        assert!(text.contains("#5 ") && !text.contains("#1 "));
        assert!(text.ends_with("(3 older todos not shown; finish or merge some)"));
    }
}
