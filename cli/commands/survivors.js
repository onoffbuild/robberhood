'use strict';
/* survivors: stop chasing fresh launches, hunt the coins that already ran and held their ground.
   every launch in the window older than --min-age, its FOMO cohort (the wallets that bought in the first ten
   minutes) read from logs: how much of their ETH is still in, where the price sits against its peak trade, how much
   of the ETH in the deployer and the wallets it funded have taken out, whether the early money is one whale.
   reads only. the rules are in .env (SURVIVOR_*), --min-hold overrides the one that matters most. */
const K = require('../cohort');
const { makeLinks } = require('../pons');

module.exports = async function survivors(ctx) {
  const { ui, log, flags, env, chain } = ctx, C = ui.C, Kl = makeLinks(env);
  const json = !!flags.json;
  const rules = {
    minHold: flags['min-hold'] != null ? parseFloat(flags['min-hold']) : env.num('SURVIVOR_MIN_HOLD_PCT'),
    minRetrace: env.num('SURVIVOR_MIN_RETRACE_PCT'), maxDevOut: env.num('SURVIVOR_MAX_DEV_OUT_PCT'),
    maxCohortTop5: env.num('SURVIVOR_MAX_COHORT_TOP5_PCT'), minCohort: env.num('SURVIVOR_MIN_COHORT'), holdCare: env.num('SURVIVOR_HOLD_CARE_PCT')
  };
  const minAgeMin = flags['min-age'] != null ? parseFloat(flags['min-age']) : env.num('SURVIVOR_MIN_AGE_MIN');
  const top = Math.max(1, parseInt(flags.top != null ? flags.top : 12, 10) || 12);
  const head = await chain.blockNumber();
  const bt = await chain.measureBlockTime(head).catch(() => ({ blockTime: 0.25 }));
  const blockTime = bt.blockTime || 0.25;
  const window = flags.window != null ? parseInt(flags.window, 10) : Math.round(env.num('SURVIVOR_WINDOW_MIN') * 60 / blockTime);
  const cohortBlocks = Math.round(env.num('SURVIVOR_COHORT_MIN') * 60 / blockTime);
  if (!json) log.dim('indexing ' + window + ' blocks (' + ui.fmtAge(window * blockTime * 1000) + ') of pons v2 launches at ' + ui.short(env.FACTORY) + '…');
  const idx = await chain.buildIndex(window, head);
  const minAgeBlocks = Math.round(minAgeMin * 60 / blockTime);
  let cands = idx.launches.filter(e => head - e.bn >= minAgeBlocks && e.pair === chain.ZERO);
  if (flags.graduated) cands = cands.filter(e => e.grad);
  if (!json) log.dim(idx.launches.length + ' launches in the window, ' + cands.length + ' older than ' + minAgeMin + ' min' + (flags.graduated ? ' and graduated' : '') + ', reading their cohorts (three log reads each)…');
  const rows = [];
  for (const e of cands) {
    let c;
    try { c = await chain.cohortRead(e, head, { gradBn: e.gradBn, cohortBlocks, churnBlocks: cohortBlocks }); }
    catch (err) { log.warn('cohort of ' + e.token + ': ' + err.message); continue; }
    const v = K.verdict(c, rules);
    const meta = await chain.tokenRead(e.token).catch(() => ({}));
    rows.push({ token: e.token, curve: e.curve, deployer: e.deployer, symbol: meta.symbol || null, name: meta.name || null, bn: e.bn, ageMs: (head - e.bn) * blockTime * 1000, grad: e.grad, swept: e.swept, cohort: c, stamp: v.stamp, why: v.why, warn: v.warn });
  }
  const rank = { SURVIVOR: 0, CAREFUL: 1, AVOID: 2 };
  rows.sort((a, b) => rank[a.stamp] - rank[b.stamp] || (b.cohort.holdPct || 0) - (a.cohort.holdPct || 0));
  if (json) { log.raw(JSON.stringify({ head, blockTime, window, rules, minAgeMin, rows }, null, 2)); return 0; }
  const pct = (x, d) => x == null ? C.dim('n/a') : x.toFixed(d == null ? 0 : d) + '%';
  const hold = x => x == null ? C.dim('n/a') : (x >= rules.minHold ? C.green : x >= rules.holdCare ? C.amber : C.red)(x.toFixed(0) + '%');
  const shown = rows.slice(0, top);
  log.raw('');
  log.raw(ui.table(['stamp', 'token', 'age', 'phase', 'cohort', 'hold %', 'vs peak', 'dev out', 'top-5', 'churn'], shown.map(r => [
    ui.badge(r.stamp), C.bold(C.white('$' + (r.symbol || '…'))) + C.dim('  ' + ui.link(Kl.fomo(r.token), ui.short(r.token))), ui.fmtAge(r.ageMs), r.grad ? 'pool' : r.swept ? 'gap' : 'curve',
    r.cohort.cohort + C.dim(' · ' + r.cohort.cohortEth.toFixed(2) + ' ETH'), hold(r.cohort.holdPct) + C.dim(' / ' + pct(r.cohort.holdCountPct) + ' by count'),
    r.cohort.retracePct == null ? C.dim('n/a') : (r.cohort.retracePct >= rules.minRetrace ? C.green : C.amber)(pct(r.cohort.retracePct)), pct(r.cohort.devOutPct), pct(r.cohort.cohortTop5Pct),
    (r.cohort.churn > 0 ? C.green('+' + r.cohort.churn) : r.cohort.churn < 0 ? C.red(String(r.cohort.churn)) : C.dim('0')) + C.dim(' (' + r.cohort.holdersNow + ' hold)')
  ])));
  shown.forEach(r => { if (r.why.length || r.warn.length) log.raw('  ' + C.dim('$' + (r.symbol || '…') + ': ') + [].concat(r.why.map(x => C.red(x)), r.warn.map(x => C.amber(x))).join(C.dim(' · '))); });
  log.raw('');
  log.raw(ui.note(C.dim('hold % is the ETH-weighted share of the first-' + env.num('SURVIVOR_COHORT_MIN') + '-minute buyers still holding ≥ 80 % of their peak balance. SURVIVOR needs hold ≥ ' + rules.minHold + '%, price ≥ ' + rules.minRetrace + '% of its peak trade, deployer out ≤ ' + rules.maxDevOut + '% of the ETH in, a cohort of ' + rules.minCohort + '+. pool-phase trades are not curve trades, so "vs peak" on a pool token is the curve\'s peak against its last curve trade; the hold % is live in both phases.')));
  if (rows.length > top) log.raw(ui.note(C.dim((rows.length - top) + ' more · --top ' + rows.length)));
  return 0;
};
