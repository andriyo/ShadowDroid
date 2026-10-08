# DEX fixture

`fields.dex` is a minimal, hand-assembled DEX file for the CLI's DEX reader
(`cli/src/jdwp/dex.rs`), which finds the instructions that read or write a
field so `debug break field` can watch a field that has no setter or getter.

It was produced by `make_fixture.py` in this directory (no `d8` or Android
SDK needed):

```sh
python3 cli/tests/fixtures/dex/make_fixture.py
```

The script writes the header, string/type/proto/field/method ids, class defs,
class data, and code items for two classes; its docstring lists the classes,
their fields, and the code index of every instruction. The file has no
`map_list`, so `dexdump` and ART reject it: it is a parser fixture, not a
loadable class. The fake JDWP VM (`cli/tests/support/fake_jdwp.rs`) serves
the same classes (`io.example.app.Counter`, `io.example.app.Ui`) with line
tables at those code indexes.

Regenerate it after changing the script, and update the expectations in
`dex.rs` and `src/jdwp/tests.rs`.
