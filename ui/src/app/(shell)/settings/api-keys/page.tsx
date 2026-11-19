import type { Metadata } from "next";
import { Suspense } from "react";
import { SectionFrame } from "@/components/section";
import { ApiKeysSettings } from "@/features/settings/api-keys";

export const metadata: Metadata = { title: "API keys" };

export default function ApiKeysPage() {
  return (
    <Suspense fallback={<SectionFrame title="API keys">{null}</SectionFrame>}>
      <ApiKeysSettings />
    </Suspense>
  );
}
