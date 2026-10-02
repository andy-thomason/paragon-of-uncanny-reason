#!/usr/bin/env python3
"""Build tests/manifest.json: a classification of every Verilator regression test.

Original code for this project. The Verilator test drivers (t_*.py) are read
as *data*: each is parsed with Python's `ast` module and never executed or
imported. We record which harness calls each driver makes, with their literal
arguments, so our own runner can reproduce the checks.

Usage: python3 tools/manifest.py [path-to-test_regress/t] [-o tests/manifest.json]
"""

import argparse
import ast
import json
import os
import re
import shlex
import sys
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_T = os.path.join(HERE, "..", "..", "verilator", "test_regress", "t")
DEFAULT_OUT = os.path.join(HERE, "..", "tests", "manifest.json")

# Keyword arguments whose values are lists of command-line flags.
FLAG_KWARGS = ("verilator_flags", "verilator_flags2", "verilator_flags3", "v_flags", "v_flags2")
# Calls that check something about the run.
CHECK_CALLS = {
    "file_grep", "file_grep_not", "file_grep_any", "file_grep_count", "files_identical",
    "files_identical_sorted", "vcd_identical", "saif_identical", "glob_some", "glob_one",
    "file_contents", "extract", "inline_checks",
}
# Flags that only make sense for Verilator's C++ output (tier T3 features).
VLT_FEATURE_FLAGS = (
    "--public", "--public-flat-rw", "--trace", "--trace-fst", "--trace-vcd", "--trace-saif",
    "--coverage", "--coverage-line", "--coverage-toggle", "--coverage-user", "--protect-lib",
    "--protect-ids", "--protect-key", "--hierarchical", "--lib-create", "--json-only", "--xml-only",
    "--sc", "--dpi-hdr-only", "--vpi", "--savable", "--prof-cfuncs", "--prof-exec", "--prof-pgo",
    "-E", "--dump-defines", "--preproc-comments", "--preproc-defines", "--preproc-resolve",
)


def literal(node, src):
    """A literal value, or {"dynamic": source} if it can't be evaluated statically."""
    try:
        return ast.literal_eval(node)
    except Exception:
        return {"dynamic": ast.get_source_segment(src, node)}


def split_flags(values):
    out = []
    for v in values if isinstance(values, list) else [values]:
        if isinstance(v, str):
            try:
                out.extend(shlex.split(v))
            except ValueError:
                out.extend(v.split())
        else:
            out.append(v)
    return out


def long_form(flag):
    """Spell a long option with two dashes: -trace -> --trace. Single letters stay (-E)."""
    if isinstance(flag, str) and flag.startswith("-") and len(flag.lstrip("-")) > 1:
        return "--" + flag.lstrip("-")
    return flag


def licence_of(path):
    try:
        with open(path, encoding="latin-1") as f:
            head = f.read(4096)
    except OSError:
        return None
    m = re.search(r"SPDX-License-Identifier:\s*(\S+)", head)
    return m.group(1) if m else "unknown"


class Driver(ast.NodeVisitor):
    """Collect `test.<method>(...)` calls and `test.<attr> = ...` assignments.

    Calls into helper modules imported from the test directory, such as
    `common.run(test)`, are followed into that module's function (parsed as
    data, like the driver)."""

    def __init__(self, src, t_dir=None, seen=None):
        self.src = src
        self.calls = []
        self.attrs = {}
        self.depth = 0  # nesting inside if/for/while/def
        self.t_dir = t_dir
        self.imports = set()
        self.seen = seen if seen is not None else set()

    def visit_Import(self, node):
        for a in node.names:
            self.imports.add(a.asname or a.name)

    def follow(self, module, func):
        """Visit `func` in helper module `module`, if it lives in the test directory."""
        key = (module, func)
        path = os.path.join(self.t_dir or "", module + ".py")
        if key in self.seen or not self.t_dir or not os.path.exists(path):
            return
        self.seen.add(key)
        src = open(path, encoding="utf-8", errors="replace").read()
        try:
            tree = ast.parse(src)
        except SyntaxError:
            return
        for node in tree.body:
            if isinstance(node, ast.FunctionDef) and node.name == func:
                sub = Driver(src, self.t_dir, self.seen)
                sub.imports = {a.asname or a.name for n in tree.body if isinstance(n, ast.Import) for a in n.names}
                for stmt in node.body:
                    sub.visit(stmt)
                for c in sub.calls:
                    c["via"] = f"{module}.{func}"
                self.calls.extend(sub.calls)
                self.attrs.update(sub.attrs)

    def _nested(self, node):
        self.depth += 1
        self.generic_visit(node)
        self.depth -= 1

    visit_If = visit_For = visit_While = visit_FunctionDef = visit_With = visit_Try = _nested

    def visit_Assign(self, node):
        for t in node.targets:
            if isinstance(t, ast.Attribute) and isinstance(t.value, ast.Name) and t.value.id == "test":
                self.attrs[t.attr] = literal(node.value, self.src)
        self.generic_visit(node)

    def visit_Call(self, node):
        f = node.func
        if (isinstance(f, ast.Attribute) and isinstance(f.value, ast.Name) and f.value.id in self.imports
                and f.value.id != "test"):
            self.follow(f.value.id, f.attr)
        if isinstance(f, ast.Attribute) and isinstance(f.value, ast.Name) and f.value.id == "test":
            call = {"call": f.attr}
            if node.args:
                call["args"] = [literal(a, self.src) for a in node.args]
            kw = {k.arg: literal(k.value, self.src) for k in node.keywords if k.arg}
            if kw:
                call["kwargs"] = kw
            if self.depth:
                call["conditional"] = True
            self.calls.append(call)
        self.generic_visit(node)


def classify(name, t_dir, entry):
    calls = entry["calls"]
    kinds = [c["call"] for c in calls]
    tags = set()
    scenarios = entry["scenarios"]

    flags = []
    for c in calls:
        for k in FLAG_KWARGS:
            v = c.get("kwargs", {}).get(k)
            if isinstance(v, (list, str)):
                flags.extend(split_flags(v))
    entry["flags"] = flags

    if "skip" in kinds and not any(c.get("conditional") for c in calls if c["call"] == "skip"):
        tags.add("skipped")
    if "dist" in scenarios:
        tags.add("dist")
    fails = any(c.get("kwargs", {}).get("fails") is True for c in calls if c["call"] in ("compile", "lint", "execute"))
    if fails:
        tags.add("expect-fail")
    if any("expect_filename" in c.get("kwargs", {}) for c in calls):
        tags.add("golden")
    if "execute" in kinds:
        tags.add("execute")
    if "lint" in kinds:
        tags.add("lint")

    # Checks on Verilator internals.
    for c in calls:
        if c["call"] in CHECK_CALLS:
            arg0 = (c.get("args") or [None])[0]
            text = json.dumps(arg0)
            if "stats" in text:
                tags.add("internal-stats")
            elif "obj_dir" in text or "glob" in c["call"]:
                tags.add("internal-objdir")
            elif c["call"] in ("vcd_identical", "saif_identical") or "trace" in text or ".vcd" in text or ".fst" in text:
                tags.add("trace")
            else:
                tags.add("check-other")
    if any(f in ("--stats", "--dump-tree", "--debug") or str(f).startswith("--dump") for f in flags):
        tags.add("internal-dump")

    # Waivers from the analysis-plan decisions.
    cpp = [f for f in flags if isinstance(f, str) and f.endswith((".cpp", ".cc", ".c"))]
    if cpp or entry.get("pli_filename") or os.path.exists(os.path.join(t_dir, name + ".cpp")):
        tags.add("harness-cpp")
    if "uvm" in name:
        tags.add("uvm")
    # Verilator accepts long options with one dash too (-trace, -sc).
    long_flags = [long_form(f) for f in flags]
    if "--sc" in long_flags:
        tags.add("systemc")
    if any(f in VLT_FEATURE_FLAGS or (isinstance(f, str) and f.startswith(("--trace", "--coverage", "--public")))
           for f in long_flags):
        tags.add("vlt-feature")
    if "--timing" in long_flags:
        tags.add("timing")

    # Tier: T4 internals, T3 Verilator features and waived categories, T2 diagnostics, T1 behaviour.
    if tags & {"dist", "internal-stats", "internal-objdir", "internal-dump"}:
        tier = "T4"
    elif tags & {"harness-cpp", "systemc", "uvm", "vlt-feature", "trace"}:
        tier = "T3"
    elif "expect-fail" in tags or ("golden" in tags and "lint" in tags):
        tier = "T2"
    elif "execute" in tags:
        tier = "T1"
    elif "lint" in tags or "compile" in kinds:
        tier = "T2" if "golden" in tags else "T1-compile"
    else:
        tier = "other"
    entry["tags"] = sorted(tags)
    entry["tier"] = tier


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("t_dir", nargs="?", default=DEFAULT_T)
    ap.add_argument("-o", "--output", default=DEFAULT_OUT)
    a = ap.parse_args()
    t_dir = os.path.abspath(a.t_dir)
    drivers = sorted(f for f in os.listdir(t_dir) if f.startswith("t_") and f.endswith(".py"))

    tests, unparsed = [], []
    for d in drivers:
        name = d[:-3]
        path = os.path.join(t_dir, d)
        src = open(path, encoding="utf-8", errors="replace").read()
        try:
            tree = ast.parse(src)
        except SyntaxError as e:
            unparsed.append({"name": name, "error": str(e)})
            continue
        v = Driver(src, t_dir)
        v.visit(tree)
        scen = [s for c in v.calls if c["call"] == "scenarios" for s in c.get("args", []) if isinstance(s, str)]
        top = v.attrs.get("top_filename", f"t/{name}.v")
        if not isinstance(top, str):
            top = f"t/{name}.v"
        golden = v.attrs.get("golden_filename", f"t/{name}.out")
        entry = {
            "name": name,
            "scenarios": scen,
            "top": top,
            "golden": golden if isinstance(golden, str) and os.path.exists(os.path.join(t_dir, "..", golden)) else None,
            "calls": [c for c in v.calls if c["call"] != "scenarios"],
        }
        for k in ("pli_filename", "sim_time"):
            if k in v.attrs:
                entry[k] = v.attrs[k]
        top_path = os.path.join(t_dir, "..", top)
        entry["top_exists"] = os.path.exists(top_path)
        entry["licence"] = licence_of(top_path)
        classify(name, t_dir, entry)
        tests.append(entry)

    summary = {
        "tests": len(tests),
        "unparsed": len(unparsed),
        "tiers": dict(Counter(t["tier"] for t in tests).most_common()),
        "tags": dict(Counter(tag for t in tests for tag in t["tags"]).most_common()),
        "licences": dict(Counter(t["licence"] for t in tests).most_common()),
    }
    os.makedirs(os.path.dirname(os.path.abspath(a.output)), exist_ok=True)
    with open(a.output, "w") as f:
        json.dump({"source": t_dir, "summary": summary, "tests": tests, "unparsed": unparsed}, f, indent=1)
    json.dump(summary, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
