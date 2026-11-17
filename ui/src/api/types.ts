// Hand-written until the ts-rs types land in src/api/generated/. Wire rules: ids are strings,
// timestamps are RFC 3339 UTC, durations are microseconds as JSON numbers.

export interface Page<T> {
  data: T[];
  next_cursor: string | null;
  has_more: boolean;
}

export interface TraceListItem {
  trace_id: string;
  name: string;
  start_time: string;
  duration_us: number;
  span_count: number;
  error_count: number;
}

export interface TimeWindow {
  from: string;
  to: string;
}

export interface Problem {
  type?: string;
  title?: string;
  status?: number;
  detail?: string;
  code?: string;
  request_id?: string;
  errors?: { pointer?: string; code?: string; message?: string }[];
  retry_after_s?: number;
}
