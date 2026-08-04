"""Computed PVs with calc — the Python counterpart to
spvirit-server/examples/linked_calc.rs.

    python demo_calc.py
    pvput CALC:A 10 && pvput CALC:B 3
    pvget CALC:SUM      # 13
    pvmonitor CALC:MEAN
"""
import spvirit

# ANCHOR: links
a = spvirit.ao("CALC:A", 0.0)
b = spvirit.ao("CALC:B", 0.0)

total = spvirit.calc("CALC:SUM", [a, b], lambda v: v[0] + v[1])
product = spvirit.calc("CALC:PROD", [a, b], lambda v: v[0] * v[1])
mean = spvirit.calc("CALC:MEAN", [a, b], lambda v: (v[0] + v[1]) / 2.0)

server = spvirit.Server(pvs=[a, b, total, product, mean])
# ANCHOR_END: links

server.run()
