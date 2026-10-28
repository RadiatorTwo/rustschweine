# rustschweine

A native port of **Hogs of War** (German: *Frontschweine*, 2000) in Rust. The game's own
program is recompiled: the player's `warhogs.exe` and its 3D engine `Data/_d3d.dll` are
decoded with [iced-x86], every function is lifted to [Cranelift] IR and compiled to native
code, and the Windows/DirectX functions they call are to be implemented in Rust. Built like
[future_rust_cop]. No game code or data is part of this repository: you need your own copy
of the game.

Work in progress: the code of both programs is found and compiled; the platform layer is
next (see `PLAN.md`).

```
cargo test --release
cargo run --release --example functions -- path/to/game
```

[iced-x86]: https://github.com/icedland/iced
[Cranelift]: https://cranelift.dev
[future_rust_cop]: https://github.com/RadiatorTwo/future_rust_cop
