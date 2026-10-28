//! Hogs of War / Frontschweine (2000) recompiled to native code with Cranelift.
//!
//! The player's warhogs.exe and its 3D engine Data/_d3d.dll are decoded (iced-x86), every
//! function is lifted to Cranelift IR and compiled to host code; game pointers stay 32-bit in
//! a 4 GB guest address space.

pub mod discover;
pub mod game;
pub mod image;
pub mod iso;
pub mod platform;
pub mod jit;
pub mod lift;
pub mod rt;
pub mod smc;
