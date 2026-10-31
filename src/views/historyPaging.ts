// Pure paging arithmetic for the History view. Tested in historyPaging.test.ts.

export const PAGE_SIZE = 20;

/** The offset to show after the total changed (rows deleted, history cleared, new rows). */
export function clampOffset(offset: number, total: number, pageSize = PAGE_SIZE): number {
  if (total <= 0) {
    return 0;
  }
  const lastPage = Math.floor((total - 1) / pageSize) * pageSize;
  return Math.max(0, Math.min(offset, lastPage));
}

export function pageRange(
  offset: number,
  count: number,
  total: number,
): { from: number; to: number } {
  if (total === 0 || count === 0) {
    return { from: 0, to: 0 };
  }
  return { from: offset + 1, to: Math.min(total, offset + count) };
}

export function hasNewer(offset: number): boolean {
  return offset > 0;
}

export function hasOlder(offset: number, total: number, pageSize = PAGE_SIZE): boolean {
  return offset + pageSize < total;
}
