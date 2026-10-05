# parallax by example

Each block below is a complete program. The test suite runs every one
(`cargo test --test programs examples`); a block whose first line is
`# error: ...` must be rejected with that message.

Run one with `parallax run file.px`, or describe it without running with
`parallax check file.px`. Both print JSON.

## 1. A decision, compared across shared worlds

```parallax
# Two bets on the same coin. Both policies see the same 1000 coins, so
# their difference is the bet, not luck.
action Bet = small | big

world Coin {
    uncertain heads ~ bernoulli(0.55)
}

model play(bet: Bet) -> Int = {
    let stake = match bet { small => 1, big => 3 }
    if Coin.heads { stake } else { -stake }
}

policy cautious = small
policy bold = big

study which_bet {
    worlds 1000
    seed 1
    maximize mean(outcome)
    report probability(outcome > 0), p5(outcome)
}
```

## 2. Inputs, units and records

```parallax
# What the host knows arrives as typed inputs, with units and bounds.
input distance: km between 0 km and 500 km = 120 km
input fuel_price: p/l = 145 p/l

unit l                                  # litres: a unit of our own

type Trip = { time: h, cost: £ }

action Speed = mph56 | mph70

world Road {
    uncertain congestion ~ triangular(0.0, 0.1, 0.6)
}

model drive(s: Speed) -> Trip = {
    let speed = match s { mph56 => 90 km/h, mph70 => 112 km/h }
    let economy = match s { mph56 => 18 km/l, mph70 => 14 km/l }
    let time = distance / speed * (1.0 + Road.congestion)
    Trip { time, cost: distance / economy * fuel_price }
}

policy steady = mph56
policy brisk = mph70

study commute {
    worlds 500
    minimize mean(cost)
    report mean(time), p95(time), mean(cost) in p
}
```

## 3. A family of candidates and a constraint

```parallax
# How many spare servers to keep? Each failure is keyed by server and
# hour, so every candidate faces the same failures.
type Outage = { down_hours: Int, cost: £ }

action Spares = Int

world Fleet {
    uncertain fails(server: Int, hour: Int) ~ bernoulli(0.002)
}

const SERVERS = 20
const HOURS = 168

model week(spares: Spares) -> Outage = {
    var down = 0
    for hour in 0..HOURS {
        let failed = count([Fleet.fails(s, hour) for s in 0..SERVERS])
        if failed > spares { down = down + 1 }
    }
    Outage { down_hours: down, cost: float(spares) * 40 £ + float(down) * 500 £ }
}

policy keep[n in 0..=3] = n

study spares {
    worlds 300
    require probability(down_hours > 0) <= 10%
    minimize mean(cost)
    report cvar95(cost), mean(down_hours)
}
```

## 4. Time, forecasts and an oracle

```parallax
# Each hour a pump runs or rests. Demand is uncertain hour by hour; the
# operator sees the tank level, not the coming demand.
type Tank = { level: Int, spilled: Int, dry: Int }
action Pump = run | rest

world Town {
    uncertain demand(hour: Int) ~ uniform(2, 8)
}

model waterworks {
    horizon 24
    init = Tank { level: 20, spilled: 0, dry: 0 }
    step(s: Tank, a: Pump, t: Int) -> Tank = {
        let inflow = if a == run { 6 } else { 0 }
        let level = s.level + inflow - Town.demand(t)
        Tank {
            level: clamp(level, 0, 40),
            spilled: s.spilled + max(0, level - 40),
            dry: s.dry + if level < 0 { 1 } else { 0 },
        }
    }
    observe(s: Tank, t: Int) -> Tank = s
}

policy always(s: Tank, t: Int) -> Pump = run
policy refill_below[x in [10, 20]](s: Tank, t: Int) -> Pump =
    if s.level < x { run } else { rest }

# Imagines the next 3 hours 20 times each way and picks the safer.
policy planner(s: Tank, t: Int) -> Pump = {
    let risk_rest = mean([float(r.dry) for r in forecast(rest, horizon: 3, worlds: 20)])
    if risk_rest > 0.0 { run } else { rest }
}

# Sees the real demand: an upper bound on what better forecasts buy.
oracle policy prophet(s: Tank, t: Int) -> Pump =
    if rollout(rest, horizon: 3).dry > s.dry { run } else { rest }

study day {
    worlds 200
    require probability(dry > 0) <= 5%
    minimize mean(spilled)
    report mean(dry), probability(dry > 0)
}
```

## 5. A policy built to be served

```parallax
# Retry a flaky call now, or wait for a backoff? A study picks the policy;
# then `parallax decide this.px --policy measured --input failures=3` gives
# one decision with its reasons, in a few thousand operations.
input failures: Int between 0 and 20 = 1       # failures in a row so far
input deadline: s = 30 s

type Call = { ok: Bool, took: s }
action Retry = now | backoff

world Service {
    latent down ~ bernoulli(0.2)                # an outage, not bad luck
    uncertain flake ~ bernoulli(0.3)
    uncertain recovery ~ lognormal(20 s, 0.8)
}

model attempt(r: Retry) -> Call = {
    # The more failures in a row, the likelier it's an outage.
    let outage = Service.down and failures >= 2
    let wait = match r { now => 0 s, backoff => 10 s }
    let up = not Service.flake and (not outage or wait > Service.recovery)
    Call { ok: up, took: wait + 1 s }
}

policy eager = now
policy patient = backoff

const BATCHES = [16, 48, 192]

# Compares the actions in growing batches of the same imagined worlds, and
# stops once one clearly wins where they differ (a sign test), or they
# rarely differ at all and the choice hardly matters.
policy measured = {
    var seen = 0
    var wins = 0                                # now beats backoff, net
    var differ = 0                              # worlds where they differ
    var clear = false
    for r in 0..len(BATCHES) while not clear {
        let k = BATCHES[r]
        let a = forecast(now, worlds: k, skip: seen)
        let b = forecast(backoff, worlds: k, skip: seen)
        for i in 0..k {
            if a[i].ok != b[i].ok {
                differ = differ + 1
                wins = wins + if a[i].ok { 1 } else { -1 }
            }
        }
        seen = seen + k
        clear = float(abs(wins)) > 3.0 * sqrt(float(max(differ, 1))) or differ * 10 < seen
    }
    note worlds = seen
    note differ = differ
    note retry_now_edge = float(wins) / float(seen) * 100%
    if wins >= 0 { now } else { backoff }
}

study retry {
    worlds 2000
    with failures = 3
    maximize probability(ok)
    report mean(took)
}
```

## What the language rejects

A policy can't peek at the world it is being evaluated in:

```parallax
# error: a policy can't read the world
action Bet = small | big
world Coin { uncertain heads ~ bernoulli(0.5) }
model play(bet: Bet) -> Int = if Coin.heads { 1 } else { -1 }
policy cheat = if Coin.heads { big } else { small }
study s { worlds 10 maximize mean(outcome) }
```

Functions are pure, and there is no recursion:

```parallax
# error: recursion is not allowed
fn fact(n: Int) -> Int = if n == 0 { 1 } else { n * fact(n - 1) }
```

There is no `while`: every loop has a bound known before running.

```parallax
# error: no `while` loop
fn f() -> Int = {
    var i = 0
    while i < 10 { i = i + 1 }
    i
}
```

Units are checked:

```parallax
# error: units don't match
const MIXED = 5 min + 3 kWh
```

A `note` is a policy's reason, so only policies give one:

```parallax
# error: use it in a policy's body
fn score(x: Int) -> Int = {
    note doubled = 2 * x
    x
}
```

Uncertainty only enters in a `world`:

```parallax
# error: uncertainty enters only in a `world`
fn noise() -> Float = uniform(0.0, 1.0)
```

`observe` reveals a fact whole or not at all, so forecasts can hold it
fixed without leaking anything else:

```parallax
# error: must reveal a world fact whole
type S = { n: Int }
type Seen = { hot: Bool }
action A = go
world W { uncertain temp(t: Int) ~ uniform(0, 40) }
model m {
    horizon 3
    init = S { n: 0 }
    step(s: S, a: A, t: Int) -> S = s
    observe(s: S, t: Int) -> Seen = Seen { hot: W.temp(t) > 30 }
    belief(o: Seen, t: Int) -> S = S { n: 0 }
}
```
