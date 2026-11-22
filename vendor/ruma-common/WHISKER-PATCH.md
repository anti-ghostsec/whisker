# ruma-common 0.20.2, patched for Whisker

This is ruma-common 0.20.2 from crates.io (MIT licence,
<https://github.com/ruma/ruma>, copyright the Ruma developers), with one
change: in `src/room.rs` three lines

    let json: Box<RawJsonValue> = Box::deserialize(deserializer)?;

became

    let json = <Box<RawJsonValue>>::deserialize(deserializer)?;

Same meaning, but the type is named on the call. Typst's Unicode data (ICU)
turns on zerovec's `serde` feature, which adds more `Deserialize` impls for
`Box<…>`; the compiler then can't settle which `Box` the original lines mean
and gives up (E0275). It's used through `[patch.crates-io]` in the workspace
`Cargo.toml`, and can go once matrix-sdk uses a ruma-common without the
problem.
