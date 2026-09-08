'use strict';
/* engine: the desk's side of the Rust hot path. connects to the engine's socket, hands every launch the engine sees
   to the desk's own scorer and rules, and tells the engine to enter the ones that pass, before the opening tax
   crosses the ceiling. the engine does the timing, the signing and the exits; this command decides and watches.
   --auto skips the scorer: the engine's own filters decide. reads by default: the engine is on paper until it is
   started with PRIVATE_KEY, and this command says which on connect. */
const net = require('net');
const { makeFeed } = require('../feed');
const E = require('../engine');
const { makeLinks } = require('../pons');

const ethHex = eth => '0x' + (BigInt(Math.round(parseFloat(eth) * 1e6)) * 10n ** 12n).toString(16);
const fromHex = h => Number(BigInt(h)) / 1e18;

module.exports = async function engine(ctx) {
  const { ui, log, flags, env, chain } = ctx, C = ui.C, K = makeLinks(env);
  const sock = flags.socket || env.ENGINE_SOCKET || '/tmp/loxley-engine.sock';
  const eth = flags.eth != null ? parseFloat(flags.eth) : env.num('PAPER_ETH');
  const rules = { minScore: flags['min-score'] != null ? parseFloat(flags['min-score']) : env.num('MIN_SCORE'), maxOpen: flags['max-open'] != null ? parseInt(flags['max-open'], 10) : env.num('MAX_OPEN'),
    maxDevShare: env.num('MAX_DEV_SHARE'), maxCreatorTax: env.num('MAX_CREATOR_TAX'), ethPairsOnly: true, maxTwins: env.num('MAX_TWINS'), maxExempt: env.num('MAX_EXEMPT'), maxBundlePct: env.num('MAX_BUNDLE_PCT'),
    requireSocials: env.bool('REQUIRE_SOCIALS'), refuseFarms: env.bool('REFUSE_FARMS'), open: 0 };
  const ceiling = env.num('TAX_CEILING_BPS'), slippage = env.num('SLIPPAGE_BPS');
  const forS = flags.for != null ? parseFloat(flags.for) : null;
  const auto = !!flags.auto;

  let feed = null;
  if (!auto) {
    log.dim('indexing the factory for the deployer record…');
    feed = makeFeed(ctx, { window: flags.window != null ? parseInt(flags.window, 10) : undefined });
    try { await feed.start(); } catch (e) { log.warn('no factory index (' + e.message + '): launches are scored without the deployer record'); feed = null; }
  }

  const open = {};   /* curve -> position line */
  const s = net.connect(sock);
  const send = o => s.write(JSON.stringify(o) + '\n');
  let buf = '', done = false, code = 0;
  const finish = c => { if (done) return; done = true; code = c; if (feed) feed.stop(); s.end(); };
  const tag = (curve, sym) => C.bold(C.white('$' + (sym || '…'))) + C.dim('  ' + ui.link(K.axiom(curve), ui.short(curve)));
  const symOf = {};

  s.on('error', e => { log.error('engine socket ' + sock + ': ' + e.message + '. start it with: loxley-engine run --socket ' + sock); finish(1); });
  s.on('close', () => { if (!done) { log.warn('the engine went away'); finish(1); } });
  s.on('connect', () => {
    log.ok('connected to the engine at ' + sock);
    /* the desk's exit rules, in the engine's units */
    send({ op: 'rules', take_profit_bps: Math.round(env.num('TAKE_PROFIT_PCT') * 100), stop_loss_bps: Math.round(env.num('STOP_LOSS_PCT') * 100), max_hold_ms: Math.round(env.num('MAX_HOLD_MIN') * 60000), trail: [[0, Math.round(env.num('TRAILING_PCT') * 100)], [20000, 1500], [90000, 1000]] });
    if (env.WIRE_MS) send({ op: 'wire', ms: env.num('WIRE_MS') });
    if (auto) send({ op: 'auto', eth: ethHex(eth), ceiling_bps: ceiling, slippage_bps: slippage, max_creator_tax_bps: Math.round(env.num('MAX_CREATOR_TAX') * 100), max_exemptions: env.num('MAX_EXEMPT'), max_open: rules.maxOpen });
    if (forS) setTimeout(() => { log.dim('--for ' + forS + ' s is up'); finish(0); }, forS * 1000);
  });
  s.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { const line = buf.slice(0, i); buf = buf.slice(i + 1); let o; try { o = JSON.parse(line); } catch (e) { continue; } on(o).catch(e => log.error('engine event: ' + e.message)); } });

  async function on(o) {
    switch (o.ev) {
      case 'hello':
        log.raw(ui.kv([['engine', 'v' + o.engine], ['wallet', o.paper ? C.amber('none: paper, every fire is reported and never sent') : C.green(o.address)], ['entry', auto ? 'the engine\'s filters (--auto): ' + eth + ' ETH a shot, tax ceiling ' + ceiling + ' bps' : 'the desk scores every launch, min score ' + rules.minScore + ', ' + eth + ' ETH a shot'], ['exits', 'take profit +' + env.num('TAKE_PROFIT_PCT') + '% · stop −' + env.num('STOP_LOSS_PCT') + '% · half off at +100% · trail ' + env.num('TRAILING_PCT') + '% tightening to 10% · siren on any watched wallet, pre-signed']]));
        break;
      case 'launch':
        symOf[o.hash] = o.symbol;
        log.raw(ui.stamp() + '  ' + C.amber('launch') + '  ' + C.bold(C.white('$' + o.symbol)) + C.dim(' ' + (o.name || '').slice(0, 24)) + C.dim('  by ' + ui.short(o.from) + '  dev buy ' + fromHex(o.quote_in).toFixed(3) + ' ETH  creator tax ' + (o.creator_tax_bps / 100).toFixed(1) + '%  exempt ' + o.exemptions));
        break;
      case 'launch_ready': {
        symOf[o.curve] = symOf[o.hash];
        if (auto) break;
        if (!o.native) { log.dim('          not an ETH pair, passed'); break; }
        const t0 = Date.now();
        const e = { kind: 'launch', token: o.token, curve: o.curve, deployer: o.deployer, pair: chain.ZERO, bn: o.bn || 0, idx: 0, thr: 0 };
        let L;
        try { L = feed ? await feed.enrich(e) : null; } catch (err) { log.warn('          unreadable (' + err.message + '), passed'); break; }
        if (!L) { log.warn('          no scorer without the factory index, passed'); break; }
        const why = E.refusals(L, Object.assign({}, rules, { open: Object.keys(open).length }));
        const left = o.crossing_in_ms - (Date.now() - t0);
        log.raw('          ' + ui.badge(L.score.verdict) + ' ' + C.bold(String(L.score.total)) + C.dim('  read in ' + (Date.now() - t0) + ' ms, ' + left + ' ms to the crossing  ') + L.score.parts.slice(0, 4).map(p => (p.v > 0 ? C.green('+' + p.v) : C.red(String(p.v))) + C.dim(' ' + p.k)).join(C.dim(' · ')));
        if (why.length) { log.raw('          ' + C.dim('passed: ') + C.amber(why.join(' · '))); break; }
        if (left < 50) { log.warn('          too late for the crossing (' + left + ' ms), passed'); break; }
        send({ op: 'enter', hash: o.hash, eth: ethHex(eth), ceiling_bps: ceiling, slippage_bps: slippage });
        log.raw('          ' + C.green('enter ') + eth + ' ETH' + C.dim(' sent to the engine'));
        break;
      }
      case 'passed': log.raw('          ' + C.dim('passed by the engine: ') + C.amber(o.why)); break;
      case 'planned': log.raw('          ' + C.dim('armed: ') + fromHex(o.spend).toFixed(4) + ' ETH' + C.dim(' fires in ' + o.fire_in_ms + ' ms at tax ' + o.tax_bps + ' bps')); break;
      case 'opened': open[o.curve] = { t: Date.now(), cost: fromHex(o.cost) }; log.raw(ui.stamp() + '  ' + ui.badge('FILL') + ' ' + tag(o.curve, symOf[o.curve]) + '  ' + fromHex(o.cost).toFixed(4) + ' ETH' + C.dim('  ' + (o.hash === 'paper' ? 'paper' : ui.short(o.hash) + ' in ' + o.ms + ' ms'))); break;
      case 'mark': { const p = open[o.curve]; if (!p) break; if (Date.now() - (p.said || 0) < 5000 && Math.abs(o.gain_bps - (p.last || 0)) < 300) break; p.said = Date.now(); p.last = o.gain_bps; log.raw(ui.stamp() + '  ' + C.dim('mark  ') + tag(o.curve, symOf[o.curve]) + '  ' + (o.gain_bps >= 0 ? C.green : C.red)((o.gain_bps / 100).toFixed(1) + '%') + C.dim('  peak ' + (o.peak_bps / 100).toFixed(1) + '%' + (o.halted ? '  HALTED' : ''))); break; }
      case 'siren': log.raw(ui.stamp() + '  ' + ui.badge('SIREN') + ' ' + tag(o.curve, symOf[o.curve]) + '  ' + C.red(C.bold(o.kind)) + C.dim('  ' + ui.short(o.from) + ' ' + o.detail)); ui.bell(); break;
      case 'decision': log.raw('          ' + (o.trapped ? C.amber('wants out, halted: ') : C.dim('exit: ')) + o.why + C.dim('  ' + (o.bps / 100) + '% of the position')); break;
      case 'fired': if (o.label.startsWith('sell')) { log.raw(ui.stamp() + '  ' + ui.badge(o.emergency ? 'OUT' : 'SOLD') + ' ' + tag(o.curve, symOf[o.curve]) + C.dim('  ' + (o.hash === 'paper' ? 'paper' : ui.short(o.hash)) + ' on the wire in ' + o.ms + ' ms' + (o.emergency ? ', pre-signed' : ''))); if (o.label.split(':').length < 3) delete open[o.curve]; } break;
      case 'error': log.warn('engine: ' + o.what); break;
      default: break;
    }
  }
  await new Promise(r => { const t = setInterval(() => { if (done) { clearInterval(t); r(); } }, 200); process.on('SIGINT', () => { finish(0); }); });
  return code;
};
