-- Skill bodies the host expanded from a user's `@name` mentions, kept beside
-- the typed words instead of inside them. A JSON array of
-- {name, scope, body}; evidence without mentions holds `[]`.
ALTER TABLE review_evidence ADD COLUMN attachments_json TEXT NOT NULL DEFAULT '[]';
