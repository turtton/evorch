-- An explicit cancellation or a local startup failure is not a crash intent.
ALTER TABLE lanes ADD COLUMN start_blocked_reason TEXT;
