-- Sign-in providers carry a version (contract 12.5): a change or a delete through the API sends the version it read
-- in If-Match. The providers that exist start at version 1. Forward-only; no column has a default (contract 2.7).

ALTER TABLE auth_providers ADD COLUMN version bigint;
UPDATE auth_providers SET version = 1;
ALTER TABLE auth_providers ALTER COLUMN version SET NOT NULL;
