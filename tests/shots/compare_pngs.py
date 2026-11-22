"""Compare the dimensions and RGBA samples of two sets of captured PNGs."""

from pathlib import Path
import subprocess
import sys


def pixels(path):
    # PAM carries dimensions and RGBA samples without PNG encoding or metadata.
    return subprocess.check_output(
        ["convert", f"png:{path}", "-strip", "-alpha", "on", "-depth", "16", "pam:-"]
    )


def main(first_dir, second_dir):
    first = {path.name: path for path in Path(first_dir).glob("*.png")}
    second = {path.name: path for path in Path(second_dir).glob("*.png")}
    if not first and not second:
        print("The scenario produced no PNGs to compare.", file=sys.stderr)
        return 1

    differences = []
    for name in sorted(first.keys() | second.keys()):
        if name not in first:
            differences.append(f"{name} (missing from run 1)")
        elif name not in second:
            differences.append(f"{name} (missing from run 2)")
        else:
            try:
                if pixels(first[name]) != pixels(second[name]):
                    differences.append(name)
            except subprocess.CalledProcessError:
                differences.append(f"{name} (could not decode)")

    if differences:
        print("Differing screenshots:", file=sys.stderr)
        for difference in differences:
            print(f"  {difference}", file=sys.stderr)
        return 1
    print(f"Matched {len(first)} screenshot(s) pixel for pixel.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
