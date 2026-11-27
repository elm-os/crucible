"use client";

import { type UseQueryResult, useMutation, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import { Textarea } from "@ui-sdk/react/textarea";
import { Toggle } from "@ui-sdk/react/toggle";
import { type FormEvent, useRef, useState } from "react";
import { newIdempotencyKey, writeFeedback, writeFlag, writeTag } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { FlagName } from "@/api/generated/FlagName";
import type { Score } from "@/api/generated/Score";
import type { ScoreValue } from "@/api/generated/ScoreValue";
import type { TraceDetail } from "@/api/generated/TraceDetail";
import type { TraceFacts } from "@/api/generated/TraceFacts";
import { ErrorCode } from "@/components/error-code";
import { failureCopy } from "@/components/form-alert";
import { AlertIcon, CheckIcon } from "@/components/icons";
import { SectionFrame, useLoadingPhase } from "@/components/section";

const TAG_BYTES = 256;
const COMMENT_BYTES = 4_096;

const bytes = (text: string) => new TextEncoder().encode(text).length;

function tagProblem(text: string): string | null {
  const tag = text.trim();
  if (tag === "") return "Enter a tag.";
  return bytes(tag) > TAG_BYTES ? "A tag is at most 256 bytes." : null;
}

function valueProblem(text: string): string | undefined {
  return parseScoreValue(text) === null ? "Enter a number, true or false, or a word." : undefined;
}

function nameProblem(text: string): string | null {
  const name = text.trim();
  return name === "" || name.includes("#") ? "Enter a name without #." : null;
}

/** What this page wrote and the server acknowledged, kept until a refetch shows it. */
interface Acked {
  tags: Map<string, "add" | "remove">;
  flags: Map<FlagName, boolean>;
  scores: Score[];
}

const NOTHING_ACKED: Acked = { tags: new Map(), flags: new Map(), scores: [] };

const scoreKey = (s: Score) => `${s.name}#${s.score_id ?? ""}`;

// A fact is durable when acknowledged, but a read may come from a store that has not received it yet,
// so the acknowledged value stays on screen until the server's answer agrees with it.
export function shownFacts(server: TraceFacts, acked: Acked): TraceFacts {
  const tags = new Set(server.tags);
  for (const [tag, op] of acked.tags) {
    if (op === "add") tags.add(tag);
    else tags.delete(tag);
  }
  const scores = new Map(server.scores.map((s) => [scoreKey(s), s]));
  for (const score of acked.scores) {
    const held = scores.get(scoreKey(score));
    if (!held || held.fact.version < score.fact.version) scores.set(scoreKey(score), score);
  }
  return {
    ...server,
    tags: [...tags].toSorted(),
    bookmarked: acked.flags.get("bookmarked") ?? server.bookmarked,
    public: acked.flags.get("public") ?? server.public,
    scores: [...scores.values()].toSorted((a, b) => scoreKey(a).localeCompare(scoreKey(b))),
  };
}

/** Parses what a person typed as a score value: a number, true or false, or else text. */
export function parseScoreValue(text: string): ScoreValue | null {
  const trimmed = text.trim();
  if (trimmed === "") return null;
  if (trimmed === "true" || trimmed === "false") return trimmed === "true";
  const number = Number(trimmed);
  return Number.isFinite(number) ? number : trimmed;
}

function scoreText(score: Score): string {
  if (typeof score.value === "boolean") return score.value ? "Yes" : "No";
  return String(score.value);
}

export function TraceFactsSection({
  project,
  trace,
  query,
}: {
  project: string;
  trace: string;
  query: UseQueryResult<TraceDetail>;
}) {
  const phase = useLoadingPhase(query.isPending);
  if (query.isPending) {
    return (
      <SectionFrame title="Tags, flags and feedback" busy>
        <div aria-hidden="true" className={phase === "hidden" ? "invisible" : "flex gap-2"}>
          <div className="skeleton h-7 w-24" />
          <div className="skeleton h-7 w-24" />
        </div>
      </SectionFrame>
    );
  }
  // A failed or empty trace shows its own state in the spans section.
  const detail = query.data;
  if (!detail || detail.spans.length === 0) return null;
  return <FactsEditor project={project} trace={trace} server={detail.facts} />;
}

function FactsEditor({
  project,
  trace,
  server,
}: {
  project: string;
  trace: string;
  server: TraceFacts;
}) {
  const queryClient = useQueryClient();
  const [acked, setAcked] = useState<Acked>(NOTHING_ACKED);
  const facts = shownFacts(server, acked);
  const refresh = () => queryClient.invalidateQueries({ queryKey: [project, "trace", trace] });

  const flag = useMutation({
    mutationFn: (v: { flag: FlagName; value: boolean; key: string }) =>
      writeFlag(project, { trace_id: trace, flag: v.flag, value: v.value }, v.key),
    onSuccess: (_, v) => {
      setAcked((a) => ({ ...a, flags: new Map(a.flags).set(v.flag, v.value) }));
      void refresh();
    },
  });
  const tag = useMutation({
    mutationFn: (v: { tag: string; op: "add" | "remove"; key: string }) =>
      writeTag(project, { trace_id: trace, tag: v.tag, op: v.op }, v.key),
    onSuccess: (_, v) => {
      setAcked((a) => ({ ...a, tags: new Map(a.tags).set(v.tag, v.op) }));
      void refresh();
    },
  });

  return (
    <SectionFrame title="Tags, flags and feedback">
      <div className="flex flex-col gap-4">
        <div className="flex flex-wrap items-center gap-2">
          {(["bookmarked", "public"] as const).map((name) => {
            const on = name === "bookmarked" ? facts.bookmarked : facts.public === true;
            return (
              <Toggle
                key={name}
                pressed={on}
                disabled={flag.isPending}
                onPressedChange={(value) =>
                  flag.mutate({ flag: name, value, key: newIdempotencyKey() })
                }
                className="text-data"
              >
                {name === "bookmarked" ? "Bookmark" : "Public"}
              </Toggle>
            );
          })}
          {flag.isError ? <Problem failed="Couldn't save the flag." error={flag.error} /> : null}
        </div>
        <TagsEditor
          tags={facts.tags}
          pending={tag.isPending ? tag.variables : undefined}
          failed={tag.isError ? tag.error : null}
          onEdit={(name, op) => tag.mutate({ tag: name, op, key: newIdempotencyKey() })}
        />
        <Scores
          scores={facts.scores}
          project={project}
          trace={trace}
          onWritten={(score) => {
            setAcked((a) => ({ ...a, scores: [...a.scores, score] }));
            void refresh();
          }}
        />
      </div>
    </SectionFrame>
  );
}

// A failed write, beside the control that made it, in words for the kind of failure.
function Problem({ failed, error }: { failed: string; error: unknown }) {
  const copy = failureCopy(asUiError(error), failed);
  return (
    <div role="alert" className="flex flex-col items-start gap-1 text-label">
      <p className="flex items-center gap-1.5">
        <AlertIcon className="shrink-0" />
        {copy.title} {copy.detail}
      </p>
      <ErrorCode code={copy.reference ?? null} />
    </div>
  );
}

function TagsEditor({
  tags,
  pending,
  failed,
  onEdit,
}: {
  tags: string[];
  pending: { tag: string; op: "add" | "remove" } | undefined;
  failed: unknown;
  onEdit: (tag: string, op: "add" | "remove") => void;
}) {
  const [draft, setDraft] = useState("");
  const [invalid, setInvalid] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    const problem = tagProblem(draft);
    setInvalid(problem);
    if (problem !== null) {
      input.current?.focus();
      return;
    }
    setDraft("");
    onEdit(draft.trim(), "add");
  };
  return (
    <div className="flex flex-col gap-2">
      <h3 className="text-label font-medium text-ink-soft">Tags</h3>
      {tags.length === 0 ? (
        <p data-empty="tags" className="text-data text-ink-soft">
          No tags yet.
        </p>
      ) : (
        <ul className="flex flex-wrap gap-2">
          {tags.map((tag) => (
            <li
              key={tag}
              className="flex items-center gap-1 rounded-md border border-ash-border bg-parchment-white ps-2 text-data"
            >
              {tag}
              <Button
                aria-label={`Remove tag ${tag}`}
                disabled={pending?.tag === tag}
                onClick={() => onEdit(tag, "remove")}
                className="px-2"
              >
                ×
              </Button>
            </li>
          ))}
        </ul>
      )}
      <form onSubmit={submit} className="flex flex-wrap items-end gap-2" noValidate>
        <Field name="tag" invalid={invalid !== null ? true : undefined}>
          <FieldLabel className="text-label">New tag</FieldLabel>
          <Input
            ref={input}
            value={draft}
            onValueChange={(value) => {
              setDraft(value);
              // Once an error shows, it clears the moment the value is fixed.
              if (invalid !== null) setInvalid(tagProblem(value));
            }}
            className="text-data"
          />
          <FieldDescription className="text-label text-ink-soft">Up to 256 bytes.</FieldDescription>
          {invalid ? <FieldError match>{invalid}</FieldError> : null}
        </Field>
        <Button type="submit" disabled={pending !== undefined}>
          {pending?.op === "add" ? "Adding tag" : "Add tag"}
        </Button>
      </form>
      {failed ? <Problem failed="Couldn't save the tag." error={failed} /> : null}
    </div>
  );
}

function Scores({
  scores,
  project,
  trace,
  onWritten,
}: {
  scores: Score[];
  project: string;
  trace: string;
  onWritten: (score: Score) => void;
}) {
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [comment, setComment] = useState("");
  const [invalid, setInvalid] = useState<{ name?: string; value?: string }>({});
  const nameInput = useRef<HTMLInputElement>(null);
  const valueInput = useRef<HTMLInputElement>(null);
  const write = useMutation({
    mutationFn: (v: { name: string; value: ScoreValue; comment: string; key: string }) =>
      writeFeedback(
        project,
        { trace_id: trace, name: v.name, value: v.value, comment: v.comment || undefined },
        v.key,
      ),
    onSuccess: (written, v) => {
      setName("");
      setValue("");
      setComment("");
      onWritten({
        name: v.name,
        score_id: null,
        data_type:
          typeof v.value === "number"
            ? "numeric"
            : typeof v.value === "boolean"
              ? "boolean"
              : "categorical",
        value: v.value,
        source: "feedback",
        comment: v.comment,
        metadata: null,
        fact: {
          fact_id: written.fact_id,
          version: written.version,
          created_at: written.created_at,
          author_kind: "api_key",
          author_id: "",
        },
      });
    },
  });
  const submit = (event: FormEvent) => {
    event.preventDefault();
    const parsed = parseScoreValue(value);
    const problems = { name: nameProblem(name) ?? undefined, value: valueProblem(value) };
    setInvalid(problems);
    if (problems.name) nameInput.current?.focus();
    else if (problems.value) valueInput.current?.focus();
    if (problems.name || problems.value || parsed === null) return;
    write.mutate({
      name: name.trim(),
      value: parsed,
      comment: comment.trim(),
      key: newIdempotencyKey(),
    });
  };
  return (
    <div className="flex flex-col gap-2">
      <h3 className="text-label font-medium text-ink-soft">Scores and feedback</h3>
      {scores.length === 0 ? (
        <p data-empty="scores" className="text-data text-ink-soft">
          No scores or feedback yet.
        </p>
      ) : (
        <ul className="flex flex-col">
          {scores.map((score) => (
            <li
              key={`${score.name}#${score.score_id ?? ""}`}
              data-score={score.name}
              className="flex flex-wrap items-baseline gap-x-3 border-t border-ash-border py-1.5 text-data first:border-t-0"
            >
              <span className="font-medium">{score.name}</span>
              <span className="tabular-nums">{scoreText(score)}</span>
              {score.comment ? <span className="text-ink-soft">{score.comment}</span> : null}
              <span className="text-label text-ink-soft">{score.source}</span>
            </li>
          ))}
        </ul>
      )}
      <form
        onSubmit={submit}
        className="grid gap-2 sm:grid-cols-[1fr_1fr_2fr_auto] sm:items-end"
        noValidate
      >
        <Field name="score-name" invalid={invalid.name ? true : undefined}>
          <FieldLabel className="text-label">Score name</FieldLabel>
          <Input
            ref={nameInput}
            value={name}
            onValueChange={(next) => {
              setName(next);
              if (invalid.name) setInvalid((i) => ({ ...i, name: nameProblem(next) ?? undefined }));
            }}
            className="text-data"
          />
          {invalid.name ? <FieldError match>{invalid.name}</FieldError> : null}
        </Field>
        <Field name="score-value" invalid={invalid.value ? true : undefined}>
          <FieldLabel className="text-label">Score value</FieldLabel>
          <Input
            ref={valueInput}
            value={value}
            onValueChange={(next) => {
              setValue(next);
              if (invalid.value) setInvalid((i) => ({ ...i, value: valueProblem(next) }));
            }}
            className="text-data"
          />
          <FieldDescription className="text-label text-ink-soft">
            A number, true or false, or a word.
          </FieldDescription>
          {invalid.value ? <FieldError match>{invalid.value}</FieldError> : null}
        </Field>
        <Field name="score-comment">
          <FieldLabel className="text-label">Score comment</FieldLabel>
          <Textarea
            value={comment}
            onValueChange={(next) => setComment(next)}
            rows={1}
            className="text-data"
          />
          {bytes(comment) > COMMENT_BYTES - 500 ? (
            <FieldDescription className="text-label text-ink-soft">
              {`${Math.max(0, COMMENT_BYTES - bytes(comment))} bytes left.`}
            </FieldDescription>
          ) : null}
        </Field>
        <Button type="submit" disabled={write.isPending}>
          {write.isPending ? "Adding score" : "Add score"}
        </Button>
      </form>
      {write.isError ? <Problem failed="Couldn't save the score." error={write.error} /> : null}
      {write.isSuccess ? (
        <p className="flex items-center gap-1.5 text-label text-ink-soft">
          <CheckIcon className="shrink-0" />
          Score saved.
        </p>
      ) : null}
    </div>
  );
}
