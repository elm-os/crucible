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
import type { SqlRelation } from "@/api/generated/SqlRelation";
import type { SqlSchema } from "@/api/generated/SqlSchema";
import { EmptyState } from "@/components/empty-state";
import { ChevronRightIcon } from "@/components/icons";
import { Section } from "@/components/section";

const SKELETON_ROWS = ["a", "b", "c", "d"] as const;

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
                      {relation.columns.map((column) => (
                        <tr key={column.name} className="border-t border-ash-border">
                          <td className="py-0.5 pe-3 font-mono break-all">{column.name}</td>
                          <td className="py-0.5 font-mono break-all text-ink-soft">
                            {column.type}
                          </td>
                        </tr>
                      ))}
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
