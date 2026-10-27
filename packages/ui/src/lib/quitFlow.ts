/**
 * The quit flow: ask about unsaved work, then quit. Every way of quitting
 * (Cmd+Q from the macOS menu, Ctrl+Q on Windows and Linux, closing the
 * window) ends up here. Only one flow runs at a time: a second request while
 * the question is open (or while a Save as runs) joins the running one
 * instead of stacking another dialog or quitting halfway. Resolves `true`
 * only when the quit went through.
 */
export function createQuitFlow(steps: {
  /** Tells the backend a person is being asked, before any dialog, so the request waits for them. */
  acknowledge: () => Promise<void>;
  /** Asks about unsaved work; `false` when the user cancelled or a save failed. */
  confirmUnsaved: () => Promise<boolean>;
  /** Quits the app; rejects when quitting failed. */
  quit: () => Promise<void>;
  /** Tells the backend the user stays (so the next request asks again). */
  stay: () => Promise<void>;
  /** Shows why the check or the quit failed. */
  failed: (error: unknown) => void;
}): () => Promise<boolean> {
  let running: Promise<boolean> | null = null;

  /** Ends a flow that does not quit: free the flow first, so a request right after is answered. */
  async function stayOpen(): Promise<false> {
    running = null;
    await steps.stay().catch((error: unknown) => steps.failed(error));
    return false;
  }

  return () => {
    if (running) {
      // A repeated request joins the open question; it is acknowledged too, so
      // the backend keeps waiting for the user instead of timing out.
      void steps.acknowledge().catch(() => undefined);
      return running;
    }
    const work = (async (): Promise<boolean> => {
      await steps.acknowledge().catch(() => undefined);
      let ok: boolean;
      try {
        ok = await steps.confirmUnsaved();
      } catch (error: unknown) {
        // A failing check never leaves the backend waiting.
        steps.failed(error);
        return stayOpen();
      }
      if (!ok) return stayOpen();
      try {
        await steps.quit();
        return true;
      } catch (error: unknown) {
        // The app stays open; the user can try again.
        steps.failed(error);
        return stayOpen();
      }
    })();
    running = work;
    void work.finally(() => {
      if (running === work) running = null;
    });
    return work;
  };
}
