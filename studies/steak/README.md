# Steak: pan temperature and flip schedule

Status: **validation gate not passed. The steak studies have not been run.**

`steak.px` is a parallax program. It is outside `simulations/` on purpose:
it isn't a finding yet, and it has no golden output.

## The question

Which pan temperature (180-260 °C) and flip schedule (once, every 30 s,
every 15 s) gives the thinnest grey band at a 54 °C core, while still
building a crust in 90% of worlds? Steaks are 15-40 mm thick and start at
-18 to 20 °C. Cooking loss, p95 grey band and time are reported.

## Deviations from the plan, and choices made without asking

- **The pan temperature is in the action**, `Move { pan, flip }`. The plan
  says "hold or flip", but the policy families range over pan temperature,
  and a family can only vary what the policy returns. The model reads the
  setting once, at t = 0.
- **21 nodes**, not 20, so the core is a node.
- **Implicit (backward Euler) conduction**, not explicit: at 1 s steps an
  explicit scheme is unstable for thin, frozen slabs on high contact
  conductance.
- **The crust proxy is the less-browned face's** seconds dry above 150 °C,
  so both sides must crust. The threshold `crust_need` (60 s) is a guess.
- **"Flip once"** flips when the core is halfway from its start to the
  target (a probe-thermometer rule). "Every 30/15 s" flip on the clock.
- **Grey** means peak temperature above 65 °C (a guess). With 21 nodes,
  node spacing is 0.75-2 mm, and the band is interpolated between nodes.

## Validation gate (before any study)

Pass bands, written down before running:

1. **Patty, one flip** (Thiffeault 2022): the best single flip time gives a
   cook time within ±5% of the published 80 s (76-84 s).
2. **Patty, many flips**: the best "flip every second, then stop" schedule
   is within ±5% of 62.6 s (59.5-65.7 s). This reference is Thiffeault's
   extrapolation, not a computed optimum, so it is the weaker check.
3. **Steak core temperature curves** (Zaragoza group, J. Food Eng. 298,
   110498, 2021; 19 mm steak, centre 30/44/57 °C at very rare, rare and
   done, weight loss 4/8/11%). The curves are needed, not just the end
   points: RMSE within the paper's own model error (2.2-4.6 °C).

Checks 1 and 2 test the solver and flip mechanics only (constant
properties, no water). Check 3 is the one that tests the physics.

## What would kill it

These were written before any steak study ran.

- **"Hottest pan wins."** If 260 °C is best at every start temperature and
  thickness, the likely cause is what the model leaves out: burnt crust,
  smoke point, fat spatter, carryover after resting. The model only rewards
  speed and penalises nothing on the surface except wetness. The answer
  would then come from the model's omissions, not from anything it found.
  The same applies to any optimum at the edge of the grid (180 or 260 °C).
- **The recommended pan temperature moves** by more than one grid step
  (20 °C) across the contact-conductance sweep (250-900 W/m²K). Then the
  answer depends on a number we only borrowed from patties, and the honest
  output is "measure contact conductance", not a temperature.
- **The crust constraint is all or nothing**: every policy passes, or none
  does. Then the guessed threshold, not the physics, picks the winner.
- **Paired differences between the top policies include 0.** Then they
  are indistinguishable, however the ranking reads.
- **Grey-band differences under ~0.5 mm** are below the grid resolution.
- **Frozen starts**: the apparent-heat-capacity melt with properties
  lagged by one step can misplace energy. If an energy-balance check
  (heat in vs sensible + latent stored) is off by more than 2%, discard
  the frozen-start results.

## Known omissions

No rest or carryover after the pan. No burning, smoke point, or crust
conductivity change. Properties don't change as water leaves. 1D, so no
edges. Contact conductance doesn't fall as the surface dries or shrinks.
None of the cooking-loss constants are Kondjoyan's fitted values: the form
is his, the numbers are placeholders.

## Validation results

Run 2026-10-06, parallax 0.2.0 from source:

```sh
parallax run studies/steak/steak.px --study patty_once  --input patty_substeps=1
parallax run studies/steak/steak.px --study patty_rapid --input patty_substeps=10
```

| Check | Reference | 1 s steps (as the steak runs) | 0.1 s steps | Verdict |
| :--- | :--- | :--- | :--- | :--- |
| 1. Patty, one flip | 80 s | 81.0 s (flip at 35 s) | 80.1 s (flip at 38 s) | **pass** |
| 2. Patty, many flips | 62.6 s (59.5-65.7) | 66.0 s (flips until 45 s) | 63.1 s (until 52 s) | **fail at 1 s** by 0.3 s; pass at 0.1 s |
| 3. Steak core curves | Zaragoza 2021 | not run | not run | **blocked**: no data |

- Check 2: at 1 s steps the solver is too coarse to follow a slab
  flipped every second. The error is time-step error: it shrinks to 0.5 s
  at 0.1 s steps. The steak schedules flip at most every 15 s, where
  check 1 says 1 s steps are within 1 s. But the band was set in advance,
  and at the step the steak uses, check 2 misses it.
- Check 3: the paper's full text, and its data, couldn't be reached from
  this environment (zaguan.unizar.es, sciencedirect.com and arxiv.org are
  blocked by the network policy). Only the abstract's end points are
  known, without the pan temperature or the times, so there is nothing to
  compare a curve against. Fitting to the end points alone would be
  tuning, not validation.
- Thiffeault's 10-flip result (69 s, optimised intervals of 6-11 s) was not
  reproduced. His schedule isn't published in what was reachable, and
  matching it would need his optimiser.

**So the gate stops here.** The steak studies stay unrun until check 3
has data.
