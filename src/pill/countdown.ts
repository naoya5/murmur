// Pure timing helpers for the pill. Tested in countdown.test.ts.

/** A deadline computed once from `remaining_ms`, then read locally. */
export function deadlineFrom(remainingMs: number, now: number): number {
  return now + Math.max(0, remainingMs);
}

export function remainingAt(deadline: number, now: number): number {
  return Math.max(0, deadline - now);
}

/** Fraction of the countdown still left, 0..1. */
export function fractionLeft(deadline: number, totalMs: number, now: number): number {
  if (totalMs <= 0) {
    return 0;
  }
  return Math.min(1, remainingAt(deadline, now) / totalMs);
}

/** Whole seconds left, rounded up so "3" shows until the first second has fully passed. */
export function secondsLeft(deadline: number, now: number): number {
  return Math.ceil(remainingAt(deadline, now) / 1000);
}

/** The local start time implied by an `elapsed_ms` that arrived at `now`. */
export function startFrom(elapsedMs: number, now: number): number {
  return now - Math.max(0, elapsedMs);
}

export function elapsedAt(start: number, now: number): number {
  return Math.max(0, now - start);
}

/** Fast attack, slow decay: the waveform follows a loud syllable at once and lets go softly. */
export function smoothLevel(previous: number, level: number, decay = 0.78): number {
  const clamped = Math.min(1, Math.max(0, level));
  return clamped >= previous ? clamped : previous * decay + clamped * (1 - decay);
}
