-- A conversation's plan: the projection, current only, and an index from revision to the change event.
-- The change event itself lives in `events`; the plan transaction is its only writer.
CREATE TABLE conversation_plans (
 plan_id TEXT PRIMARY KEY,
 thread_id TEXT NOT NULL UNIQUE,
 revision INTEGER NOT NULL CHECK(revision >= 0),
 next_task_seq INTEGER NOT NULL DEFAULT 1 CHECK(next_task_seq >= 1),
 node_count INTEGER NOT NULL DEFAULT 0 CHECK(node_count >= 0),
 plan_bytes INTEGER NOT NULL DEFAULT 0 CHECK(plan_bytes >= 0),
 progress_json TEXT NOT NULL,
 updated_ns INTEGER NOT NULL
);
CREATE TABLE conversation_plan_nodes (
 plan_id TEXT NOT NULL,
 task_id TEXT NOT NULL,
 ord INTEGER NOT NULL CHECK(ord >= 0),
 parent_id TEXT,
 position INTEGER NOT NULL CHECK(position >= 0),
 depth INTEGER NOT NULL CHECK(depth >= 1),
 title TEXT NOT NULL,
 description TEXT NOT NULL,
 result TEXT NOT NULL,
 blocker TEXT NOT NULL,
 weight INTEGER NOT NULL CHECK(weight BETWEEN 1 AND 100),
 status TEXT CHECK(status IS NULL OR status IN ('pending','in_progress','completed','blocked','interrupted')),
 child_count INTEGER NOT NULL CHECK(child_count >= 0),
 share_json TEXT NOT NULL,
 PRIMARY KEY(plan_id, task_id)
) WITHOUT ROWID;
CREATE UNIQUE INDEX idx_conversation_plan_nodes_ord ON conversation_plan_nodes(plan_id, ord);
CREATE TABLE conversation_plan_history (
 plan_id TEXT NOT NULL,
 revision INTEGER NOT NULL CHECK(revision >= 1),
 event_id INTEGER NOT NULL,
 PRIMARY KEY(plan_id, revision)
) WITHOUT ROWID;
