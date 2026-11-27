"use client";

import { PageError } from "@/components/page-error";

export default function ShellError({ retry }: { error: unknown; retry: () => void }) {
  return <PageError retry={retry} />;
}
