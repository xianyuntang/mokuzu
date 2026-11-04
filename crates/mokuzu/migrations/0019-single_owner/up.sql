-- Mokuzu serves its owner alone, so memory, skills, jobs, and webhooks are no longer kept per
-- Discord user: everything the allowed users saved becomes one set. Skills with the same name keep
-- the newest. Messages keep only whether a turn wrote them (`from_turn`), which search_history
-- uses to skip the turns a compaction summary re-inserts.
ALTER TABLE memories DROP COLUMN user_id;
DROP INDEX skills_by_user;
DELETE FROM skills WHERE EXISTS (
    SELECT 1 FROM skills AS newer
    WHERE newer.name = skills.name
        AND (newer.updated_at > skills.updated_at
            OR (newer.updated_at = skills.updated_at AND newer.id > skills.id))
);
ALTER TABLE skills DROP COLUMN user_id;
CREATE UNIQUE INDEX skills_by_name ON skills (name);
ALTER TABLE jobs DROP COLUMN user_id;
ALTER TABLE webhooks DROP COLUMN user_id;
UPDATE messages SET user_id = 1 WHERE user_id IS NOT NULL;
ALTER TABLE messages RENAME COLUMN user_id TO from_turn;
