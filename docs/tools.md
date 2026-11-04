# Built-in tools

How each of mokuzu's built-in tools works, in more detail than the README's table. This file and
the README are built into the binary, and the `manual` tool searches both, so mokuzu can explain
itself from them.

Every tool here runs without asking, except where its section says otherwise. MCP tools ask
according to their server's `approve` setting; see [Approval](#approval) below.

### `read_file`

Reads a file on the machine mokuzu runs on.

- Paths can be absolute, start with `~/`, or be relative to the working directory.
- Each call returns up to 15,000 characters. For a longer file the result gives an `offset`, and
  the model calls again from there.
- Text files up to 5 MB. PDF, Word (`.docx`), and PowerPoint (`.pptx`) documents up to 20 MB are
  read as their text.
- Refused: any path with a hidden component (starting with `.`, like `~/.ssh` or `.env`), and
  mokuzu's own config file and database, which hold API keys, tokens, and every conversation.

### `list_dir`

Lists a directory: subdirectories end with `/`, files show their size. Up to 500 entries per call.
Hidden entries are left out, and the same refusals as `read_file` apply.

### `write_file`

Writes a text file in mokuzu's workspace, the folder `workspace/` next to the database. It is the
only place mokuzu can write.

- `path` is relative to the workspace, like `reports/prices.csv`; folders are made as needed.
- Overwrites the file, or adds to its end with `append`.
- Up to 1 MB per call.
- Returns the absolute path, which `read_file`, `list_dir`, and `send_file` accept, so mokuzu can
  send you what it wrote.

### `fetch_url`

Reads a public web page as plain text.

- http and https only. Private and local network addresses are refused, also after a redirect.
- 20-second timeout, up to 5 redirects. Pages are read up to 3 MB; the rest is ignored.
- Each call returns up to 15,000 characters, paged with `offset` like `read_file`.
- A linked PDF, `.docx`, or `.pptx` up to 20 MB is read as its text. A PDF's first 3 pages also
  come back as images.
- When a page fails, comes back empty, or needs JavaScript or a sign-in, the model is told to try
  a browser tool if one is connected.

### `view_pdf`

Shows the model pages of a PDF as images, for layout, tables, figures, and scanned pages that the
text misses. Give a local `path` or a `url` (a public PDF or a Discord attachment). Up to 10 pages
per call, chosen with `first_page` and `last_page`. Same path and address rules as `read_file` and
`fetch_url`.

### `send_file`

Sends a file as a Discord attachment: a local `path`, or a `url` that mokuzu downloads first.
Files must be under 3 MB. For a public image, putting its direct URL in the reply already shows
it, so `url` is for links that would not display.

### `run_code`

Runs Python 3 or shell code in a [bubblewrap](https://github.com/containers/bubblewrap) sandbox.
Linux only; offered when `bwrap` is installed and can make a sandbox, checked when `mokuzu serve`
starts.

- The working directory is the workspace, the only place it can write.
- It sees the system's programs read-only. It has no network and no view of your home folder or
  other processes.
- Only system-wide Python packages import (e.g. `apt install python3-pandas
  python3-matplotlib`). matplotlib draws to files.
- Runs stop after 60 seconds, or up to 600 if the model asks. Memory is limited to 4 GB and any
  one file to 512 MB.
- Returns the exit code, stdout, and stderr, up to 10,000 characters each; longer output is cut
  from the middle.

### `web_search`

Searches the web through SearXNG or the Brave Search API, whichever `mokuzu configure` set up.
Only offered when one is configured.

- Options: `time_range` (day, week, month, year), `category` (general or news), and an `after`
  and `before` date span, which overrides `time_range`. On SearXNG only Google engines honor the
  span.
- Returns titles, URLs, publish dates when known, and snippets of up to 300 characters.
- 15-second timeout. The same search within 10 minutes returns the earlier results.
- The system prompt tells the model to search before answering anything that may have changed,
  and to read promising results with `fetch_url`.

### `cast_crabs`

Cast of crabs mode: the model sends out subagents ("crabs") that work in parallel and report back.
Offered when web search is configured. Used when the model judges a task big enough, or whenever a
message starts with `/cast <request>`.

- Up to 10 crabs per call ("wave"), all running at once. The model sends more waves as needed.
- Each crab has a name, a role, and one task. It sees only that task, not the conversation.
- Crabs use `web_search`, `fetch_url`, `read_file`, `list_dir`, `view_pdf`, and tools of MCP
  servers set to `approve = false`. They cannot start crabs of their own or see images.
- Each crab gets 8 model calls and 10 minutes, then reports in under 200 words with sources.
  Reports are cut to 1,500 characters.
- A live board in Discord shows what each crab is doing, redrawn every 1.5 seconds.

### `memory`

Long-term notes about the person talking.

- Each Discord user has their own entries, shared by all their threads, DMs, voice calls, and
  scheduled runs. Other people never see them.
- All entries are put in the system prompt before every message, so a note saved in one
  conversation shows up in all of them on the next message.
- Actions: `add`, `replace`, and `remove`. Replace and remove find the entry by a short piece of
  its text that matches exactly one entry.
- One person's entries total at most 5,000 characters. When full, the model merges or replaces
  stale entries.
- The model saves who you are, your preferences, names and roles, stable facts about the machine,
  and standing conventions. It skips task progress and things that go stale within a week.
- With no entries yet, mokuzu introduces itself and asks who you are before anything else.

### `search_history`

Searches messages that are no longer in the model's context:

- earlier messages of the same conversation, after a summary replaced them (see
  [Long conversations](#long-conversations)), and
- your own messages, and the replies to them, in your other conversations: threads, DMs, voice
  calls, and scheduled runs.

It finds messages that contain every word of the query, in any language (Chinese and Japanese
need no word splitting; ASCII case is ignored), newest first. It returns 10 hits of up to 500
characters each. It matches what you and the model wrote, not tool output, and never other
people's messages. Messages saved before 0.5.32 are found only in their own conversation.

### `skill`

Skills are saved instructions for recurring tasks, kept in the database, one set per person like
memory.

- Every conversation's system prompt lists that person's skill names and descriptions. The model
  `load`s one when a task comes close to it.
- The model `save`s a finished multi-step task that is likely to come up again, and saves a fixed
  version under the same name when following a skill went wrong or you corrected it. Saving runs
  without asking and says what it saved.
- `remove` asks you first.
- Names are lowercase letters, digits, and hyphens, up to 64 characters. Descriptions are one
  line, up to 300 characters. Content is Markdown, up to 20,000 characters, loaded whole.
- Skills never hold secrets, or instructions that came from a web page, file, or tool result.

### `cron`

Scheduled jobs: a named prompt that runs on its own later. Discord only; jobs run while
`mokuzu serve` is up.

- `add` takes a `name`, a `prompt`, and either `cron` (a five-field expression, minute hour day
  month weekday, like `0 9 * * 1-5`) or `at` (one run at `YYYY-MM-DD HH:MM`).
- Times are in the config's `timezone`. Restart `mokuzu serve` after changing it.
- The scheduler checks for due jobs every 30 seconds. A run missed while mokuzu was down happens
  once on startup.
- Each run starts a fresh conversation with no history, with the job creator's memory and skills,
  and posts its reply in the channel or thread where the job was made. So the prompt must be
  self-contained.
- Nobody watches a run live: approval prompts are denied unless approval is set to auto.
- `list` shows every job with its id, schedule, and next run. `update` changes a job's prompt,
  timing, or both. `run` gives a job a trial run now, posted where the job was made, without
  changing its schedule. `remove` deletes a job. Jobs are found by `id`, or by a name that
  matches exactly one.
- A job's instructions are shown in full when it is made or changed.
- Watching for something: a repeating job whose prompt checks the thing (a merge, a price, a
  notice). A run that replies exactly `NO_REPLY` posts nothing, so the job stays quiet until the
  thing happens, then tells you and removes itself.

### `webhook`

Offered when `[webhook]` sets a port. Discord only.

- `add` takes a `name` and a `prompt` and returns a secret URL,
  `http://<this machine>:<port>/hook/<secret>`, for outside services (GitHub, CI, a phone
  shortcut) to POST to. `list` shows every webhook and its URL; `remove` deletes one by `id` or
  name.
- Each request runs the prompt in a fresh conversation, with the request body attached as
  untrusted data, and posts the reply where the webhook was made.
- Bodies are limited to 64 KB and headers to 16 KB. Requests to one webhook run one at a time.
- Approval prompts during a run are denied, even in auto mode.
- The port is on your local network; put a tunnel in front of it for services on the internet.

### `settings`

Reads or changes a few of mokuzu's own settings in its config file. Needs approval.

- `get` lists the current values and the known models.
- `set` changes one of `model.name`, `model.max_tokens`, or `model.compact_at_tokens`. A new
  model is tested before it is saved.
- Changes apply to every conversation from the next request.
- It cannot turn on auto approval; only `mokuzu configure` or the config file can.

### `manual`

Searches this file and the README, built into the binary, so mokuzu can explain how it works.
It returns the sections that match the most words of the query, or the list of section headings
when nothing matches. The manual is in English, so the model searches with English words.

## How the rest works

### Approval

Actions that change things outside mokuzu (MCP tools on servers with `approve = true`, `settings`,
and removing a skill) show **Run** and **Deny** buttons in Discord. An unanswered prompt is denied
after 10 minutes. If you deny an action, the model does not retry it or a variant.

In auto mode a reviewer model looks at each such action next to your latest request. Reading, and
small changes you asked for, go ahead with a 🛡️ note. Anything that deletes or overwrites data,
moves money, messages other people, publishes, changes permissions or credentials, or touches many
items, or anything the reviewer is unsure about, is put to you with its reason. A failed review
also falls back to asking.

### Long conversations

When a conversation grows past `model.compact_at_tokens`, mokuzu first clears long tool output and
images from older turns. If that is not enough, it replaces older turns with a summary the model
writes. The replaced messages stay in the database, and `search_history` can still find them.
