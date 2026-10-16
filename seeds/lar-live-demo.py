"""Demo data for the live queries' screenshots (`demo_orders` on a scratch LAR).

    python3 seeds/lar-live-demo.py > seed.sql && psql -h 127.0.0.1 -p 5490 -U lar -d lar -f seed.sql

The `lar-live*` demo stages (`DATAMAN_BENCH_PG=127.0.0.1:5490:lar::lar`) SUBSCRIBE to
`demo_orders WHERE id >= 49190` and write rows from 49300 up (and 50000 up for
`lar-live-burst`) while the capture runs; the stage deletes them again before it writes.
"""
import random, datetime
random.seed(7)
out = []
def w(s): out.append(s + ";")
w("DROP TABLE IF EXISTS demo_orders")
w("CREATE TABLE demo_orders (id BIGINT PRIMARY KEY, demo_customer BIGINT, status TEXT, total DOUBLE PRECISION, created_at TIMESTAMPTZ, updated_at TIMESTAMPTZ)")
T0 = datetime.datetime(2026, 10, 12, 9, 0, 0)
def ts(d): return "'" + d.strftime('%Y-%m-%d %H:%M:%S') + "+00'"
st = ['new', 'paid', 'shipped', 'paid', 'shipped', 'new']
rows = []
for i in range(48000, 49200):
    cr = T0 + datetime.timedelta(seconds=(i - 48000) * 17)
    rows.append(f"({i}, {random.randint(1000, 1199)}, '{random.choice(st)}', {random.randint(1, 12) * 49.5:.2f}, {ts(cr)}, {ts(cr)})")
# The row the `lar-live` stage updates (new → paid) while the grid is live.
rows.append("(49200, 1042, 'new', 84.90, '2026-10-12 14:39:57+00', '2026-10-12 14:39:57+00')")
for j in range(0, len(rows), 300):
    w("INSERT INTO demo_orders VALUES " + ",".join(rows[j:j + 300]))
print("\n".join(out))
