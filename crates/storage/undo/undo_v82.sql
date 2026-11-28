ALTER TABLE host_chat_submissions DROP COLUMN edit_original_text;
ALTER TABLE host_chat_submissions DROP COLUMN edit_attachment_count;
ALTER TABLE host_chat_submissions DROP COLUMN edit_image_count;
ALTER TABLE host_chat_submissions DROP COLUMN edit_preview_truncated;
ALTER TABLE host_chat_submissions DROP COLUMN edit_preview;
PRAGMA user_version = 81;
