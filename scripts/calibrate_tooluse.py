#!/usr/bin/env python3
"""Estimate tooluse.px's `calibrated` inputs from a log of agent tasks.

    scripts/calibrate_tooluse.py simulations/tooluse_log.jsonl > simulations/tooluse_calibration.json
    scripts/calibrate_tooluse.py --verify simulations/tooluse_log.jsonl > verify_inputs.json

Each log line is one task (a yes/no claim whose truth is known):

    {"id": "c000", "truth": true, "draft": true, "difficulty": 0.3,
     "calls": {"context": [true, true], "code": [true, null], ...},
     "cost": {"code": 0.004, ...}, "latency": {"code": 3.1, ...}}

`calls[source]` lists that source's answers in order: true, false, or null
(couldn't answer). Two calls per source are what separate a source that is
misled on a task (wrong every time) from one that is merely noisy. `cost`
($) and `latency` (s) are per call. Sources missing from the log keep their
preset (the novel domain's), and so does `ask` unless it was measured.

The estimates are the ones tooluse.px's header describes:
  fail    share of first calls that couldn't answer
  misled  (w2 - w1^2) / (1 - 2 w1 + w2), from tasks where both calls
          answered: w1 the share of first calls wrong, w2 of both wrong
  acc     1 - (w1 - misled) / (1 - misled), then scaled for difficulty
For `context` (thinking again), being misled means echoing the draft, so
it's estimated from how often it agrees with the draft when the draft is
right and when it's wrong. Counts get half a pseudo-observation, so a
source that was never wrong comes out very good rather than perfect.
"""
import json
import sys

SOURCES = ["context", "web", "github", "code", "doc", "ask"]
# The novel preset, for sources the log doesn't measure
PRESET = {
    "context": dict(acc=0.60, misled=0.35, fail=0.00, cost=0.002, latency=5),
    "web": dict(acc=0.70, misled=0.30, fail=0.95, cost=0.01, latency=15),
    "github": dict(acc=0.85, misled=0.10, fail=0.20, cost=0.01, latency=20),
    "code": dict(acc=0.99, misled=0.02, fail=0.05, cost=0.01, latency=10),
    "doc": dict(acc=0.93, misled=0.07, fail=0.05, cost=0.01, latency=10),
    "ask": dict(acc=0.98, misled=0.02, fail=0.10, cost=2.0, latency=300),
}
MISLED_CAP = 1.0   # an input chance; tooluse.px then caps misled_at at 0.9


def clamp(x, lo, hi):
    return max(lo, min(hi, x))


def model_acc(measured):
    """The model makes calls noisier with difficulty (acc_at); at an
    average difficulty of 0.5 a source's acc shows as 0.5 + (acc - 0.5) * 0.75."""
    return clamp(0.5 + (measured - 0.5) / 0.75, 0.5, 0.995)


def tool(tasks, src):
    firsts = [t["calls"][src][0] for t in tasks if t["calls"].get(src)]
    n = len(firsts)
    fail = (sum(a is None for a in firsts) + 0.5) / (n + 1)
    pairs = [(c[0] == t["truth"], c[1] == t["truth"]) for t in tasks
             if len(c := t["calls"].get(src, [])) >= 2 and c[0] is not None and c[1] is not None]
    k = len(pairs)
    w1 = (sum(not a for a, _ in pairs) + 0.5) / (k + 1)
    w2 = (sum(not a and not b for a, b in pairs) + 0.25) / (k + 1)
    misled = clamp((w2 - w1 * w1) / (1 - 2 * w1 + w2), 0.0, MISLED_CAP)
    acc = 1 - (w1 - misled) / (1 - misled)
    return dict(acc=model_acc(acc), misled=misled, fail=fail), dict(tasks=n, pairs=k,
        first_wrong=sum(not a for a, _ in pairs), both_wrong=sum(not a and not b for a, b in pairs),
        unable=sum(a is None for a in firsts))


def reflection(tasks):
    """Agreement with the draft when it's right is a(1-m) + m; when it's
    wrong, (1-a)(1-m) + m. Solve for m and a from the two rates."""
    calls = [(t["draft"] == t["truth"], a == t["draft"]) for t in tasks
             for a in t["calls"].get("context", []) if a is not None]
    right = [same for ok, same in calls if ok]
    wrong = [same for ok, same in calls if not ok]
    p_r = (sum(right) + 0.5) / (len(right) + 1)     # agrees with a right draft
    p_w = (sum(wrong) + 0.5) / (len(wrong) + 1)     # agrees with a wrong draft
    m = clamp(p_r + p_w - 1, 0.0, MISLED_CAP)        # adding the two equations
    # Each rate gives its own `a` given m; average them.
    a_right = (p_r - m) / (1 - m) if m < 1 else 0.5
    a_wrong = 1 - (p_w - m) / (1 - m) if m < 1 else 0.5
    acc = model_acc(clamp((a_right + a_wrong) / 2, 0.0, 1.0))
    # tooluse.px echoes at most 0.9 of the time. If reflection echoes more
    # than that, the model's remaining "honest" reflections would be news
    # the data never showed: make them uninformative instead.
    if m > 0.9:
        acc = 0.5
    return dict(acc=acc, misled=m, fail=0.0), dict(
        calls=len(calls), changed_right_draft=len(right) - sum(right),
        changed_wrong_draft=len(wrong) - sum(wrong))


def prior(tasks):
    """Least squares of draft-right on difficulty, as `easy` (0) and `hard` (1)."""
    xs = [t["difficulty"] for t in tasks]
    ys = [1.0 if t["draft"] == t["truth"] else 0.0 for t in tasks]
    n = len(xs)
    mx, my = sum(xs) / n, sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx if sxx else 0.0
    easy = clamp(my - slope * mx, 0.01, 0.99)
    hard = clamp(my + slope * (1 - mx), 0.01, 0.99)
    return dict(easy=round(easy, 3), hard=round(hard, 3)), dict(tasks=n, draft_right=int(sum(ys)))


def mean_of(tasks, field, src):
    xs = [t[field][src] for t in tasks if src in t.get(field, {})]
    return sum(xs) / len(xs) if xs else None


def verify_inputs(tasks):
    """verify.px's telemetry: per-call rates for reading the source
    (github) and running code, and the draft's line over difficulty."""
    out = {}
    p, _ = prior(tasks)
    out["draft_easy"], out["draft_hard"] = round(p["easy"] * 100, 2), round(p["hard"] * 100, 2)
    for src, name in [("github", "source"), ("code", "code")]:
        calls = [(a, t["truth"]) for t in tasks for a in t["calls"].get(src, [])]
        said = [(a, truth) for a, truth in calls if a is not None]
        out[f"{name}_unable"] = round(100 * (len(calls) - len(said) + 0.5) / (len(calls) + 1), 2)
        out[f"{name}_right"] = round(100 * (sum(a == truth for a, truth in said) + 0.5) / (len(said) + 1), 2)
        out[f"{name}_cost"] = round(mean_of(tasks, "cost", src), 4)
        out[f"{name}_time"] = round(mean_of(tasks, "latency", src), 2)
    return out


def main(path):
    tasks = [json.loads(line) for line in open(path) if line.strip()]
    measured = {s for t in tasks for s in t["calls"]}
    sources, evidence = [], {}
    for s in SOURCES:
        est = dict(PRESET[s])
        if s in measured:
            rates, evidence[s] = reflection(tasks) if s == "context" else tool(tasks, s)
            est.update(rates)
        else:
            evidence[s] = "preset (not measured)"
        for field in ("cost", "latency"):
            m = mean_of(tasks, field, s)
            if m is not None:
                est[field] = m
        sources.append({k: round(v, 4) for k, v in est.items()})
    p, evidence["prior"] = prior(tasks)
    print(json.dumps({"domain": "calibrated", "prior_right": p, "sources": sources}, indent=2))
    print(json.dumps(evidence, indent=2), file=sys.stderr)


if __name__ == "__main__":
    if sys.argv[1] == "--verify":
        rows = [json.loads(line) for line in open(sys.argv[2]) if line.strip()]
        print(json.dumps(verify_inputs(rows), indent=2))
    else:
        main(sys.argv[1])
