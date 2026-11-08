-- Fence asynchronous preparation across cancelled/reused stable SHA IDs, including ABA.
ALTER TABLE pr_notices ADD COLUMN reservation_revision INTEGER NOT NULL DEFAULT 1 CHECK (reservation_revision > 0);
