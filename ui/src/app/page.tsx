"use client";

import type { Route } from "next";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect } from "react";

// The entry route only forwards to Home, keeping any project and range in the query.
export default function EntryPage() {
  const router = useRouter();
  useEffect(() => {
    router.replace(`/home${window.location.search}` as Route);
  }, [router]);
  return (
    <main className="grid min-h-dvh place-items-center px-4">
      <p className="text-data text-ink-soft">
        Opening Crucible. If nothing happens, go to <Link href="/home">Home</Link>.
      </p>
    </main>
  );
}
