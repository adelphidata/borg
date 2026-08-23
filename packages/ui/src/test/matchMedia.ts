/**
 * jsdom ships no `window.matchMedia` at all, so anything that asks the OS a
 * question — the theme's `prefers-color-scheme`, the mobile breakpoint — throws
 * rather than degrading. This installs a real, listener-driven stand-in so the
 * tests can drive a breakpoint change the way a resized window would, instead
 * of asserting only the value the component happened to start with.
 */

export interface MediaController {
  /** Flip what the query matches and notify every listener, as the OS would. */
  set(matches: boolean): void;
  /** How many `change` listeners are attached right now. */
  listenerCount(): number;
  /** How many times `addEventListener("change", …)` has ever been called. */
  subscribeCalls(): number;
}

/**
 * Install a `matchMedia` whose answer is decided by `matches(query)`.
 *
 * Returns a controller per query string; `set` dispatches a real `change` event
 * so `useSyncExternalStore` and `addEventListener` paths both run.
 */
export function installMatchMedia(initial: (query: string) => boolean): {
  controller(query: string): MediaController;
} {
  const lists = new Map<
    string,
    { matches: boolean; listeners: Set<(e: MediaQueryListEvent) => void>; subscribeCalls: number }
  >();

  const entry = (query: string) => {
    let found = lists.get(query);
    if (!found) {
      found = { matches: initial(query), listeners: new Set(), subscribeCalls: 0 };
      lists.set(query, found);
    }
    return found;
  };

  window.matchMedia = ((query: string) => {
    const state = entry(query);
    return {
      get matches() {
        return state.matches;
      },
      media: query,
      onchange: null,
      // Type-aware, like the real EventTarget: a listener registered for any
      // event other than "change" never hears a media change, and removing
      // with the wrong type removes nothing.
      addEventListener: (type: string, listener: (e: MediaQueryListEvent) => void) => {
        if (type !== "change") return;
        state.subscribeCalls += 1;
        state.listeners.add(listener);
      },
      removeEventListener: (type: string, listener: (e: MediaQueryListEvent) => void) => {
        if (type !== "change") return;
        state.listeners.delete(listener);
      },
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    } as unknown as MediaQueryList;
  }) as typeof window.matchMedia;

  return {
    controller(query: string): MediaController {
      const state = entry(query);
      return {
        set(matches: boolean) {
          state.matches = matches;
          for (const listener of state.listeners) {
            listener({ matches } as MediaQueryListEvent);
          }
        },
        listenerCount: () => state.listeners.size,
        subscribeCalls: () => state.subscribeCalls,
      };
    },
  };
}
