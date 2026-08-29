-- Nested folders: a folder may live inside another folder.
-- NULL parent_id means the folder sits at the top level of the sidebar.

ALTER TABLE folders ADD COLUMN parent_id TEXT REFERENCES folders(id);

CREATE INDEX IF NOT EXISTS idx_folders_parent_id ON folders(parent_id);
