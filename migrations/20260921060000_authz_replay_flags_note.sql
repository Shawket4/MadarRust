-- A reviewer's optional note, kept with the resolution (owner, 2026-09-17):
-- clearable flags now carry who, when AND why they were cleared.
ALTER TABLE authz_replay_flags ADD COLUMN IF NOT EXISTS review_note text NULL;
