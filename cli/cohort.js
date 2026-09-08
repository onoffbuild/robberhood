'use strict';
/* cohort: the "FOMO hold" family of filters, computed from logs and nothing else.
   who bought early, how much of it they still hold, how far the price sits from its peak, how much ETH the deployer
   has taken out, whether the early money is one whale or many. the post-launch questions the launch score cannot
   ask: it looks at the door on the way in, these look at who is still in the room an hour later.

   pure: build(events, opts) takes parsed logs and returns numbers. chain.cohortRead fetches the logs. */

/* the trades on the curve (parseFactoryLog buy/sell rows), the token's transfers ({ bn, from, to, tokens }), and
   the launch. opts: { launchBn, cohortBlocks, deployer, exclude: [addresses that are venues, not holders], nowBn, churnBlocks } */
function build(ev, opts) {
  opts = opts || {};
  const launchBn = opts.launchBn || 0, cohortEnd = launchBn + (opts.cohortBlocks || 600);
  const exclude = new Set((opts.exclude || []).map(a => String(a).toLowerCase()).concat([String(opts.curve || '0xcurve').toLowerCase()]));
  const deployer = String(opts.deployer || '').toLowerCase();
  const trades = (ev.trades || []).filter(t => t && !t.removed).slice().sort((a, b) => a.bn - b.bn || a.idx - b.idx);
  let transfers = (ev.transfers || []).slice().sort((a, b) => a.bn - b.bn || (a.idx || 0) - (b.idx || 0));
  /* a curve buy is a transfer from the curve, a sell one back. when the token's transfer logs are not there (a node
     that will not answer the range, or the mock chain) the trades stand in for them, so the balances still add up. */
  const CURVE = String(opts.curve || '0xcurve').toLowerCase();
  if (!transfers.length && trades.length) transfers = trades.map(t => t.kind === 'buy' ? { bn: t.bn, idx: t.idx, from: CURVE, to: t.recipient || t.who, tokens: t.tokens || 0 } : { bn: t.bn, idx: t.idx, from: t.who, to: CURVE, tokens: t.tokens || 0 });

  /* balances from transfers: peak and current per wallet; the cohort is whoever received tokens from a curve buy
     inside the window. spent is the ETH they put in, so one whale does not carry the vote. */
  const bal = {}, peak = {}, spent = {}, firstIn = {}, out = {};
  const bump = (w, d) => { if (!w || exclude.has(w)) return; bal[w] = (bal[w] || 0) + d; if (bal[w] > (peak[w] || 0)) peak[w] = bal[w]; };
  transfers.forEach(x => {
    const f = String(x.from || '').toLowerCase(), t = String(x.to || '').toLowerCase();
    bump(f, -(x.tokens || 0)); bump(t, x.tokens || 0);
    if (t && !exclude.has(t) && firstIn[t] == null) firstIn[t] = x.bn;
    if (f && !exclude.has(f) && (x.tokens || 0) > 0) out[f] = (out[f] || 0) + (x.tokens || 0);
  });
  const cohort = new Set();
  let quoteIn = 0, devEthOut = 0, peakPrice = 0, lastPrice = null;
  const funded = new Set([deployer]);
  trades.forEach(t => {
    const who = String(t.recipient || t.who || '').toLowerCase();
    if (t.kind === 'buy') {
      quoteIn += t.quote || 0;
      if (t.bn <= cohortEnd && who && !exclude.has(who)) { cohort.add(who); spent[who] = (spent[who] || 0) + (t.quote || 0); }
    } else if (t.kind === 'sell') {
      const seller = String(t.who || '').toLowerCase();
      if (funded.has(seller)) devEthOut += t.quote || 0;
    }
    if (t.tokens > 0 && t.quote > 0) { const p = t.quote / t.tokens; lastPrice = p; if (p > peakPrice) peakPrice = p; }
  });
  /* wallets the deployer handed tokens to count as the deployer: the second-wallet exit */
  transfers.forEach(x => { const f = String(x.from || '').toLowerCase(), t = String(x.to || '').toLowerCase(); if (f === deployer && t && !exclude.has(t)) funded.add(t); });
  trades.forEach(t => { if (t.kind === 'sell') { const s = String(t.who || '').toLowerCase(); if (s !== deployer && funded.has(s)) devEthOut += t.quote || 0; } });

  /* hold: a cohort wallet holds if it still has ≥ 80 % of its peak balance. weighted by ETH spent, and by count. */
  const HOLD = opts.holdFloor != null ? opts.holdFloor : 0.8;
  let held = 0, total = 0, heldN = 0, reaccum = 0;
  const remaining = [];
  cohort.forEach(w => {
    const s = spent[w] || 0, pk = peak[w] || 0, b = bal[w] || 0;
    total += s;
    const holds = pk > 0 && b >= pk * HOLD;
    if (holds) { held += s; heldN++; }
    if (b > 0) remaining.push(b);
    if ((out[w] || 0) > 0 && b > 0 && pk > 0 && b >= pk * HOLD) reaccum++;
  });
  remaining.sort((a, b) => b - a);
  const remainingSum = remaining.reduce((a, b) => a + b, 0);
  const top5 = remaining.slice(0, 5).reduce((a, b) => a + b, 0);

  /* churn: wallets whose first tokens arrived inside the last churnBlocks, against wallets that went to zero in it */
  const nowBn = opts.nowBn || (transfers.length ? transfers[transfers.length - 1].bn : cohortEnd), since = nowBn - (opts.churnBlocks || 600);
  let newHolders = 0, gone = 0;
  Object.keys(bal).forEach(w => { if (firstIn[w] != null && firstIn[w] > since && (bal[w] || 0) > 0) newHolders++; });
  transfers.forEach(x => { const f = String(x.from || '').toLowerCase(); if (x.bn > since && f && !exclude.has(f) && (bal[f] || 0) <= 0 && (peak[f] || 0) > 0) gone++; });
  const holdersNow = Object.keys(bal).filter(w => (bal[w] || 0) > 0).length;

  return {
    cohort: cohort.size, cohortEth: total,
    holdPct: total > 0 ? held / total * 100 : null,            /* the FOMO hold %, ETH-weighted */
    holdCountPct: cohort.size ? heldN / cohort.size * 100 : null,
    reaccum,
    retracePct: peakPrice > 0 && lastPrice != null ? lastPrice / peakPrice * 100 : null,   /* price now as % of the peak trade */
    peakPrice, lastPrice,
    devEthOut, quoteIn, devOutPct: quoteIn > 0 ? devEthOut / quoteIn * 100 : null,
    cohortTop5Pct: remainingSum > 0 ? top5 / remainingSum * 100 : null,
    holdersNow, newHolders, gone, churn: newHolders - gone,
    funded: funded.size - 1
  };
}

/* the verdict: one line per rule, the way the launch score names its refusals */
const DEFAULTS = { minHold: 20, minRetrace: 50, maxDevOut: 30, maxCohortTop5: 60, minCohort: 8, holdCare: 15 };
function verdict(c, rules) {
  const r = Object.assign({}, DEFAULTS, rules || {});
  const why = [], warn = [];
  if (c.cohort < r.minCohort) why.push('cohort of ' + c.cohort + ' < ' + r.minCohort);
  if (c.holdPct == null) why.push('hold % unknown');
  else if (c.holdPct < r.holdCare) why.push('hold ' + c.holdPct.toFixed(0) + '% < ' + r.holdCare + '%');
  else if (c.holdPct < r.minHold) warn.push('hold ' + c.holdPct.toFixed(0) + '% under ' + r.minHold + '%');
  if (c.retracePct != null && c.retracePct < r.minRetrace) why.push('price at ' + c.retracePct.toFixed(0) + '% of peak < ' + r.minRetrace + '%');
  if (c.devOutPct != null && c.devOutPct > r.maxDevOut) why.push('deployer took ' + c.devOutPct.toFixed(0) + '% of the ETH in > ' + r.maxDevOut + '%');
  if (c.cohortTop5Pct != null && c.cohortTop5Pct > r.maxCohortTop5) warn.push('top-5 of the cohort hold ' + c.cohortTop5Pct.toFixed(0) + '% of what remains');
  if (c.churn < 0 && c.gone > 3) warn.push(c.gone + ' holders left, ' + c.newHolders + ' came');
  const stamp = why.length ? 'AVOID' : warn.length ? 'CAREFUL' : 'SURVIVOR';
  return { stamp, why, warn, rules: r };
}

module.exports = { build, verdict, DEFAULTS };
