-- Restores the `run_context_provenance` shape migration v41 replaced (the whole map in
-- `map_json`), for tests that rebuild an older schema by hand and replay the migrations
-- from there. Every map is rebuilt from the message rows, so that a conversion of the
-- result can be compared with the original.
ALTER TABLE run_context_provenance ADD COLUMN map_json TEXT NOT NULL DEFAULT '{}';
UPDATE run_context_provenance SET map_json = json_object(
    'version', map_version,
    'raw_message_count', raw_message_count,
    'checkpoint_count', checkpoint_count,
    'messages', COALESCE((
        SELECT json_group_array(json(entry_json)) FROM (
            SELECT entry_json FROM run_message_provenance
            WHERE run_id = run_context_provenance.run_id ORDER BY map_index)), json('[]')));
DROP TABLE run_message_provenance;
ALTER TABLE run_context_provenance DROP COLUMN map_version;
ALTER TABLE run_context_provenance DROP COLUMN raw_message_count;
ALTER TABLE run_context_provenance DROP COLUMN checkpoint_count;
ALTER TABLE run_context_provenance DROP COLUMN map_message_count;
