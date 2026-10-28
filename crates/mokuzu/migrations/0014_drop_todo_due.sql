-- Todos no longer have due times: anything at a set time is a cron job, and todos are followed
-- until their condition is met.
ALTER TABLE todos DROP COLUMN due;
