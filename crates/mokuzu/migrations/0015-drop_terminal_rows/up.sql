-- Mokuzu runs only as a Discord bot now, so every row belongs to a Discord user. Rows the
-- terminal saved (NULL user_id) can never load again and are dropped.
DELETE FROM memories WHERE user_id IS NULL;
DELETE FROM skills WHERE user_id IS NULL;
DELETE FROM jobs WHERE user_id IS NULL;
DELETE FROM todos WHERE user_id IS NULL;
