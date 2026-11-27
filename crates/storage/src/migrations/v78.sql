-- Evaluation sample kind, source references, tags and checks (ADR 0036): one JSON object.
ALTER TABLE eval_samples ADD COLUMN spec TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(spec) AND json_type(spec) = 'object');
