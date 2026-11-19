const ROWS = Array.from({ length: 12 }, (_, row) => `skeleton-row-${row}`);

// The loading shape of a table section: a header row, then twelve rows.
export function ListSkeleton() {
  return (
    <div>
      <div className="h-8" />
      {ROWS.map((id) => (
        <div key={id} className="flex h-8 items-center gap-4 border-t border-ash-border">
          <div className="skeleton h-3 w-1/6" />
          <div className="skeleton h-3 w-2/5" />
          <div className="skeleton h-3 w-1/12" />
          <div className="skeleton h-3 w-1/12" />
        </div>
      ))}
    </div>
  );
}
