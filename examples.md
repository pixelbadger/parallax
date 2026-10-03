Example 1: The Observer Effect
Shows how if statements force collapse, while math operations defer it.

```
fn main() = {
    seed(42);
    let x : ?Int = open;
    
    # 1. Propagation
    # y is a Future (~Int). No random number generated yet.
    let y : ~Int = x + 10;
    
    # 2. Observation
    # The 'if' requires a concrete value, forcing 'y' (and thus 'x') to collapse.
    if (y > 50) {
        print(111, y);
    } else {
        print(222, y);
    }
}
```

Example 2: Time Travel (Forks)
Safe experimentation using branched environments.

```
type Result = { success, val };

fn main() = {
    seed(99);
    let reactor_heat = 0;

    # Fork a new timeline
    let experiment = fork {
        let spike = open;
        let reactor_heat = reactor_heat + spike; # Mutates LOCAL heat only
        
        # Return struct
        if (reactor_heat > 80) { Result { success: 0, val: reactor_heat } }
        else { Result { success: 1, val: reactor_heat } }
    };

    # Introspect result without committing
    if (experiment.success) {
        print(111); # Success
        commit experiment; # Merge the low heat change
    } else {
        print(222); # Failure
        discard experiment; # Rollback the dangerous heat change
    }
    
    print(333, reactor_heat);
}
```

Example 3: Pinning & Wormholes
Holding a collapsed value fixed across re-seeds of the RNG.

```
fn main() = {
    # Run 1
    seed(10);
    # This generates a random number and saves it to GlobalMemo as "key"
    pin key = open; 
    print(111, key);

    # Run 2 (New Seed)
    seed(9999);
    # Even though seed changed, "key" is pinned. It reuses the old value.
    pin key = open; 
    print(222, key); # Same as 111

    # Reset
    reset key;
    
    # Run 3
    # Key is forgotten, generates new value.
    pin key = open;
    print(333, key); 
}
```

Example 4: The Multiverse
Monte Carlo in one expression, conditioning with `given`.

```
type Dice = { total, double };

fn main() = {
    # How often do two dice sum to 7?
    let sevens = multiverse 1000 { open(1, 6) + open(1, 6) == 7 };
    print("P(7) %", sevens.rate);

    # Given the first die shows at least 4, what does the total look like?
    let r = multiverse 1000 {
        let a = open(1, 6);
        let b = open(1, 6);
        given a > 3;
        Dice { total: a + b, double: a == b }
    };
    print("kept", r.total.n, "mean total", r.total.mean, "doubles %", r.double.rate);
}
```

Example 5: Loops and Mutation
A random walk, run until it drifts 10 steps from home.

```
fn main() = {
    let pos = 0;
    let steps = 0;
    while (abs(pos) < 10) {
        pos = pos + open(0, 1) * 2 - 1;
        steps = steps + 1;
    }
    print("escaped after", steps, "steps");
}
```
