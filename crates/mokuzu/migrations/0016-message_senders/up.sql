-- Who each message belongs to: the Discord user whose turn wrote it, for both their prompt and the
-- model's reply, so search_history can reach one person's other conversations. NULL for overheard
-- chat, compaction summaries and the turns they re-insert, and rows from before this column.
ALTER TABLE messages ADD COLUMN user_id INTEGER;
