ALTER TABLE memory_ledger ADD COLUMN kind TEXT NOT NULL DEFAULT 'lesson'
    CHECK(kind IN ('lesson', 'finding'));
