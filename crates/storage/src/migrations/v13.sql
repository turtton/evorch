-- Retention seeks through diagnostic rows only. Each maintenance tick examines
-- a bounded page of this index, including protected/unrecognized diagnostics.
CREATE INDEX idx_events_diagnostic_retention ON events(wall_clock_ns, id)
WHERE kind = 'Diagnostic';
