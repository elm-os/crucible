-- Each node's own clock at its latest heartbeat, beside `heartbeat_at` (the database clock), so that the leader sees
-- how far every live node's clock is from the database clock before a project deletion drops a partition (contracts
-- 11.1 and 13.4). Null in a row that a version before this column wrote.
ALTER TABLE nodes ADD COLUMN node_clock timestamptz;
