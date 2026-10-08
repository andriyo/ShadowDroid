#!/usr/bin/env python3
"""Assemble cli/tests/fixtures/dex/fields.dex by hand (no d8 needed).

The file is a minimal DEX (version 035) with exactly what the CLI's DEX
reader uses: header, string_ids, type_ids, proto_ids, field_ids,
method_ids, class_defs, class_data, and code items. It has no map_list,
so `dexdump` and ART would reject it; it is a parser fixture, not a
loadable class. Checksum and signature are filled in for realism.

    class io.example.app.Counter {            // Lio/example/app/Counter;
        private int count;                    // field 0
        private static int hits;              // field 1
        void bump()  { count = count + 1; }   // iget@0 add-int/lit8@2 iput@4 return@6
        void reset() { count = 0; int h = hits; }
                                               // const/4@0 iput@1 sget@3 return@5,
                                               // then a fill-array-data payload@6
                                               // whose data unit looks like iget
    }
    class io.example.app.Ui {                 // Lio/example/app/Ui;
        static void onClick(Counter c) { c.count = c.count; Counter.hits = c.count; }
                                               // iget@0 iput@2 sput@4 return@6
    }

Regenerate with: python3 cli/tests/fixtures/dex/make_fixture.py
"""
import hashlib
import os
import struct
import zlib

STRINGS = sorted([
    "I", "Lio/example/app/Counter;", "Lio/example/app/Ui;", "Ljava/lang/Object;",
    "V", "VL", "bump", "count", "hits", "onClick", "reset",
])
S = {s: i for i, s in enumerate(STRINGS)}
TYPES = ["I", "Lio/example/app/Counter;", "Lio/example/app/Ui;", "Ljava/lang/Object;", "V"]
T = {t: i for i, t in enumerate(TYPES)}
NO_INDEX = 0xFFFFFFFF


def uleb(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def units(*words):
    return b"".join(struct.pack("<H", w) for w in words)


BUMP = units(0x1052, 0x0000, 0x00D8, 0x0100, 0x1059, 0x0000, 0x000E)
RESET = units(0x0012, 0x1059, 0x0000, 0x0060, 0x0001, 0x000E,
              0x0300, 0x0001, 0x0002, 0x0000, 0x5952)
ON_CLICK = units(0x1052, 0x0000, 0x1059, 0x0000, 0x0067, 0x0001, 0x000E)


def code_item(registers, ins, insns):
    return struct.pack("<HHHHII", registers, ins, 0, 0, 0, len(insns) // 2) + insns


HEADER = 0x70
n_strings, n_types, n_protos, n_fields, n_methods, n_classes = len(STRINGS), len(TYPES), 2, 2, 3, 2
off = HEADER
string_ids_off = off; off += 4 * n_strings
type_ids_off = off; off += 4 * n_types
proto_ids_off = off; off += 12 * n_protos
field_ids_off = off; off += 8 * n_fields
method_ids_off = off; off += 8 * n_methods
class_defs_off = off; off += 32 * n_classes
data_off = off

data = bytearray()


def place(blob, align=1):
    while (data_off + len(data)) % align:
        data.append(0)
    at = data_off + len(data)
    data.extend(blob)
    return at


string_offsets = [place(uleb(len(s)) + s.encode() + b"\0") for s in STRINGS]
params_counter = place(struct.pack("<IH", 1, T["Lio/example/app/Counter;"]), 4)
bump_off = place(code_item(2, 1, BUMP), 4)
reset_off = place(code_item(2, 1, RESET), 4)
click_off = place(code_item(2, 1, ON_CLICK), 4)
# class_data: static fields, instance fields, direct methods, virtual methods
counter_data = place(
    uleb(1) + uleb(1) + uleb(0) + uleb(2)
    + uleb(1) + uleb(0x0A)                         # static hits (field 1)
    + uleb(0) + uleb(0x02)                         # instance count (field 0)
    + uleb(0) + uleb(0x01) + uleb(bump_off)        # bump (method 0)
    + uleb(1) + uleb(0x01) + uleb(reset_off)       # reset (method 1)
)
ui_data = place(uleb(0) + uleb(0) + uleb(1) + uleb(0)
                + uleb(2) + uleb(0x09) + uleb(click_off))  # onClick (method 2)

ids = bytearray()
for o in string_offsets:
    ids += struct.pack("<I", o)
for t in TYPES:
    ids += struct.pack("<I", S[t])
# protos: ()V, (Counter)V
ids += struct.pack("<III", S["V"], T["V"], 0)
ids += struct.pack("<III", S["VL"], T["V"], params_counter)
# fields: Counter.count:I, Counter.hits:I
ids += struct.pack("<HHI", T["Lio/example/app/Counter;"], T["I"], S["count"])
ids += struct.pack("<HHI", T["Lio/example/app/Counter;"], T["I"], S["hits"])
# methods: Counter.bump()V, Counter.reset()V, Ui.onClick(Counter)V
ids += struct.pack("<HHI", T["Lio/example/app/Counter;"], 0, S["bump"])
ids += struct.pack("<HHI", T["Lio/example/app/Counter;"], 0, S["reset"])
ids += struct.pack("<HHI", T["Lio/example/app/Ui;"], 1, S["onClick"])
for cls, class_data in (("Lio/example/app/Counter;", counter_data), ("Lio/example/app/Ui;", ui_data)):
    ids += struct.pack("<IIIIIIII", T[cls], 1, T["Ljava/lang/Object;"], 0, NO_INDEX, 0, class_data, 0)
assert HEADER + len(ids) == data_off

body = bytearray(ids) + data
file_size = HEADER + len(body)
header = bytearray(HEADER)
header[0:8] = b"dex\n035\0"
struct.pack_into("<I", header, 32, file_size)
struct.pack_into("<I", header, 36, HEADER)
struct.pack_into("<I", header, 40, 0x12345678)
struct.pack_into("<II", header, 0x38, n_strings, string_ids_off)
struct.pack_into("<II", header, 0x40, n_types, type_ids_off)
struct.pack_into("<II", header, 0x48, n_protos, proto_ids_off)
struct.pack_into("<II", header, 0x50, n_fields, field_ids_off)
struct.pack_into("<II", header, 0x58, n_methods, method_ids_off)
struct.pack_into("<II", header, 0x60, n_classes, class_defs_off)
struct.pack_into("<II", header, 0x68, len(data), data_off)
blob = bytearray(header) + body
blob[12:32] = hashlib.sha1(bytes(blob[32:])).digest()
struct.pack_into("<I", blob, 8, zlib.adler32(bytes(blob[12:])))

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fields.dex")
with open(out, "wb") as f:
    f.write(blob)
print(out, len(blob), "bytes")
