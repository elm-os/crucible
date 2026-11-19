-- A project's salt goes with its project (contracts 11.1 and 13.4). The salt is now created in the transaction
-- that creates the project, so the row can reference its project, and deleting a project's row deletes its salt.
-- A salt whose project row is gone is removed first, as the cascade would have removed it. Forward-only.

DELETE FROM project_salts s WHERE NOT EXISTS (SELECT 1 FROM projects p WHERE p.id = s.project_id);

ALTER TABLE project_salts
    ADD CONSTRAINT project_salts_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id) ON DELETE CASCADE;
