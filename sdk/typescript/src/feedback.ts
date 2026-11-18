// Feedback is planned: it will post a score with source `feedback` to `POST /api/v1/projects/{id}/feedback`
// (contract 12.6). The API has no facts routes yet, so this stub refuses without sending anything.

/** A score on a trace, span or session. */
export interface Feedback {
  /** Score name, for example `helpful`. */
  name: string;
  value: number | string | boolean;
  traceId?: string;
  spanId?: string;
  sessionId?: string;
  comment?: string;
}

/** Planned. Always rejects in this release, because the Crucible API has no feedback route yet. */
export function feedback(_feedback: Feedback): Promise<never> {
  return Promise.reject(
    new Error(
      "Crucible feedback() is planned and not available in this release: the Crucible API has no feedback route yet, so nothing was sent.",
    ),
  );
}
