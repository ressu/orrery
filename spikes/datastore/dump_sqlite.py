"""Prints every row of an SQLite database as JSON lines, in the same format as
`datastore-spike --read-sqlite`, so the two can be diffed. Run with python3 -I."""
import json
import sqlite3
import struct
import sys


def decode(b):
    tag = b[0]
    if tag == ord("q"):
        return struct.unpack("<q", b[1:])[0]
    if tag == ord("d"):
        return struct.unpack("<d", b[1:])[0]
    if tag == ord("?"):
        return struct.unpack("?", b[1:])[0]
    if tag == ord("s"):
        return b[1:].decode()
    if tag in (ord("["), ord("{")):
        return json.loads(b)
    return None


def cell(v):
    if isinstance(v, bytes):
        return {"$blob": v.hex()}
    return v


conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
tables = [r[0] for r in conn.execute(
    "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
for t in tables:
    cur = conn.execute(f'SELECT * FROM "{t}" ORDER BY rowid')
    cols = [d[0] for d in cur.description]
    for r in cur:
        row = {c: cell(v) for c, v in zip(cols, r)}
        if t == "namespace_store":
            row["value"] = decode(r[2])
        print(json.dumps({"table": t, "row": row}, separators=(",", ":"), ensure_ascii=False))
