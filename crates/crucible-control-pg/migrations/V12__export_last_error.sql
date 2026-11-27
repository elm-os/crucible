-- Export jobs keep what went wrong in their latest failed attempt (contract 12.8): `get_export` shows it while
-- the export waits for a later attempt, and once it has failed. Forward-only; the column has no default
-- (contract 2.7), and the exports that exist have no message.

ALTER TABLE export_jobs ADD COLUMN last_error text;
