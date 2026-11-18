"use client";

import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import { Suspense } from "react";
import { useProject } from "@/lib/project";
import { parseRange, RANGES, type Range, useTimeRange } from "@/lib/time-range";

function RangeSegments({
  value,
  onChange,
}: {
  value: Range | null;
  onChange?: (next: Range) => void;
}) {
  return (
    <SegmentedControl
      aria-label="Time range"
      value={value}
      onValueChange={(next) => {
        const range = parseRange(next);
        if (range) onChange?.(range);
      }}
    >
      <SegmentedControlIndicator />
      {RANGES.map((range) => (
        <SegmentedControlItem key={range} value={range} className="text-data">
          {range}
        </SegmentedControlItem>
      ))}
    </SegmentedControl>
  );
}

function ScopedRangeSegments() {
  const [range, setRange] = useTimeRange(useProject());
  return <RangeSegments value={range} onChange={setRange} />;
}

// The one time range for every telemetry page: kept in `range=` and remembered per project.
export function TimeRangeControl() {
  return (
    <Suspense fallback={<RangeSegments value={null} />}>
      <ScopedRangeSegments />
    </Suspense>
  );
}
