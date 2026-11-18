// OTel ids on the wire are lowercase hex; uppercase is accepted and lowered, as the API does.
const TRACE_ID = /^[0-9a-f]{32}$/;
const SPAN_ID = /^[0-9a-f]{16}$/;

export function parseTraceId(value: string | null): string | null {
  const id = value?.trim().toLowerCase() ?? "";
  return TRACE_ID.test(id) ? id : null;
}

export function parseSpanId(value: string | null): string | null {
  const id = value?.trim().toLowerCase() ?? "";
  return SPAN_ID.test(id) ? id : null;
}
