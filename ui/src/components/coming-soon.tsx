import { EmptyState } from "./empty-state";
import { PageHeader } from "./page-header";
import { SectionFrame } from "./section";

export function ComingSoon({ title, description }: { title: string; description: string }) {
  return (
    <>
      <PageHeader title={title} />
      <SectionFrame title={`About ${title}`}>
        <EmptyState kind="planned" title={`${title} is not built yet.`}>
          {description}
        </EmptyState>
      </SectionFrame>
    </>
  );
}
