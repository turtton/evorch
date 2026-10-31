-- A provider may finish a request without reporting token usage (Codex does). Those
-- rows keep zero token counts as placeholders, so the flag tells "unknown" from a
-- reported zero. Daily rollups count them so a total stays marked incomplete.
ALTER TABLE usage_requests ADD COLUMN usage_reported INTEGER NOT NULL DEFAULT 1;
ALTER TABLE usage_daily ADD COLUMN unreported_count INTEGER NOT NULL DEFAULT 0;
