const ROWS = Array.from({ length: 3 }, (_, row) => `skeleton-row-${row}`);

export function RowsSkeleton() {
  return (
    <div>
      <div className="h-8" />
      {ROWS.map((id) => (
        <div key={id} className="flex h-10 items-center gap-4 border-t border-ash-border">
          <div className="skeleton h-3 w-1/5" />
          <div className="skeleton h-3 w-1/4" />
          <div className="skeleton h-3 w-1/6" />
        </div>
      ))}
    </div>
  );
}
