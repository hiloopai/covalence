# Lake module archives

Packs and unpacks the `.ltar` archives that [Lake](https://github.com/leanprover/lean4/tree/master/src/lake)
keeps one per module, in memory, with the same output as `leantar`.

An `.ltar` holds a module's trace and its outputs: `.olean` files in the
[`lgz`](https://github.com/digama0/leangz) object codec, and Zstandard frames
for the rest. Lake packs them with `leantar -s`, which drops the trace's
`depHash`, and supplies its input hash on unpack. [`unpack`] takes that hash
too; [`pack`] always drops it.

```no_run
let bytes = std::fs::read("Module.ltar")?;
let archive = covalence_lean_ltar::unpack(&bytes, 0x0123_4567_89ab_cdef)?;
for file in std::iter::once(&archive.trace).chain(&archive.files) {
    println!("{:?} {} ({} bytes)", file.dir, file.path, file.bytes.len());
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Supported: `LTR4` archives and `LGZ2` streams, which Lean 4.22 and later
write, including the scalar-array (`0xa0`) and sized-string (`0xb0`) opcodes
of [leangz#12](https://github.com/digama0/leangz/pull/12) and
[leangz#15](https://github.com/digama0/leangz/pull/15). Older versions fail
with [`Error::Unsupported`].

The crate's `leantar` binary runs the two command lines Lake uses,
`leantar -s -C <lib> -C <ir> OUT.ltar MOD.trace (-i <index> FILE)...` and
`leantar -C <lib> -C <ir> -x -j -`, and reports [`VERSION`] for
`leantar --version`. Install it as the Lean toolchain's `bin/leantar`.

## Provenance

The codec is a port of leangz's `lgz` and `ltar` code, Apache-2.0, at upstream
`33570f263222fe8aa4e071f4a30b1b69bfc0f4e3` merged with
[leangz#22](https://github.com/digama0/leangz/pull/22) head
`b15180b547e17bda933b7c7706db1a1ca611fcc8`. `src/v1.dict` is upstream's
Zstandard dictionary, unchanged. Each source file records its changes; the
licence is in `LICENSE-leangz`, and upstream ships no `NOTICE`. This crate is
therefore Apache-2.0 only.

Maintainer: Thomas. Retire this crate once the `leantar` that Lean distributions
ship supports scalar arrays, embedded NULs and deep nesting, and Lake unpacks
archives in process.
