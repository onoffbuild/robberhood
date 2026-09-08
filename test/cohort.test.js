'use strict';
/* the cohort maths, without a chain: node --test */
const test = require('node:test');
const assert = require('node:assert/strict');
const K = require('../cli/cohort');

const CURVE = '0xc', DEV = '0xd', A = '0xa', B = '0xb', W = '0xw', X = '0xx';
const buy = (bn, who, quote, tokens) => ({ kind: 'buy', bn, idx: 0, who, recipient: who, quote, tokens });
const sell = (bn, who, tokens, quote) => ({ kind: 'sell', bn, idx: 0, who, recipient: who, quote, tokens });
const tr = (bn, from, to, tokens) => ({ bn, from, to, tokens });

test('hold %: ETH-weighted, a whale that stays outweighs three that leave', () => {
  const trades = [buy(100, W, 4, 400), buy(101, A, 1, 100), buy(102, B, 1, 100), buy(103, X, 1, 100), buy(900, DEV, 1, 100)];
  const transfers = [tr(100, CURVE, W, 400), tr(101, CURVE, A, 100), tr(102, CURVE, B, 100), tr(103, CURVE, X, 100),
    tr(500, A, CURVE, 100), tr(501, B, CURVE, 100), tr(502, X, CURVE, 60), tr(900, CURVE, DEV, 100)];
  const c = K.build({ trades, transfers }, { launchBn: 100, cohortBlocks: 300, deployer: DEV, exclude: [CURVE], nowBn: 1000 });
  assert.equal(c.cohort, 4, 'the deployer bought after the window');
  assert.ok(Math.abs(c.holdPct - 4 / 7 * 100) < 1e-9, 'W holds 4 of 7 ETH: ' + c.holdPct);
  assert.equal(c.holdCountPct, 25);
  assert.equal(c.holdersNow, 3, 'W, X (40 left) and DEV still hold something');
});

test('retracement, deployer ETH out through a funded wallet, top-5 of the cohort', () => {
  const trades = [buy(10, DEV, 1, 1000), buy(11, A, 1, 500), buy(12, B, 2, 500), sell(50, X, 300, 1.2), sell(60, B, 100, 0.3)];
  const transfers = [tr(10, CURVE, DEV, 1000), tr(11, CURVE, A, 500), tr(12, CURVE, B, 500), tr(20, DEV, X, 300), tr(50, X, CURVE, 300), tr(60, B, CURVE, 100)];
  const c = K.build({ trades, transfers }, { launchBn: 10, cohortBlocks: 100, deployer: DEV, exclude: [CURVE], nowBn: 100 });
  /* prices per trade: 0.001, 0.002, 0.004 (peak), 0.004, 0.003 (last) */
  assert.ok(Math.abs(c.retracePct - 75) < 1e-9, 'last 0.003 over peak 0.004: ' + c.retracePct);
  assert.equal(c.funded, 1, 'X was funded by the deployer');
  assert.ok(Math.abs(c.devEthOut - 1.2) < 1e-9, 'the funded wallet sold for 1.2 ETH');
  assert.ok(Math.abs(c.devOutPct - 30) < 1e-9, '1.2 of 4 ETH in');
  assert.ok(c.cohortTop5Pct === 100, 'three cohort wallets remain, all in the top-5');
});

test('verdict: survivor, careful, avoid', () => {
  const good = { cohort: 20, holdPct: 35, retracePct: 70, devOutPct: 5, cohortTop5Pct: 30, churn: 2, gone: 1, newHolders: 3 };
  assert.equal(K.verdict(good).stamp, 'SURVIVOR');
  assert.equal(K.verdict(Object.assign({}, good, { holdPct: 17 })).stamp, 'CAREFUL');
  const bad = K.verdict(Object.assign({}, good, { holdPct: 8, retracePct: 20 }));
  assert.equal(bad.stamp, 'AVOID');
  assert.equal(bad.why.length, 2);
  assert.equal(K.verdict(good, { minHold: 40 }).stamp, 'CAREFUL', 'the rules are yours');
});

test('no transfer logs: the trades stand in, a buyer who never sold holds', () => {
  const trades = [buy(10, A, 1, 100), buy(11, B, 1, 100), sell(50, B, 100, 0.9)];
  const c = K.build({ trades, transfers: [] }, { launchBn: 10, cohortBlocks: 100, deployer: DEV, curve: CURVE, nowBn: 100 });
  assert.equal(c.cohort, 2);
  assert.equal(c.holdPct, 50);
  assert.equal(c.holdersNow, 1);
});
