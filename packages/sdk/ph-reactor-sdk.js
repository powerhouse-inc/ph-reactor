/*
 * @powerhousedao/ph-reactor-sdk — the client half of the capability bridge.
 *
 * A plugin editor runs in a sandboxed iframe on an origin of its own, with a
 * Content-Security-Policy that sets `connect-src 'none'`. It therefore cannot
 * fetch, cannot open a socket, and cannot reach the reactor except by asking
 * the host console to act on its behalf. This module is that asking.
 *
 * The vocabulary is deliberately reactor-browser's — `useQuery`, `useSubmit` —
 * so anyone who has written a Powerhouse editor already knows the shape, even
 * though the transport underneath is postMessage rather than GraphQL and the
 * reducers underneath are declarative JSON rather than TypeScript.
 *
 * Framework-free on purpose: it ships inlined into a single HTML document, so
 * pulling in React to provide two functions would be a poor trade. `useQuery`
 * here is a subscription with a callback, not a React hook; the name is kept
 * because what it *means* is the same.
 *
 * Nothing in here is a security boundary. The host checks every call against
 * the package's declared capabilities, and the daemon checks it again before
 * touching the store, so a plugin that skipped this file entirely and wrote its
 * own postMessage would gain exactly nothing.
 */

export function createClient(options = {}) {
  const timeoutMs = options.timeoutMs ?? 15000;
  const target = options.parent ?? window.parent;
  let seq = 0;
  const pending = new Map();

  window.addEventListener("message", (ev) => {
    // Only the host frame can answer. Anything else is noise or an attempt.
    if (ev.source !== target) return;
    const m = ev.data;
    if (!m || typeof m.id !== "number") return;
    const p = pending.get(m.id);
    if (!p) return;
    pending.delete(m.id);
    clearTimeout(p.timer);
    if (m.error) p.reject(new Error(m.error));
    else p.resolve(m.result);
  });

  function call(op, payload) {
    return new Promise((resolve, reject) => {
      const id = ++seq;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error("the reactor did not answer in time"));
      }, timeoutMs);
      pending.set(id, { resolve, reject, timer });
      // "*" is required rather than sloppy: the host frame may be on an origin
      // this document is not permitted to name. postMessage still delivers to
      // that window and no other.
      target.postMessage({ id, op, ...payload }, "*");
    });
  }

  /** Who am I, and how should I look? Never identity or configuration. */
  const host = () => call("host", {});

  /** One read. `filter` is `{field, value}` or null for everything. */
  const query = (model, filter = null) => call("query", { model, filter });

  /**
   * A read that keeps itself current.
   *
   * There is no change feed across the bridge, so this polls. That is the
   * honest implementation rather than a pretend-live one: documents arrive
   * over the mesh at their own pace, and a plugin claiming instant updates
   * would be lying about a network it cannot see.
   *
   * Returns an unsubscribe function. Call it — a plugin that leaks pollers
   * keeps waking the daemon for a view nobody is looking at.
   */
  function useQuery(model, filter, onData, opts = {}) {
    const everyMs = opts.everyMs ?? 4000;
    let stopped = false;
    let timer = null;
    let last = null;

    const tick = async () => {
      if (stopped) return;
      try {
        const rows = await query(model, filter);
        const json = JSON.stringify(rows);
        // Only call back when something actually changed, so a plugin can
        // re-render freely without fighting its own poll loop.
        if (json !== last) {
          last = json;
          onData(rows, null);
        }
      } catch (e) {
        onData(null, e);
      } finally {
        if (!stopped) timer = setTimeout(tick, everyMs);
      }
    };
    tick();

    return () => {
      stopped = true;
      if (timer) clearTimeout(timer);
    };
  }

  /**
   * One write.
   *
   * `kind` is a reducer on the model. `init` creates the document; anything
   * else acts on the existing one named `name`. Success here means the action
   * was signed and applied locally, and is on its way to peers — it does not
   * mean every peer has it yet.
   *
   * A rejection is normal and worth showing the user verbatim: the model's own
   * preconditions, auth rule and quorum requirement all speak through it.
   */
  const submit = (model, kind, name, payload = {}) =>
    call("action", { model, kind, name, payload });

  /** The reactor-browser-shaped alias. */
  const useSubmit = () => submit;

  return { host, query, useQuery, submit, useSubmit };
}

export default createClient;
