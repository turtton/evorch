-- Receipts survive candidate retention: replay must not increase occurrences or quotas.
CREATE TABLE improvement_observations (
    observation_key TEXT PRIMARY KEY NOT NULL
) WITHOUT ROWID;
