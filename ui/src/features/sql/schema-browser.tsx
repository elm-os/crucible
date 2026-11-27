"use client";

import type { UseQueryResult } from "@tanstack/react-query";
import {
  Accordion,
  AccordionHeader,
  AccordionItem,
  AccordionPanel,
  AccordionTrigger,
} from "@ui-sdk/react/accordion";
import { Button } from "@ui-sdk/react/button";
import { Fragment } from "react";
import type { SqlRelation } from "@/api/generated/SqlRelation";
import type { SqlSchema } from "@/api/generated/SqlSchema";
import { EmptyState } from "@/components/empty-state";
import { ChevronRightIcon } from "@/components/icons";
import { Section } from "@/components/section";
import { enumValues, shortType } from "./cells";

const SKELETON_ROWS = ["a", "b", "c", "d"] as const;

// The unit or meaning of the cost, duration and token columns, which their names leave out.
const COLUMN_NOTES: Record<string, string> = {
  total_cost_nusd: "Cost in nano-USD, billionths of a dollar: divide by 1e9 for US dollars.",
  cost_values: "The cost of each usage_keys entry, in nano-USD.",
  cost_provided_values: "The costs the client sent, in nano-USD, one per cost_provided_keys entry.",
  duration_us: "Duration in microseconds: divide by 1e6 for seconds.",
  input_tokens: "Tokens the model read, from the reported usage.",
  output_tokens: "Tokens the model wrote, from the reported usage.",
  total_tokens: "All tokens, from the reported usage.",
};

// A long type wraps after an opening parenthesis, between its parts, rather than inside a name.
function TypeName({ type }: { type: string }) {
  let at = 0;
  return type.split(/(?<=\()/).map((part) => {
    const start = at;
    at += part.length;
    return (
      <Fragment key={start}>
        {start > 0 ? <wbr /> : null}
        {part}
      </Fragment>
    );
  });
}

export function SchemaBrowser({
  query,
  onInsert,
  className,
}: {
  query: UseQueryResult<SqlSchema>;
  onInsert: (relation: SqlRelation) => void;
  className?: string;
}) {
  return (
    <Section
      title="Schema"
      query={query}
      className={className}
      skeleton={
        <div className="flex flex-col gap-3 py-1">
          {SKELETON_ROWS.map((row) => (
            <div key={row} className="skeleton h-4 w-3/4" />
          ))}
        </div>
      }
      isEmpty={(schema) => schema.relations.length === 0}
      empty={
        <EmptyState kind="first-use" title="No relations to query.">
          This server offers no readable relations to the SQL workspace yet.
        </EmptyState>
      }
    >
      {(schema) => (
        <Accordion
          multiple
          defaultValue={schema.relations.slice(0, 1).map((relation) => relation.name)}
          className="max-w-none"
        >
          {schema.relations.map((relation) => (
            <AccordionItem key={relation.name} value={relation.name}>
              <AccordionHeader>
                <AccordionTrigger className="group justify-start gap-1.5 text-start font-mono text-data">
                  <ChevronRightIcon className="shrink-0 transition-transform duration-150 group-data-open:rotate-90 motion-reduce:transition-none" />
                  <span className="min-w-0 break-all">{relation.name}</span>
                </AccordionTrigger>
              </AccordionHeader>
              <AccordionPanel>
                <div className="flex flex-col items-start gap-2 pb-3">
                  <p className="text-data text-ink-soft">{relation.description}</p>
                  <p className="text-label text-ink-soft">
                    Time column <span className="font-mono">{relation.time_column}</span>
                    {relation.window_required
                      ? ". A query must filter it with the time window."
                      : "."}
                  </p>
                  <table className="w-full text-start text-label">
                    <caption className="sr-only">Columns of {relation.name}</caption>
                    <thead>
                      <tr className="text-ink-soft">
                        <th scope="col" className="pe-3 text-start font-medium">
                          Column
                        </th>
                        <th scope="col" className="text-start font-medium">
                          Type
                        </th>
                      </tr>
                    </thead>
                    <tbody>
                      {relation.columns.map((column) => {
                        const values = enumValues(column.type);
                        const note = COLUMN_NOTES[column.name];
                        return (
                          <Fragment key={column.name}>
                            <tr className="border-t border-ash-border">
                              <td className="py-0.5 pe-3 align-top font-mono whitespace-nowrap">
                                {column.name}
                              </td>
                              <td className="py-0.5 align-top font-mono wrap-anywhere text-ink-soft">
                                <TypeName type={shortType(column.type)} />
                              </td>
                            </tr>
                            {values.length > 0 || note ? (
                              <tr>
                                <td colSpan={2} className="pb-1 text-ink-soft">
                                  {values.length > 0 ? (
                                    <span className="font-mono">{values.join(", ")}</span>
                                  ) : (
                                    note
                                  )}
                                </td>
                              </tr>
                            ) : null}
                          </Fragment>
                        );
                      })}
                    </tbody>
                  </table>
                  <Button onClick={() => onInsert(relation)}>
                    Insert example
                    <span className="sr-only"> for {relation.name}</span>
                  </Button>
                </div>
              </AccordionPanel>
            </AccordionItem>
          ))}
        </Accordion>
      )}
    </Section>
  );
}
