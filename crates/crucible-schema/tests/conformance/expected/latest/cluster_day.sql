-- table: span_core
CREATE TABLE crucible.span_core
(
    `project_id` UUID CODEC(ZSTD(1)),
    `start_minute` DateTime('UTC') CODEC(ZSTD(1)),
    `trace_hash` UInt32 CODEC(ZSTD(1)),
    `start_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `end_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `parent_span_id` FixedString(8) CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `is_root` UInt8 CODEC(ZSTD(1)),
    `is_app_root` UInt8 CODEC(ZSTD(1)),
    `clock_skew` UInt8 CODEC(ZSTD(1)),
    `kind` Enum8('unspecified' = 0, 'internal' = 1, 'server' = 2, 'client' = 3, 'producer' = 4, 'consumer' = 5) CODEC(ZSTD(1)),
    `status_code` Enum8('unset' = 0, 'ok' = 1, 'error' = 2) CODEC(ZSTD(1)),
    `observation_type` Enum8('span' = 0, 'generation' = 1, 'event' = 2, 'embedding' = 3, 'agent' = 4, 'tool' = 5, 'chain' = 6, 'retriever' = 7, 'evaluator' = 8, 'guardrail' = 9) CODEC(ZSTD(1)),
    `level` Enum8('debug' = 0, 'default' = 1, 'warning' = 2, 'error' = 3) CODEC(ZSTD(1)),
    `trace_public` Enum8('unset' = 0, 'private' = 1, 'public' = 2) CODEC(ZSTD(1)),
    `cost_source` Enum8('none' = 0, 'provided' = 1, 'calculated' = 2) CODEC(ZSTD(1)),
    `mcp_side` Enum8('none' = 0, 'client' = 1, 'server' = 2) CODEC(ZSTD(1)),
    `io_shape` Enum8('raw' = 0, 'genai_messages' = 1, 'indexed_messages' = 2) CODEC(ZSTD(1)),
    `input_truncation` Enum8('none' = 0, 'cap_exceeded' = 1) CODEC(ZSTD(1)),
    `output_truncation` Enum8('none' = 0, 'cap_exceeded' = 1) CODEC(ZSTD(1)),
    `source_format` Enum8('otel' = 0, 'crucible_sdk' = 1, 'crucible_proxy' = 2, 'compat' = 3) CODEC(ZSTD(1)),
    `name` String CODEC(ZSTD(1)),
    `status_message` String CODEC(ZSTD(1)),
    `service_version` String CODEC(ZSTD(1)),
    `scope_version` String CODEC(ZSTD(1)),
    `telemetry_sdk_version` String CODEC(ZSTD(1)),
    `sdk_version` String CODEC(ZSTD(1)),
    `release` String CODEC(ZSTD(1)),
    `version` String CODEC(ZSTD(1)),
    `trace_name` String CODEC(ZSTD(1)),
    `user_id` String CODEC(ZSTD(1)),
    `session_id` String CODEC(ZSTD(1)),
    `prompt_name` String CODEC(ZSTD(1)),
    `prompt_version` String CODEC(ZSTD(1)),
    `service_name` LowCardinality(String) CODEC(ZSTD(1)),
    `scope_name` LowCardinality(String) CODEC(ZSTD(1)),
    `telemetry_sdk_name` LowCardinality(String) CODEC(ZSTD(1)),
    `telemetry_sdk_language` LowCardinality(String) CODEC(ZSTD(1)),
    `sdk_name` LowCardinality(String) CODEC(ZSTD(1)),
    `environment` LowCardinality(String) CODEC(ZSTD(1)),
    `model_requested` LowCardinality(String) CODEC(ZSTD(1)),
    `model_response` LowCardinality(String) CODEC(ZSTD(1)),
    `provider` LowCardinality(String) CODEC(ZSTD(1)),
    `duration_us` UInt64 CODEC(T64, ZSTD(1)),
    `input_tokens` UInt64 CODEC(T64, ZSTD(1)),
    `output_tokens` UInt64 CODEC(T64, ZSTD(1)),
    `total_tokens` UInt64 CODEC(T64, ZSTD(1)),
    `input_bytes` UInt64 CODEC(T64, ZSTD(1)),
    `output_bytes` UInt64 CODEC(T64, ZSTD(1)),
    `completion_start_time` Nullable(DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `trace_tags` Array(String) CODEC(ZSTD(1)),
    `tags` Array(String) CODEC(ZSTD(1)),
    `tool_names` Array(String) CODEC(ZSTD(1)),
    `finish_reasons` Array(LowCardinality(String)) CODEC(ZSTD(1)),
    `usage_keys` Array(LowCardinality(String)) CODEC(ZSTD(1)),
    `usage_values` Array(UInt64) CODEC(ZSTD(1)),
    `cost_values` Array(Int64) CODEC(ZSTD(1)),
    `total_cost_nusd` Int64 CODEC(T64, ZSTD(1)),
    `price_model_id` UUID CODEC(ZSTD(1)),
    `price_tier_id` UUID CODEC(ZSTD(1)),
    `api_key_id` UUID CODEC(ZSTD(1)),
    `tool_call_count` UInt32 CODEC(T64, ZSTD(1)),
    `normalizer_version` UInt32 CODEC(T64, ZSTD(1)),
    `event_bytes` UInt32 CODEC(T64, ZSTD(1)),
    `compat_dialect` UInt8 CODEC(ZSTD(1)),
    `input_preview` String CODEC(ZSTD(1)),
    `output_preview` String CODEC(ZSTD(1)),
    `input_sha256` FixedString(32) CODEC(ZSTD(1)),
    `output_sha256` FixedString(32) CODEC(ZSTD(1)),
    `row_digest` UInt64 CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `usage_aggregate` UInt8 CODEC(ZSTD(1)),
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_user_id user_id TYPE bloom_filter(0.01) GRANULARITY 2,
    INDEX idx_session_id session_id TYPE bloom_filter(0.01) GRANULARITY 2,
    INDEX idx_observation_type observation_type TYPE set(100) GRANULARITY 4,
    INDEX idx_level level TYPE set(100) GRANULARITY 4,
    INDEX idx_name name TYPE text(tokenizer = ngrams(3), preprocessor = lower(name)) GRANULARITY 100000000,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1,
    INDEX idx_received_at received_at TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/span_core', '{replica}')
PARTITION BY toDate(start_time)
PRIMARY KEY (project_id, start_minute, trace_hash)
ORDER BY (project_id, start_minute, trace_hash, start_time, span_id)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: span_payload
CREATE TABLE crucible.span_payload
(
    `project_id` UUID CODEC(ZSTD(1)),
    `start_minute` DateTime('UTC') CODEC(ZSTD(1)),
    `trace_hash` UInt32 CODEC(ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `start_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `user_id` String CODEC(ZSTD(1)),
    `session_id` String CODEC(ZSTD(1)),
    `input` String CODEC(ZSTD(3)),
    `output` String CODEC(ZSTD(3)),
    `system_instructions` String CODEC(ZSTD(3)),
    `attributes` String CODEC(ZSTD(3)),
    `resource_attributes` String CODEC(ZSTD(3)),
    `events` String CODEC(ZSTD(3)),
    `links` String CODEC(ZSTD(3)),
    `tool_definitions` String CODEC(ZSTD(3)),
    `tool_calls` String CODEC(ZSTD(3)),
    `model_parameters` String CODEC(ZSTD(3)),
    `trace_metadata` String CODEC(ZSTD(3)),
    `metadata_keys` Array(String) CODEC(ZSTD(1)),
    `metadata_values` Array(String) CODEC(ZSTD(1)),
    `metadata_kv` Array(String) CODEC(ZSTD(1)),
    `usage_provided_keys` Array(LowCardinality(String)) CODEC(ZSTD(1)),
    `cost_provided_keys` Array(LowCardinality(String)) CODEC(ZSTD(1)),
    `usage_provided_values` Array(UInt64) CODEC(ZSTD(1)),
    `cost_provided_values` Array(Int64) CODEC(ZSTD(1)),
    `trace_state` String CODEC(ZSTD(1)),
    `truncated_fields` Array(LowCardinality(String)) CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    INDEX idx_input input TYPE text(tokenizer = splitByNonAlpha, preprocessor = lower(input)) GRANULARITY 100000000,
    INDEX idx_output output TYPE text(tokenizer = splitByNonAlpha, preprocessor = lower(output)) GRANULARITY 100000000,
    INDEX idx_metadata_kv metadata_kv TYPE text(tokenizer = array) GRANULARITY 100000000,
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/span_payload', '{replica}')
PARTITION BY toDate(start_time)
PRIMARY KEY (project_id, start_minute, trace_hash)
ORDER BY (project_id, start_minute, trace_hash, span_id)
SETTINGS index_granularity = 1024, index_granularity_bytes = 16777216, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: mcp_ops
CREATE TABLE crucible.mcp_ops
(
    `project_id` UUID CODEC(ZSTD(1)),
    `side` Enum8('none' = 0, 'client' = 1, 'server' = 2) CODEC(ZSTD(1)),
    `capture_source` Enum8('sdk' = 1, 'proxy_stdio' = 2, 'proxy_http' = 3, 'otlp' = 4) CODEC(ZSTD(1)),
    `era` Enum8('legacy' = 1, 'modern' = 2) CODEC(ZSTD(1)),
    `transport` Enum8('unknown' = 0, 'stdio' = 1, 'streamable_http' = 2, 'sse' = 3) CODEC(ZSTD(1)),
    `target_kind` Enum8('none' = 0, 'tool' = 1, 'prompt' = 2, 'resource' = 3) CODEC(ZSTD(1)),
    `direction` Enum8('client_to_server' = 1, 'server_to_client' = 2) CODEC(ZSTD(1)),
    `result_type` Enum8('none' = 0, 'complete' = 1, 'input_required' = 2, 'task' = 3) CODEC(ZSTD(1)),
    `cancel_source` Enum8('none' = 0, 'notification' = 1, 'stream_close' = 2) CODEC(ZSTD(1)),
    `status` Enum8('ok' = 0, 'tool_error' = 1, 'protocol_error' = 2, 'transport_error' = 3, 'cancelled' = 4) CODEC(ZSTD(1)),
    `error_class` Enum8('none' = 0, 'protocol' = 1, 'validation' = 2, 'not_found' = 3, 'tool_execution' = 4, 'auth' = 5, 'rate_limit' = 6, 'timeout' = 7, 'transport' = 8, 'cancelled' = 9, 'internal' = 10) CODEC(ZSTD(1)),
    `error_subclass` Enum8('none' = 0, 'auth' = 1, 'rate_limit' = 2, 'upstream' = 3, 'business' = 4) CODEC(ZSTD(1)),
    `identity_source` Enum8('none' = 0, 'identify_hook' = 1, 'token_claims' = 2, 'token_verified' = 3) CODEC(ZSTD(1)),
    `group_key_kind` Enum8('legacy_session' = 1, 'handle' = 2, 'trace' = 3, 'process' = 4) CODEC(ZSTD(1)),
    `content_truncation` Enum8('none' = 0, 'cap_exceeded' = 1) CODEC(ZSTD(1)),
    `start_minute` DateTime('UTC') CODEC(ZSTD(1)),
    `trace_hash` UInt32 CODEC(ZSTD(1)),
    `start_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `parent_span_id` FixedString(8) CODEC(ZSTD(1)),
    `transport_link_span_id` FixedString(8) CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `transport_link_trace_id` FixedString(16) CODEC(ZSTD(1)),
    `user_subject_hash` FixedString(16) CODEC(ZSTD(1)),
    `mrtr_state_in` FixedString(16) CODEC(ZSTD(1)),
    `mrtr_state_out` FixedString(16) CODEC(ZSTD(1)),
    `server_name` LowCardinality(String) CODEC(ZSTD(1)),
    `method` LowCardinality(String) CODEC(ZSTD(1)),
    `target_name` LowCardinality(String) CODEC(ZSTD(1)),
    `client_name` LowCardinality(String) CODEC(ZSTD(1)),
    `server_version` String CODEC(ZSTD(1)),
    `deploy_id` String CODEC(ZSTD(1)),
    `instance_id` String CODEC(ZSTD(1)),
    `protocol_version` String CODEC(ZSTD(1)),
    `jsonrpc_id` String CODEC(ZSTD(1)),
    `task_id` String CODEC(ZSTD(1)),
    `subscription_id` String CODEC(ZSTD(1)),
    `error_type` String CODEC(ZSTD(1)),
    `error_message` String CODEC(ZSTD(1)),
    `client_version` String CODEC(ZSTD(1)),
    `oauth_client_id` String CODEC(ZSTD(1)),
    `user_id` String CODEC(ZSTD(1)),
    `user_issuer` String CODEC(ZSTD(1)),
    `tenant` String CODEC(ZSTD(1)),
    `session_key` String CODEC(ZSTD(1)),
    `handle_id` String CODEC(ZSTD(1)),
    `group_key` String CODEC(ZSTD(1)),
    `ip_prefix` String CODEC(ZSTD(1)),
    `user_agent` String CODEC(ZSTD(1)),
    `is_notification` UInt8 CODEC(ZSTD(1)),
    `is_batch` UInt8 CODEC(ZSTD(1)),
    `timed_out` UInt8 CODEC(ZSTD(1)),
    `caller_fault` UInt8 CODEC(ZSTD(1)),
    `content_captured` UInt8 CODEC(ZSTD(1)),
    `mrtr_round` UInt16 CODEC(ZSTD(1)),
    `duration_us` UInt64 CODEC(T64, ZSTD(1)),
    `request_bytes` UInt64 CODEC(T64, ZSTD(1)),
    `response_bytes` UInt64 CODEC(T64, ZSTD(1)),
    `client_capabilities_hash` UInt64 CODEC(ZSTD(1)),
    `progress_count` Nullable(UInt32) CODEC(T64, ZSTD(1)),
    `jsonrpc_code` Nullable(Int32) CODEC(ZSTD(1)),
    `http_status` Nullable(UInt16) CODEC(ZSTD(1)),
    `content_block_types` Array(LowCardinality(String)) CODEC(ZSTD(1)),
    `content_block_counts` Array(UInt32) CODEC(ZSTD(1)),
    `content_sample_rate` Float32 CODEC(ZSTD(1)),
    `normalizer_version` UInt32 CODEC(T64, ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_group_key group_key TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_user_subject_hash user_subject_hash TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_user_id user_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_error_class error_class TYPE set(100) GRANULARITY 1,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/mcp_ops', '{replica}')
PARTITION BY toDate(start_time)
PRIMARY KEY (project_id, side, start_minute, trace_hash)
ORDER BY (project_id, side, start_minute, trace_hash, span_id)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: mcp_content
CREATE TABLE crucible.mcp_content
(
    `project_id` UUID CODEC(ZSTD(1)),
    `side` Enum8('none' = 0, 'client' = 1, 'server' = 2) CODEC(ZSTD(1)),
    `start_minute` DateTime('UTC') CODEC(ZSTD(1)),
    `trace_hash` UInt32 CODEC(ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `start_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `user_id` String CODEC(ZSTD(1)),
    `user_subject_hash` FixedString(16) CODEC(ZSTD(1)),
    `target_name` LowCardinality(String) CODEC(ZSTD(1)),
    `args` String CODEC(ZSTD(3)),
    `result` String CODEC(ZSTD(3)),
    `args_bytes` UInt64 CODEC(T64, ZSTD(1)),
    `result_bytes` UInt64 CODEC(T64, ZSTD(1)),
    `content_truncation` Enum8('none' = 0, 'cap_exceeded' = 1) CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    INDEX idx_args args TYPE text(tokenizer = splitByNonAlpha, preprocessor = lower(args)) GRANULARITY 100000000,
    INDEX idx_result result TYPE text(tokenizer = splitByNonAlpha, preprocessor = lower(result)) GRANULARITY 100000000,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/mcp_content', '{replica}')
PARTITION BY toDate(start_time)
PRIMARY KEY (project_id, side, start_minute, trace_hash)
ORDER BY (project_id, side, start_minute, trace_hash, span_id)
SETTINGS index_granularity = 1024, index_granularity_bytes = 16777216, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: mcp_tool_inventory
CREATE TABLE crucible.mcp_tool_inventory
(
    `project_id` UUID CODEC(ZSTD(1)),
    `server_name` LowCardinality(String) CODEC(ZSTD(1)),
    `deploy_id` String CODEC(ZSTD(1)),
    `tool_name` LowCardinality(String) CODEC(ZSTD(1)),
    `schema_hash` UInt64 CODEC(ZSTD(1)),
    `marker_epoch` UUID CODEC(ZSTD(1)),
    `marker_batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `marker_at` SimpleAggregateFunction(max, DateTime('UTC')) CODEC(ZSTD(1)),
    `first_seen` SimpleAggregateFunction(min, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `last_seen` SimpleAggregateFunction(max, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `capture_sources` SimpleAggregateFunction(groupUniqArrayArray(4), Array(UInt8)) CODEC(ZSTD(1))
)
ENGINE = ReplicatedAggregatingMergeTree('/clickhouse/tables/{shard}/crucible/mcp_tool_inventory', '{replica}')
PRIMARY KEY (project_id, server_name, deploy_id, tool_name, schema_hash)
ORDER BY (project_id, server_name, deploy_id, tool_name, schema_hash, marker_epoch, marker_batch_seq)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: fact_scores
CREATE TABLE crucible.fact_scores
(
    `project_id` UUID CODEC(ZSTD(1)),
    `key` String CODEC(ZSTD(1)),
    `target_day` Date CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `session_id` String CODEC(ZSTD(1)),
    `version` UInt64 CODEC(T64, ZSTD(1)),
    `fact_id` UUID CODEC(ZSTD(1)),
    `target_kind` Enum8('trace' = 1, 'span' = 2, 'session' = 3) CODEC(ZSTD(1)),
    `target_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `retracted` UInt8 CODEC(ZSTD(1)),
    `author_kind` Enum8('user' = 1, 'api_key' = 2, 'system' = 3) CODEC(ZSTD(1)),
    `author_id` UUID CODEC(ZSTD(1)),
    `created_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `data_type` Enum8('numeric' = 1, 'categorical' = 2, 'boolean' = 3, 'text' = 4) CODEC(ZSTD(1)),
    `value_num` Float64 CODEC(ZSTD(1)),
    `value_str` String CODEC(ZSTD(1)),
    `comment` String CODEC(ZSTD(1)),
    `source` Enum8('feedback' = 1, 'api' = 2, 'annotation' = 3, 'eval' = 4) CODEC(ZSTD(1)),
    `metadata` String CODEC(ZSTD(1)),
    `config_id` UUID CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_session_id session_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/fact_scores', '{replica}')
PARTITION BY toYYYYMM(target_time)
PRIMARY KEY (project_id, key, target_day, trace_id)
ORDER BY (project_id, key, target_day, trace_id, span_id, session_id, version, fact_id)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: fact_tags
CREATE TABLE crucible.fact_tags
(
    `project_id` UUID CODEC(ZSTD(1)),
    `key` String CODEC(ZSTD(1)),
    `target_day` Date CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `session_id` String CODEC(ZSTD(1)),
    `version` UInt64 CODEC(T64, ZSTD(1)),
    `fact_id` UUID CODEC(ZSTD(1)),
    `target_kind` Enum8('trace' = 1, 'span' = 2, 'session' = 3) CODEC(ZSTD(1)),
    `target_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `retracted` UInt8 CODEC(ZSTD(1)),
    `author_kind` Enum8('user' = 1, 'api_key' = 2, 'system' = 3) CODEC(ZSTD(1)),
    `author_id` UUID CODEC(ZSTD(1)),
    `created_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `op` Enum8('add' = 1, 'remove' = 2) CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_session_id session_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/fact_tags', '{replica}')
PARTITION BY toYYYYMM(target_time)
PRIMARY KEY (project_id, key, target_day, trace_id)
ORDER BY (project_id, key, target_day, trace_id, span_id, session_id, version, fact_id)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: fact_flags
CREATE TABLE crucible.fact_flags
(
    `project_id` UUID CODEC(ZSTD(1)),
    `key` String CODEC(ZSTD(1)),
    `target_day` Date CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `session_id` String CODEC(ZSTD(1)),
    `version` UInt64 CODEC(T64, ZSTD(1)),
    `fact_id` UUID CODEC(ZSTD(1)),
    `target_kind` Enum8('trace' = 1, 'span' = 2, 'session' = 3) CODEC(ZSTD(1)),
    `target_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `retracted` UInt8 CODEC(ZSTD(1)),
    `author_kind` Enum8('user' = 1, 'api_key' = 2, 'system' = 3) CODEC(ZSTD(1)),
    `author_id` UUID CODEC(ZSTD(1)),
    `created_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `value` UInt8 CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_session_id session_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/fact_flags', '{replica}')
PARTITION BY toYYYYMM(target_time)
PRIMARY KEY (project_id, key, target_day, trace_id)
ORDER BY (project_id, key, target_day, trace_id, span_id, session_id, version, fact_id)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: fact_corrections
CREATE TABLE crucible.fact_corrections
(
    `project_id` UUID CODEC(ZSTD(1)),
    `key` String CODEC(ZSTD(1)),
    `target_day` Date CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `session_id` String CODEC(ZSTD(1)),
    `version` UInt64 CODEC(T64, ZSTD(1)),
    `fact_id` UUID CODEC(ZSTD(1)),
    `target_kind` Enum8('trace' = 1, 'span' = 2, 'session' = 3) CODEC(ZSTD(1)),
    `target_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `retracted` UInt8 CODEC(ZSTD(1)),
    `author_kind` Enum8('user' = 1, 'api_key' = 2, 'system' = 3) CODEC(ZSTD(1)),
    `author_id` UUID CODEC(ZSTD(1)),
    `created_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `corrected` String CODEC(ZSTD(3)),
    `note` String CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_session_id session_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/fact_corrections', '{replica}')
PARTITION BY toYYYYMM(target_time)
PRIMARY KEY (project_id, key, target_day, trace_id)
ORDER BY (project_id, key, target_day, trace_id, span_id, session_id, version, fact_id)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: trace_heads
CREATE TABLE crucible.trace_heads
(
    `project_id` UUID CODEC(ZSTD(1)),
    `start_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `trace_hash` UInt32 CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `span_id` FixedString(8) CODEC(ZSTD(1)),
    `head_kind` Enum8('root' = 1, 'provisional' = 2) CODEC(ZSTD(1)),
    `end_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    `name` String CODEC(ZSTD(1)),
    `level` Enum8('debug' = 0, 'default' = 1, 'warning' = 2, 'error' = 3) CODEC(ZSTD(1)),
    `status_code` Enum8('unset' = 0, 'ok' = 1, 'error' = 2) CODEC(ZSTD(1)),
    `user_id` String CODEC(ZSTD(1)),
    `session_id` String CODEC(ZSTD(1)),
    `tags` Array(String) CODEC(ZSTD(1)),
    `environment` LowCardinality(String) CODEC(ZSTD(1)),
    `release` String CODEC(ZSTD(1)),
    `version` String CODEC(ZSTD(1)),
    `input_preview` String CODEC(ZSTD(1)),
    `output_preview` String CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `lsn` UInt64 CODEC(T64, ZSTD(1)),
    `received_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1)),
    INDEX idx_trace_id trace_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_user_id user_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_session_id session_id TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_name name TYPE text(tokenizer = ngrams(3), preprocessor = lower(name)) GRANULARITY 100000000,
    INDEX idx_tags tags TYPE text(tokenizer = array) GRANULARITY 100000000,
    INDEX idx_environment environment TYPE set(100) GRANULARITY 1,
    INDEX idx_batch_seq batch_seq TYPE minmax GRANULARITY 1,
    INDEX idx_received_at received_at TYPE minmax GRANULARITY 1
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/trace_heads', '{replica}')
PARTITION BY toDate(start_time)
PRIMARY KEY (project_id, start_time, trace_hash)
ORDER BY (project_id, start_time, trace_hash, trace_id)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: trace_stats
CREATE TABLE crucible.trace_stats
(
    `project_id` UUID CODEC(ZSTD(1)),
    `trace_id` FixedString(16) CODEC(ZSTD(1)),
    `row_day` Date CODEC(ZSTD(1)),
    `marker_epoch` UUID CODEC(ZSTD(1)),
    `marker_batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `marker_at` SimpleAggregateFunction(max, DateTime('UTC')) CODEC(ZSTD(1)),
    `min_start` SimpleAggregateFunction(min, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `max_end` SimpleAggregateFunction(max, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `has_root` SimpleAggregateFunction(max, UInt8) CODEC(ZSTD(1)),
    `min_head_start` SimpleAggregateFunction(min, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `max_level` SimpleAggregateFunction(max, UInt8) CODEC(ZSTD(1)),
    `span_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `error_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `generation_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `tool_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `mcp_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `input_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `output_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `total_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `total_cost_nusd` SimpleAggregateFunction(sum, Int64) CODEC(T64, ZSTD(1)),
    `environments` SimpleAggregateFunction(groupUniqArrayArray(16), Array(String)) CODEC(ZSTD(1)),
    `trace_name_pick` SimpleAggregateFunction(max, String) CODEC(ZSTD(1)),
    `user_id_pick` SimpleAggregateFunction(max, String) CODEC(ZSTD(1)),
    `session_id_pick` SimpleAggregateFunction(max, String) CODEC(ZSTD(1))
)
ENGINE = ReplicatedAggregatingMergeTree('/clickhouse/tables/{shard}/crucible/trace_stats', '{replica}')
PARTITION BY row_day
PRIMARY KEY (project_id, trace_id)
ORDER BY (project_id, trace_id, row_day, marker_epoch, marker_batch_seq)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: session_rollup
CREATE TABLE crucible.session_rollup
(
    `project_id` UUID CODEC(ZSTD(1)),
    `day` Date CODEC(ZSTD(1)),
    `session_id` String CODEC(ZSTD(1)),
    `marker_epoch` UUID CODEC(ZSTD(1)),
    `marker_batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `marker_at` SimpleAggregateFunction(max, DateTime('UTC')) CODEC(ZSTD(1)),
    `first_seen` SimpleAggregateFunction(min, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `last_seen` SimpleAggregateFunction(max, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `trace_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `span_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `error_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `input_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `output_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `total_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `total_cost_nusd` SimpleAggregateFunction(sum, Int64) CODEC(T64, ZSTD(1)),
    `user_ids` SimpleAggregateFunction(groupUniqArrayArray(8), Array(String)) CODEC(ZSTD(1)),
    INDEX idx_session_id session_id TYPE bloom_filter(0.01) GRANULARITY 1
)
ENGINE = ReplicatedAggregatingMergeTree('/clickhouse/tables/{shard}/crucible/session_rollup', '{replica}')
PARTITION BY toYYYYMM(day)
PRIMARY KEY (project_id, day, session_id)
ORDER BY (project_id, day, session_id, marker_epoch, marker_batch_seq)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: user_rollup
CREATE TABLE crucible.user_rollup
(
    `project_id` UUID CODEC(ZSTD(1)),
    `day` Date CODEC(ZSTD(1)),
    `user_id` String CODEC(ZSTD(1)),
    `marker_epoch` UUID CODEC(ZSTD(1)),
    `marker_batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `marker_at` SimpleAggregateFunction(max, DateTime('UTC')) CODEC(ZSTD(1)),
    `first_seen` SimpleAggregateFunction(min, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `last_seen` SimpleAggregateFunction(max, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `trace_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `span_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `error_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `input_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `output_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `total_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `total_cost_nusd` SimpleAggregateFunction(sum, Int64) CODEC(T64, ZSTD(1)),
    INDEX idx_user_id user_id TYPE bloom_filter(0.01) GRANULARITY 1
)
ENGINE = ReplicatedAggregatingMergeTree('/clickhouse/tables/{shard}/crucible/user_rollup', '{replica}')
PARTITION BY toYYYYMM(day)
PRIMARY KEY (project_id, day, user_id)
ORDER BY (project_id, day, user_id, marker_epoch, marker_batch_seq)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: span_metrics
CREATE TABLE crucible.span_metrics
(
    `project_id` UUID CODEC(ZSTD(1)),
    `minute` DateTime('UTC') CODEC(ZSTD(1)),
    `environment` LowCardinality(String) CODEC(ZSTD(1)),
    `observation_type` Enum8('span' = 0, 'generation' = 1, 'event' = 2, 'embedding' = 3, 'agent' = 4, 'tool' = 5, 'chain' = 6, 'retriever' = 7, 'evaluator' = 8, 'guardrail' = 9) CODEC(ZSTD(1)),
    `model_response` LowCardinality(String) CODEC(ZSTD(1)),
    `level` Enum8('debug' = 0, 'default' = 1, 'warning' = 2, 'error' = 3) CODEC(ZSTD(1)),
    `release` String CODEC(ZSTD(1)),
    `marker_epoch` UUID CODEC(ZSTD(1)),
    `marker_batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `marker_at` SimpleAggregateFunction(max, DateTime('UTC')) CODEC(ZSTD(1)),
    `count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `error_count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `input_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `output_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `total_tokens` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `total_cost_nusd` SimpleAggregateFunction(sum, Int64) CODEC(T64, ZSTD(1)),
    `dur_hist` SimpleAggregateFunction(sumMap, Tuple(
        Array(UInt16),
        Array(UInt64))) CODEC(ZSTD(1)),
    `ttft_hist` SimpleAggregateFunction(sumMap, Tuple(
        Array(UInt16),
        Array(UInt64))) CODEC(ZSTD(1))
)
ENGINE = ReplicatedAggregatingMergeTree('/clickhouse/tables/{shard}/crucible/span_metrics', '{replica}')
PARTITION BY toYYYYMM(minute)
PRIMARY KEY (project_id, minute)
ORDER BY (project_id, minute, environment, observation_type, model_response, level, release, marker_epoch, marker_batch_seq)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: mcp_tool_rollup
CREATE TABLE crucible.mcp_tool_rollup
(
    `project_id` UUID CODEC(ZSTD(1)),
    `side` Enum8('none' = 0, 'client' = 1, 'server' = 2) CODEC(ZSTD(1)),
    `minute` DateTime('UTC') CODEC(ZSTD(1)),
    `server_name` LowCardinality(String) CODEC(ZSTD(1)),
    `deploy_id` String CODEC(ZSTD(1)),
    `capture_source` Enum8('sdk' = 1, 'proxy_stdio' = 2, 'proxy_http' = 3, 'otlp' = 4) CODEC(ZSTD(1)),
    `era` Enum8('legacy' = 1, 'modern' = 2) CODEC(ZSTD(1)),
    `method` LowCardinality(String) CODEC(ZSTD(1)),
    `target_name` LowCardinality(String) CODEC(ZSTD(1)),
    `client_name` LowCardinality(String) CODEC(ZSTD(1)),
    `error_class` Enum8('none' = 0, 'protocol' = 1, 'validation' = 2, 'not_found' = 3, 'tool_execution' = 4, 'auth' = 5, 'rate_limit' = 6, 'timeout' = 7, 'transport' = 8, 'cancelled' = 9, 'internal' = 10) CODEC(ZSTD(1)),
    `caller_fault` UInt8 CODEC(ZSTD(1)),
    `marker_epoch` UUID CODEC(ZSTD(1)),
    `marker_batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `marker_at` SimpleAggregateFunction(max, DateTime('UTC')) CODEC(ZSTD(1)),
    `count` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `request_bytes` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `response_bytes` SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    `dur_hist` SimpleAggregateFunction(sumMap, Tuple(
        Array(UInt16),
        Array(UInt64))) CODEC(ZSTD(1)),
    `semconv_hist` SimpleAggregateFunction(sumMap, Tuple(
        Array(UInt8),
        Array(UInt64))) CODEC(ZSTD(1)),
    `last_seen` SimpleAggregateFunction(max, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1))
)
ENGINE = ReplicatedAggregatingMergeTree('/clickhouse/tables/{shard}/crucible/mcp_tool_rollup', '{replica}')
PARTITION BY toYYYYMM(minute)
PRIMARY KEY (project_id, side, minute)
ORDER BY (project_id, side, minute, server_name, deploy_id, capture_source, era, method, target_name, client_name, error_class, caller_fault, marker_epoch, marker_batch_seq)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: mcp_deploys
CREATE TABLE crucible.mcp_deploys
(
    `project_id` UUID CODEC(ZSTD(1)),
    `server_name` LowCardinality(String) CODEC(ZSTD(1)),
    `deploy_id` String CODEC(ZSTD(1)),
    `server_version` String CODEC(ZSTD(1)),
    `capture_source` Enum8('sdk' = 1, 'proxy_stdio' = 2, 'proxy_http' = 3, 'otlp' = 4) CODEC(ZSTD(1)),
    `marker_epoch` UUID CODEC(ZSTD(1)),
    `marker_batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `marker_at` SimpleAggregateFunction(max, DateTime('UTC')) CODEC(ZSTD(1)),
    `first_seen` SimpleAggregateFunction(min, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1)),
    `last_seen` SimpleAggregateFunction(max, DateTime64(6, 'UTC')) CODEC(Delta(8), ZSTD(1))
)
ENGINE = ReplicatedAggregatingMergeTree('/clickhouse/tables/{shard}/crucible/mcp_deploys', '{replica}')
PRIMARY KEY (project_id, server_name, deploy_id)
ORDER BY (project_id, server_name, deploy_id, server_version, capture_source, marker_epoch, marker_batch_seq)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: dedup_selftest
CREATE TABLE crucible.dedup_selftest
(
    `id` UUID CODEC(ZSTD(1)),
    `wal_epoch` UUID CODEC(ZSTD(1)),
    `batch_seq` UInt64 CODEC(T64, ZSTD(1)),
    `created_at` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1))
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/dedup_selftest', '{replica}')
PRIMARY KEY id
ORDER BY id
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
-- table: sql_probe
CREATE TABLE crucible.sql_probe
(
    `project_id` UUID CODEC(ZSTD(1)),
    `id` UInt32 CODEC(T64, ZSTD(1)),
    `canary` String CODEC(ZSTD(1)),
    `start_time` DateTime64(6, 'UTC') CODEC(Delta(8), ZSTD(1))
)
ENGINE = ReplicatedMergeTree('/clickhouse/tables/{shard}/crucible/sql_probe', '{replica}')
PRIMARY KEY project_id
ORDER BY (project_id, id)
SETTINGS index_granularity = 8192, replicated_deduplication_window = 20000, replicated_deduplication_window_seconds = 7200
