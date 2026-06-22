# Aschenbrenner Unlimited — Floor 2x / Ceiling UNLIMITED (stocks + options, v4)

Human-readable mirror of `2x_unlimited_stocks.pdf` (v4). The machine-authoritative
target table lives in `src/portfolio/targets.rs`; the two must stay in lockstep.

> Not investment advice. A structured thesis bet on a **paper** account. Floor =
> **2x by month 36** (≥$80K, ~62-68% probability). Ceiling = **uncapped** via
> long-dated OTM equity options. NO crypto (per user requirement). Supersedes the
> v3 crypto version.

## The three-bucket barbell (PDF §3)

| Tier | Weight | $ | Instrument | Role | Tail multiple |
|------|-------:|--:|------------|------|--------------:|
| A Floor builders | 50% | $20K | large-cap equity | protect the 2x floor | 3-4x |
| B Asymmetric multi-baggers | 32.5% | $13K | mid/small-cap equity | drive the 5-10x | 10-15x |
| D Moonshot sleeve | 12.5% | $5K | **LEAPS + micro-cap** | open the 100x tail; may zero | 30-150x |
| C Cash buffer | 5% | $2K | HYSA/SGOV (+$500 IPO reserve) | drawdown deployment | — |

"Unlimited" comes from the LEAPS: a $200 OTM call on an $80 stock that 10x's becomes
a $600+ ITM call → 30-50x. Same convex payoff as crypto, without the asset class.

## The 24 positions (PDF §4)

Weights normalize so the 22 invested positions sum to **95%**, cash holds **5%**.

**Tier A — Floor · 50%** (stocks): GEV 10, CEG 10, ETN 7.5, VST 7.5, KLAC 7.5, NOC 7.5.

**Tier B — Asymmetric · 32.5%** (stocks): IREN 5, APLD 3.8, CRWV 3.8, PLTR 3.8, STRL 3.8,
MTZ 2.5, BWXT 2.5, VRT 2.5, AVAV 2.5, OKLO 2.5.

**Tier D — Moonshot · 12.5%**:
| Slot | Instrument | $ | Notes |
|------|-----------|--:|-------|
| NVDA LEAPS | Jan-2028 $300C | $1,500 | anchor leg; lowest-failure LEAPS |
| CRWV LEAPS | Jan-2027 $200C | $1,000 | Aschenbrenner's #1 long, levered |
| IREN LEAPS | Jan-2027 $30C  | $1,000 | replaces v3's SOL/alt slot |
| OKLO LEAPS | Jan-2027 $150C | $500   | highest-beta single leg |
| Micro-cap #1 | config-pinned (default IONQ) | $500 | quantum slot |
| Micro-cap #2 | config-pinned (default ASTS) | $500 | defense/space slot |

LEAPS expiries resolve to the standard **3rd-Friday-of-January** monthly contract;
strikes are exact. Micro-cap picks are set in `.env` (`MICROCAP_1`/`MICROCAP_2`).

**Tier C — Cash · 5%**: HYSA $1,500 + $500 IPO reserve.

## Hold discipline (PDF §9) — `src/rebalance/rules.rs`

| Tier | Stop | Profit-take | Caps |
|------|------|-------------|------|
| A Floor | −25% review / −35% cut (advisory) | trim 25% at 3x, ride | 15%→10% position, 30% cluster |
| B Asym | **none** | trim 30% at 5x, ride | 15%→10% position, 30% cluster |
| D LEAPS | time-stop flag ≤60 DTE; never roll a loser | sell half at +500%, ride to expiry | **exempt** from equity caps |
| D micro | none (binary) | trim 50% at 10x | exempt |
| C Cash | — | — | deploy on single −25% / cohort −15% |

No leverage. No covered calls (you'd cap the upside you paid for). A 3% rebalance
band suppresses churn.

## Execution playbook (PDF §11) — `src/rebalance/build.rs`

- Week 1: Tier A floor ($20K, 6 names), limit orders, no chase.
- Week 2: Tier B asymmetric ($13K), DCA across the 10 names.
- Week 3: Tier D **LEAPS** ($4K) — buy on −3% dips, limit at option mid; never market-order.
- Weeks 4-6: micro-caps ($1K) — entry timing matters more for $5-15 names.
- Always keep the $2K cash buffer.

## Honest math (PDF §6-8)

P(≥2x floor by m36) ≈ 62-68%. P(≥10x by m60) ≈ 17-22%. P(≥100x by m60) ≈ 1-3%.
1000x is structurally impossible with a 2x floor + no crypto. The single biggest
risk is selling Tier B/D during the 25-40% drawdown that *will* happen — that's
what creates the tail; selling into it forfeits it.

## Daily-analysis overlay

The nightly multi-agent analysis biases rule-authorized equity trims/adds only
(most-bearish trimmed first, most-bullish funded first; macro regime tempers cash
deploy). It never creates a trade a rule didn't authorize, and never touches the
Tier-D moonshot lifecycle.
