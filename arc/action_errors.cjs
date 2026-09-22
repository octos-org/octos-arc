// Supplement the JSON reporter, which omits ordinary API steps. Never change results.
const fs = require('fs');
const path = require('path');
const clip = (text, limit) => Array.from(String(text)).slice(0, limit).join('');
module.exports = class ActionErrors {
  constructor(options = {}) { this.output = options.output || 'action-errors.json'; this.rows = {}; }
  onTestEnd(test, result) {
    const errors = [];
    for (const chunk of result.stderr || []) {
      for (const line of String(chunk).split('\n')) {
        const prefix = '__OCTOS_PAGE_ERROR__';
        if (!line.startsWith(prefix)) continue;
        try {
          const message = JSON.parse(line.slice(prefix.length));
          if (typeof message === 'string' && errors.length < 8)
            errors.push({order:errors.length, duration:Number.MAX_SAFE_INTEGER,
              text:'Browser observation (diagnostic only):\n'+clip(message,1600)});
        } catch (_) { /* Optional diagnostics cannot change the verdict. */ }
      }
    }
    const seen = new Set();
    const recentActions = [];
    let precedingFailure = [];
    // Playwright puts the locator (or the URL) an action ran against in the
    // step's subtitle. Without it a trace reads "Hover -> Click -> Click" and
    // never shows that the click landed on a different item's control.
    // A navigation subtitle is a bare URL and carries whatever the test put in
    // its query or fragment; keep routing evidence without those values, the way
    // the page observer already does. Locator expressions always have a call in
    // them, so they keep any `?` they contain.
    const target = subtitle => {
      const text = String(subtitle);
      return text.includes('(') ? text : text.split(/[?#]/)[0];
    };
    const describe = (step, titleLimit, targetLimit) => {
      const suffix = step.subtitle ? ' ' + clip(target(step.subtitle), targetLimit) : '';
      return clip(step.title, titleLimit) + suffix;
    };
    const visit = steps => {
      for (const step of steps || []) {
        const message = String(step.error?.message || '').replace(/\x1b\[[0-9;]*[A-Za-z]/g, '');
        if (message) precedingFailure = recentActions.slice();
        if (step.category === 'pw:api') {
          // Locator probes can otherwise evict the actions that changed the page.
          if (!message && !/^Query\b/i.test(step.title)) {
            recentActions.push(describe(step, 40, 120));
            if (recentActions.length > 6) recentActions.shift();
          }
        }
        if (step.category === 'pw:api' && message && !seen.has(message)) {
          seen.add(message);
          const location = step.location ? ` at ${clip(path.basename(step.location.file), 160)}:${step.location.line}` : '';
          errors.push({ order: errors.length, duration: step.duration || 0,
            text: `${describe(step, 200, 200)} (${step.duration || 0} ms)${location}:\n${clip(message, 1800)}` });
        }
        visit(step.steps);
      }
    };
    visit(result.steps);
    // A spec that compares values it collected itself raises outside any
    // Playwright call, so no step carries the error and there is nothing to cut
    // the trace at. Everything the test did is then the trace.
    const preceding = precedingFailure.length ? precedingFailure : recentActions;
    if (result.status !== 'passed' && result.status !== 'skipped' && preceding.length) {
      errors.push({order:-1, duration:Number.MAX_SAFE_INTEGER,
        text:'Actions preceding the final failed step (diagnostic only):\n'+preceding.join(' -> ')});
    }
    // Keep the most time-consuming failures, then present them in execution order.
    this.rows[test.id] = errors.sort((a,b) => b.duration-a.duration).slice(0,8)
      .sort((a,b) => a.order-b.order).map(e => clip(e.text, 2000));
  }
  onEnd() {
    try { fs.writeFileSync(this.output, JSON.stringify(this.rows)); }
    catch (error) { console.error(`[action diagnostics] ${error.message}`); }
  }
};
