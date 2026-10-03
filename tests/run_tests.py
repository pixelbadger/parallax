"""SPL test runner.

  python tests/run_tests.py           # check tests/*.spl and simulations/*.spl against their .out files
  python tests/run_tests.py --update  # regenerate the .out files

Each program runs with a fixed --seed, so output is reproducible. The runner
also checks that every example in examples.md runs, and that an unseeded run
can be replayed exactly from the seed it reports.
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INTERPRETER = ROOT / "interpreter.py"
SEED = "0"

def run(path, *flags):
    p = subprocess.run([sys.executable, str(INTERPRETER), *flags, str(path)],
                       capture_output=True, text=True, cwd=ROOT)
    return p.returncode, p.stdout, p.stderr

def main(update):
    failures = []

    for spl in sorted([*(ROOT / "tests").glob("*.spl"), *(ROOT / "simulations").glob("*.spl")]):
        rel = spl.relative_to(ROOT)
        code, out, err = run(rel, "--seed", SEED)
        expected = spl.with_suffix(".out")
        if code != 0:
            failures.append(f"{rel}: exited {code}\n{err}")
        elif update:
            expected.write_text(out)
            print(f"updated {expected.relative_to(ROOT)}")
        elif not expected.exists():
            failures.append(f"{rel}: missing {expected.name} (run with --update)")
        elif out != expected.read_text():
            failures.append(f"{rel}: output differs from {expected.name}\n--- got ---\n{out}")
        else:
            print(f"ok   {rel}")

    # Every fenced example in examples.md must run cleanly
    scratch = ROOT / "tests" / ".example.tmp"
    blocks = re.findall(r"```[^\n]*\n(.*?)```", (ROOT / "examples.md").read_text(), re.S)
    try:
        for n, block in enumerate(blocks, 1):
            scratch.write_text(block)
            code, _, err = run(scratch, "--seed", SEED)
            if code != 0: failures.append(f"examples.md example {n}: exited {code}\n{err}")
            else: print(f"ok   examples.md example {n}")

        # An unseeded run reports its seed; replaying with it gives identical output
        scratch.write_text("fn main() = { let a = open; let b = fork { open }; print(a, b); }")
        _, first, _ = run(scratch)
        seed = re.search(r"--seed (\d+)", first).group(1)
        _, replay, _ = run(scratch, "--seed", seed)
        if first.splitlines()[2:] != replay.splitlines()[1:]:
            failures.append(f"replay with --seed {seed} differs:\n{first}\nvs\n{replay}")
        else: print("ok   unseeded run replays from its reported seed")
    finally:
        scratch.unlink(missing_ok=True)

    for f in failures: print(f"FAIL {f}")
    return 1 if failures else 0

if __name__ == "__main__":
    sys.exit(main("--update" in sys.argv[1:]))
