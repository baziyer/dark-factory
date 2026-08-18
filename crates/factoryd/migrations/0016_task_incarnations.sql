ALTER TABLE tasks ADD COLUMN incarnation_id TEXT NOT NULL DEFAULT '';

-- Existing task ids are globally unique, so this deterministically gives
-- every migrated row its own stable incarnation without relying on time.
UPDATE tasks SET incarnation_id = 'legacy:' || id;

CREATE UNIQUE INDEX tasks_incarnation_id ON tasks(incarnation_id);
