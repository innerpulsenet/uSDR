#!/usr/bin/env python3
"""Regenerates tables.rs from fldigi's MT63 data files.

Usage: gen_tables.py <fldigi/src/mt63> > tables.rs

The symbol shape and the interleave patterns are read from symbol.dat and
mt63intl.dat as text, never retyped. Nothing in the fldigi tree is executed.
"""
import re
import sys


def block(text, name):
    m = re.search(name + r"\[[^\]]*\]\s*=\s*\{(.*?)\}", text, re.S)
    if not m:
        sys.exit("no table " + name)
    # Comments carry the index of each entry; they are not values.
    body = re.sub(r"//[^\n]*", "", m.group(1))
    body = re.sub(r"/\*.*?\*/", "", body, flags=re.S)
    return [v for v in re.split(r"[,\s]+", body) if v]


def rust(name, ty, vals, per_line, fmt):
    out = ["pub(super) const %s: [%s; %d] = [" % (name, ty, len(vals))]
    for i in range(0, len(vals), per_line):
        out.append("    " + " ".join(fmt(v) + "," for v in vals[i : i + per_line]))
    out.append("];")
    return "\n".join(out)


root = sys.argv[1]
sym = open(root + "/symbol.dat").read()
intl = open(root + "/mt63intl.dat").read()
shape = block(sym, "SymbolShape")
short = block(intl, "ShortIntlvPatt")
long_ = block(intl, "LongIntlvPatt")
if len(shape) != 512 or len(short) != 64 or len(long_) != 64:
    sys.exit("unexpected table length")
print("//! Generated mechanically by gen_tables.py from fldigi's symbol.dat and")
print("//! mt63intl.dat. Do not edit by hand.")
print()
print("/// Symbol shape (also the FFT window), 512 samples.")
print(rust("SYMBOL_SHAPE", "f32", shape, 6, lambda v: "%.8f" % float(v)))
print()
print("/// Interleave pattern, short interleave (32 symbols).")
print(rust("SHORT_INTERLEAVE", "usize", short, 16, str))
print()
print("/// Interleave pattern, long interleave (64 symbols).")
print(rust("LONG_INTERLEAVE", "usize", long_, 16, str))
