// Observe only contexts the test itself requests. Never alter application handlers or assertions.
import { test } from '@playwright/test';
export function register() {
  test.use({ context: async ({ context }, use) => {
    let remaining = 8;
    const listeners = new Map();
    const attach = page => {
      if (listeners.has(page)) return;
      const report = message => {
        if (remaining-- > 0) console.error('__OCTOS_PAGE_ERROR__' + JSON.stringify(String(message).slice(0, 1600)));
      };
      const onError = error => report(error.stack || error);
      // A generated app does most of its work over fetch/XHR, so a failing API
      // call is what empties a list; reporting navigations alone said nothing
      // about it. One line per distinct method+status+path: a view that reloads
      // would otherwise spend the whole budget on the same failure.
      const reported = new Set();
      const onResponse = response => {
        if (response.status() < 400) return;
        if (response.frame() !== page.mainFrame()) return;
        const request = response.request();
        // Keep routing evidence without credentials, query values or fragments.
        const url = new URL(response.url());
        const where = `${url.origin}${url.pathname}`;
        const line = request.isNavigationRequest()
          ? `Navigation HTTP ${response.status()} ${where}`
          : `Request HTTP ${response.status()} ${request.method()} ${where}`;
        if (reported.has(line)) return;
        reported.add(line);
        report(line);
      };
      // An app that catches its own failure renders a placeholder and throws
      // nothing, so `pageerror` never fires and only the symptom survives. Take
      // its error log, on a small budget of its own so ordinary chatter cannot
      // crowd out the page errors and failed requests above.
      let logged = 3;
      const onConsole = message => {
        const text = String(message.text());
        // The browser's own note for a failed request; `onResponse` already
        // reports those with their method, status and path.
        if (message.type() !== 'error' || text.startsWith('Failed to load resource')) return;
        if (reported.has(text) || logged-- <= 0) return;
        reported.add(text);
        report('Console error: ' + text);
      };
      listeners.set(page, { onError, onResponse, onConsole });
      page.on('pageerror', onError);
      page.on('response', onResponse);
      page.on('console', onConsole);
    };
    context.pages().forEach(attach);
    context.on('page', attach);
    try { await use(context); }
    finally {
      context.off('page', attach);
      for (const [page, listener] of listeners) {
        page.off('pageerror', listener.onError);
        page.off('response', listener.onResponse);
        page.off('console', listener.onConsole);
      }
    }
  } });
}
