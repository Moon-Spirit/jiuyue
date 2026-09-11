-- M13a: per-group file storage.
--
-- Every `kind='group'` conversation gets 1 GiB of free file storage. While the
-- group's CURRENT usage (sum of non-expired `bytes`) is below the quota, an
-- upload is permanent (`expires_at IS NULL`). Once usage is >= 1 GiB, new
-- uploads become TEMPORARY: valid for 7 days (`expires_at = now() + 7 days`).
-- Uploads are never blocked by the quota. An hourly sweeper deletes expired
-- rows and removes their disk files.
--
-- Bytes live on disk at
-- `{JIUYUE_MEDIA_DIR}/group-files/{conversation_id}/{file_id}.{ext}`; the
-- authoritative path is stored on the row (`storage_path`), because the path is
-- not derivable from `(created_at, mime)` the way M8 media is.
--
-- File listings and live `group.updated` nudges are REST-authoritative: the
-- frame is only a hint to refetch `/api/groups/{id}/files`.

CREATE TABLE group_files (
    id uuid PRIMARY KEY,
    conversation_id bigint NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    uploader_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name text NOT NULL,
    mime text NOT NULL DEFAULT 'application/octet-stream',
    bytes bigint NOT NULL,
    storage_path text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NULL
);
CREATE INDEX group_files_conversation_idx ON group_files(conversation_id);
CREATE INDEX group_files_expiry_idx ON group_files(expires_at) WHERE expires_at IS NOT NULL;
