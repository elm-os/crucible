import Link from "next/link";

export default function NotFound() {
  return (
    <main className="grid min-h-dvh place-items-center px-4">
      <div className="flex max-w-md flex-col items-start gap-3">
        <p className="text-label font-medium text-ink-soft">Error 404</p>
        <h1 className="font-display text-heading">This page doesn&apos;t exist.</h1>
        <p className="text-body text-ink-soft">
          The address may be mistyped, or the page may have moved.
        </p>
        <Link href="/home" className="text-body">
          Go to Home
        </Link>
      </div>
    </main>
  );
}
