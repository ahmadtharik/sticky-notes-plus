export function useDebouncedSave(
  getValue: () => string,
  save: (value: string) => Promise<void>,
  ms = 1000,
): { schedule: () => void; flush: () => Promise<void> } {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let inFlight: Promise<void> | undefined;

  const run = () => {
    const operation = save(getValue());
    inFlight = operation.catch(() => undefined).finally(() => {
      if (inFlight === operation) inFlight = undefined;
    });
    return operation;
  };

  const schedule = () => {
    if (timer !== undefined) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = undefined;
      void run().catch(() => undefined);
    }, ms);
  };

  const flush = async () => {
    if (timer !== undefined) {
      clearTimeout(timer);
      timer = undefined;
    }
    if (inFlight) await inFlight;
    await run();
  };

  return { schedule, flush };
}
