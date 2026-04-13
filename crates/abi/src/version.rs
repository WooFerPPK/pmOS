/// PMos OS ABI version. Kernel and userland check this at process spawn.
/// Bump rules per `contracts/syscalls.md §5`.
pub const ABI_VERSION: (u16, u16) = (1, 1);
