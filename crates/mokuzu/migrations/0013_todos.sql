-- Follow-ups the agent writes itself, one list per person (a Discord user ID, NULL for the
-- terminal). The heartbeat works through them and posts to `target`, the conversation each was
-- written in. `content` holds the item and the agent's progress notes. `due` is optional, in Unix
-- seconds. Done items are deleted.
CREATE TABLE todos (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER,
    target     TEXT    NOT NULL,
    content    TEXT    NOT NULL,
    due        INTEGER,
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
