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
You keep a todo list for the person sending the current message, for things with no set time \
that you follow until some condition is met. Add one with the todo tool in the same response \
whenever you start something whose outcome is not in yet, without being asked: after opening a \
PR, follow its CI/CD, review, or merge; after starting a deploy, build, or long job, follow it to \
the end; after sending a message, wait for the reply. Also add one when the user asks you to watch \
something (\"tell me when it merges\", \"let me know if the price drops below X\"). Write each todo \
so it works alone: what to check (URL, repo, job id), how, what counts as done, and what to tell \
the user then. Every 30 minutes you wake on your own to check open todos: when one is met or has \
failed, tell the user the outcome and mark it done; otherwise update its progress and stay \
silent. For anything at a set time (a reminder in 5 minutes, a task tomorrow at 8, every \
morning), use cron, not a todo. Never say you are tracking something unless the todo call \
succeeded.";

/// A change the todo tool asked for, already validated against the current todos.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    Add(String),
    Update { id: i64, content: String },
    Done(i64),
}

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "todo".to_owned(),
        description: "Add, update, or finish a todo for the person sending the current message: \
                      something to follow until a condition is met. `add` needs `content`; \
                      `update` needs `id` and the whole new `content`, progress included; `done` \
                      needs `id` and deletes it. Open todos are listed in your instructions."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "update", "done"]},
                "content": {
                    "type": "string",
                    "description": "For add and update: what to check, how, and what counts as done, self-contained, in the user's language.",
                },
                "id": {"type": "integer", "description": "For update and done: the todo's id."},
            },
            "required": ["action"],
        }),
    }
}

/// Validates a todo tool call against `todos`; errors are messages for the model.
pub fn plan(todos: &[Todo], args: &Value) -> Result<Edit, String> {
    let content = args["content"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    if content.is_some_and(|c| c.chars().count() > CONTENT_LIMIT) {
        return Err(format!(
            "`content` is over {CONTENT_LIMIT} characters; keep only what is left to do"
        ));
    }
    let content = || content.map(str::to_owned);
    match args["action"].as_str().unwrap_or_default() {
        "add" => Ok(Edit::Add(content().ok_or("`content` is required for add")?)),
        "update" => Ok(Edit::Update {
            id: target(todos, args)?.id,
            content: content().ok_or("`content` is required for update")?,
        }),
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

/// One todo as a line: `#id content`.
fn line(todo: &Todo) -> String {
    format!("#{} {}", todo.id, todo.content)
}

/// The open todos as they appear in the system prompt, newest kept when over budget.
pub fn snapshot(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "Open todos: none.".to_owned();
    }
    let mut lines = Vec::new();
    let mut used = 0;
    for todo in todos.iter().rev() {
        let line = line(todo);
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
        Edit::Add(content) => format!("📝 todo: {content}"),
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

/// The todo tool's result on a heartbeat run.
pub const HEARTBEAT_SAVED: &str = "saved. Once every todo is checked, reply with what the user \
     needs to know, or exactly NO_REPLY if nothing.";

/// What the model is told on a heartbeat run over `todos`, all written in one conversation.
pub fn heartbeat_prompt(todos: &[Todo], now: DateTime<Tz>) -> String {
    let list: Vec<String> = todos.iter().map(line).collect();
    format!(
        "[Heartbeat at {now}. You woke on your own to check the todos written in this \
         conversation; nobody is watching live and actions that need approval will be denied.\n\
         Check each one with your tools now. When its condition is met or it has failed, tell the \
         user the outcome (say why if it failed, and ask what next if it needs them) and mark it \
         done. Otherwise update its content with what you found and what is left. Message the \
         user only about outcomes or what needs them, in their language (from saved facts, else \
         the todos'), not this note's. If there is nothing to tell, reply exactly {SILENT} and \
         nothing else; it is not posted.]\n\n{}",
        list.join("\n"),
        now = now.format("%Y-%m-%d %H:%M"),
    )
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
    use chrono::TimeZone as _;

    use super::*;

    fn now() -> DateTime<Tz> {
        Tz::Asia__Taipei
            .with_ymd_and_hms(2026, 10, 28, 9, 0, 0)
            .single()
            .expect("time")
    }

    fn todo(id: i64, content: &str) -> Todo {
        Todo {
            id,
            user: 7,
            target: "discord:1".to_owned(),
            content: content.to_owned(),
        }
    }

    #[test]
    fn plan_validates_calls() {
        let todos = [todo(1, "watch PR #12's CI")];
        assert_eq!(
            plan(
                &todos,
                &json!({"action": "add", "content": " watch the deploy "})
            ),
            Ok(Edit::Add("watch the deploy".to_owned()))
        );
        assert!(plan(&todos, &json!({"action": "add"})).is_err());
        assert_eq!(
            plan(
                &todos,
                &json!({"action": "update", "id": 1, "content": "CI running, 2 of 5 checks done"})
            ),
            Ok(Edit::Update {
                id: 1,
                content: "CI running, 2 of 5 checks done".to_owned()
            })
        );
        assert!(plan(&todos, &json!({"action": "update", "id": 1})).is_err());
        assert!(plan(&todos, &json!({"action": "done", "id": 2})).is_err());
        assert_eq!(
            plan(&todos, &json!({"action": "done", "id": 1})),
            Ok(Edit::Done(1))
        );
        let long = json!({"action": "add", "content": "x".repeat(CONTENT_LIMIT + 1)});
        assert!(plan(&todos, &long).is_err());
    }

    #[test]
    fn heartbeat_prompt_lists_todos() {
        let prompt = heartbeat_prompt(
            &[todo(1, "watch PR #12's CI"), todo(2, "wait for Amy")],
            now(),
        );
        assert!(prompt.starts_with("[Heartbeat at 2026-10-28 09:00."));
        assert!(prompt.ends_with("#1 watch PR #12's CI\n#2 wait for Amy"));
        assert!(is_silent(" NO_REPLY\n") && !is_silent("NO_REPLY, but the PR merged"));
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
        quiet.note("📝 todo updated: x").await.expect("note");
        assert!(!quiet.finish().await.expect("finish"));
        assert!(seen.0.is_empty());

        let mut quiet = Quiet::new(&mut seen);
        quiet.say("NO_REPLY").await.expect("say");
        quiet.note("✅ done: x").await.expect("note");
        quiet.say("PR #12 is green.").await.expect("say");
        assert!(quiet.finish().await.expect("finish"));
        assert_eq!(seen.0, ["note ✅ done: x", "say PR #12 is green."]);
    }

    #[test]
    fn snapshot_keeps_newest_within_budget() {
        let long = "x".repeat(CONTENT_LIMIT);
        let todos: Vec<Todo> = (1..=5).map(|id| todo(id, &long)).collect();
        let text = snapshot(&todos);
        assert!(text.contains("#5 ") && !text.contains("#1 "));
        assert!(text.ends_with("(3 older todos not shown; finish or merge some)"));
    }
}
