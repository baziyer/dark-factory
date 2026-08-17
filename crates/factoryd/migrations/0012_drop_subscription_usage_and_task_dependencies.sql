-- Usage monitor collapsed to an on-demand `factoryctl usage` probe (no
-- daemon persistence) and task-to-task dependencies were never gated on or
-- written by anything. Drop the tables introduced for both in migrations
-- 0007 and 0006.
DROP TABLE subscription_usage_probes;

DROP TABLE subscription_usage_state;

DROP TABLE task_dependencies;
