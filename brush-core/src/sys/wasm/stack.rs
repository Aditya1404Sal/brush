//! How much of the WASM shadow stack is left.
//!
//! Rust's WASM targets put the stack first in linear memory, growing down towards `__stack_low`;
//! running past it is an out-of-bounds access that traps the whole component, which a script's
//! recursion must never cause. Nested executions check what is left before they go deeper.

unsafe extern "C" {
    /// The lowest address of the stack region, defined by the linker.
    static __stack_low: u8;
}

/// The bytes left between the current stack pointer and the bottom of the stack.
#[inline(never)]
pub(crate) fn remaining() -> usize {
    let marker = 0_u8;
    let current = std::ptr::addr_of!(marker) as usize;
    // SAFETY: only the address of the linker-defined symbol is taken; it is never read.
    let low = unsafe { std::ptr::addr_of!(__stack_low) } as usize;
    current.saturating_sub(low)
}
