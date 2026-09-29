import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useDebouncedSave } from './useDebouncedSave';

describe('useDebouncedSave', () => {
  beforeEach(() => vi.useFakeTimers());
  it('coalesces rapid saves and flushes immediately', async () => {
    const save = vi.fn(async () => {});
    const { schedule, flush } = useDebouncedSave(() => 'v', save, 1000);
    schedule(); await vi.advanceTimersByTimeAsync(400);
    schedule(); await vi.advanceTimersByTimeAsync(400);
    expect(save).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(600);
    expect(save).toHaveBeenCalledTimes(1);
    schedule(); await flush();
    expect(save).toHaveBeenCalledTimes(2);
    expect(vi.getTimerCount()).toBe(0);
  });
});
