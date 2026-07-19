#!/usr/bin/env python3
"""flametool: convert JFR/perf output to folded stacks and render a flamegraph SVG.

Usage:
  flametool.py jfr2folded  <input.txt>  > out.folded   # input = `jfr print --events jdk.ExecutionSample`
  flametool.py perf2folded <input.txt>  > out.folded   # input = `perf script`
  flametool.py flame <in.folded> <out.svg> [--title T] [--mincount N]
"""
import re
import sys
import html


def jfr2folded(path):
    """Parse `jfr print --events jdk.ExecutionSample` text into folded stacks."""
    counts = {}
    in_event = False
    in_stack = False
    frames = []
    frame_re = re.compile(r"^\s+([\w.$]+\.[\w$<>]+)\(.*?\)\s+line:")
    with open(path) as f:
        for line in f:
            if line.startswith("jdk.ExecutionSample {"):
                in_event, frames = True, []
                continue
            if in_event and "stackTrace = [" in line:
                in_stack = True
                continue
            if in_stack:
                if line.strip() == "]":
                    if frames:
                        # jfr prints top-of-stack first; folded needs root-first
                        key = ";".join(reversed(frames))
                        counts[key] = counts.get(key, 0) + 1
                    in_event = in_stack = False
                    continue
                m = frame_re.match(line)
                if m:
                    frames.append(m.group(1))
    return counts


def clean_sym(sym):
    """Normalize a (possibly c++filt-demangled) symbol for display."""
    sym = re.sub(r"::h[0-9a-f]{16}$", "", sym)  # rust legacy hash suffix
    return sym


def perf2folded(path):
    """Parse `perf script` output into folded stacks (root-first)."""
    counts = {}
    frames = []
    header_re = re.compile(r"^\S+\s+\d+\s+[\d.]+:")
    with open(path) as f:
        for line in f:
            if not line.strip():
                if frames:
                    key = ";".join(reversed(frames))
                    counts[key] = counts.get(key, 0) + 1
                frames = []
                continue
            if header_re.match(line):
                continue  # sample header: comm pid time: event:
            m = re.match(r"^\s+[0-9a-f]+\s+(\S+)", line)
            if m:
                sym = m.group(1).split("+")[0]
                if sym not in ("[unknown]", "_start"):
                    frames.append(clean_sym(sym))
    if frames:
        key = ";".join(reversed(frames))
        counts[key] = counts.get(key, 0) + 1
    return counts


def flame(counts, out_path, title, mincount=1):
    """Render folded stacks to a flamegraph SVG (icicle-style, root at left)."""
    # Build prefix tree
    tree = {}  # level map: name -> {"count": n, "children": level map}
    total = 0
    for stack, cnt in counts.items():
        if cnt < mincount:
            continue
        total += cnt
        node = tree
        for name in stack.split(";"):
            f = node.get(name)
            if f is None:
                f = node[name] = {"count": 0, "children": {}}
            f["count"] += cnt
            node = f["children"]
    # Layout: rows of frames, depth-first
    frames = []  # (depth, x0, width, name, count)
    width_total = 1000.0

    def layout(node, depth, x0, w):
        for name, n in sorted(node.items(), key=lambda kv: -kv[1]["count"]):
            cw = w * n["count"] / (sum(c["count"] for c in node.values()) or 1)
            frames.append((depth, x0, cw, name, n["count"]))
            layout(n["children"], depth + 1, x0, cw)
            x0 += cw

    # layout per top-level proportionally
    layout(tree, 0, 0.0, width_total)
    max_depth = max((d for d, *_ in frames), default=0) + 1
    fh, top = 16, 40
    W, H = 1200, top + max_depth * fh + 10

    def color(name):
        h = sum(ord(c) for c in name) % 360
        warm = ("java" in name.lower()) or ("::" not in name and ".so" not in name)
        return f"hsl({h % 40 + 10},80%,65%)" if warm else f"hsl({h},45%,60%)"

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" font-family="monospace">',
        f'<text x="10" y="24" font-size="16">{html.escape(title)} (samples={total})</text>',
    ]
    for depth, x0, w, name, cnt in frames:
        if w < 0.5:
            continue
        x = x0 / width_total * (W - 20) + 10
        rw = w / width_total * (W - 20)
        label = ""
        if rw > 30:
            label = f'<text x="{x + 3:.1f}" y="{top + depth * fh + 12}" font-size="11">{html.escape(name[: int(rw / 6.5)])}</text>'
        parts.append(
            f'<rect x="{x:.1f}" y="{top + depth * fh}" width="{max(rw - 0.5, 0.5):.1f}" height="{fh - 1}" '
            f'fill="{color(name)}"><title>{html.escape(name)} ({cnt})</title></rect>{label}'
        )
    parts.append("</svg>")
    with open(out_path, "w") as f:
        f.write("\n".join(parts))


def main():
    cmd = sys.argv[1]
    if cmd == "jfr2folded":
        for k, v in sorted(jfr2folded(sys.argv[2]).items()):
            print(f"{k} {v}")
    elif cmd == "perf2folded":
        for k, v in sorted(perf2folded(sys.argv[2]).items()):
            print(f"{k} {v}")
    elif cmd == "flame":
        counts = {}
        with open(sys.argv[2]) as f:
            for line in f:
                stack, _, cnt = line.rstrip().rpartition(" ")
                counts[stack] = counts.get(stack, 0) + int(cnt)
        title = "Flame Graph"
        if "--title" in sys.argv:
            title = sys.argv[sys.argv.index("--title") + 1]
        flame(counts, sys.argv[3], title)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
