-- Skill bodies the host expanded from a user's `@name` mentions, kept beside
-- the typed words instead of inside them. A JSON array of
-- {name, scope, body}; evidence without mentions holds `[]`.
ALTER TABLE review_evidence ADD COLUMN attachments_json TEXT NOT NULL DEFAULT '[]';
-- Each accepted submission names the evidence row it stands for. Evidence is
-- immutable and keyed by content, so the same words and skills sent twice are
-- one row; the receipts (one per submission) are what keep the repeats of
-- attachments, and acceptance_seq, assigned in the transaction that accepts
-- the submission, keeps their order.
ALTER TABLE host_input_receipts ADD COLUMN evidence_id TEXT;
ALTER TABLE host_input_receipts ADD COLUMN acceptance_seq INTEGER;
UPDATE host_input_receipts SET acceptance_seq = rowid;
CREATE INDEX host_input_receipts_seq ON host_input_receipts(acceptance_seq);
