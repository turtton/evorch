-- Data a client needs to edit or restore an accepted chat message: a short preview, how many
-- attachments (and images) it carried, and its words as typed when it had attachments.
-- Nullable ADD COLUMNs only: no backfill, and a row without a preview cannot be edited.
-- The words come last so reading a preview never touches the pages of a large body.
ALTER TABLE host_chat_submissions ADD COLUMN edit_preview TEXT;
ALTER TABLE host_chat_submissions ADD COLUMN edit_preview_truncated INTEGER;
ALTER TABLE host_chat_submissions ADD COLUMN edit_image_count INTEGER;
ALTER TABLE host_chat_submissions ADD COLUMN edit_attachment_count INTEGER;
ALTER TABLE host_chat_submissions ADD COLUMN edit_original_text TEXT;
