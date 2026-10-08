-- Lesson scope routes promoted lessons: project lessons are injected only into
-- their own project, user lessons into every project, and harness lessons only
-- into self-improvement intake. Existing rows predate the split and stay project.
ALTER TABLE memory_ledger ADD COLUMN scope TEXT NOT NULL DEFAULT 'project'
    CHECK(scope IN ('project', 'harness', 'user'));
ALTER TABLE memory_entries ADD COLUMN scope TEXT NOT NULL DEFAULT 'project'
    CHECK(scope IN ('project', 'harness', 'user'));
CREATE INDEX idx_memory_entries_scope_status ON memory_entries(scope, status);
