//! OS entropy for WebSocket masks and handshake nonces on Scarlet.

/// getrandom's custom backend, selected by the Scarlet target rustflags.
///
/// # Safety
/// The caller supplies `len` exclusively writable bytes at `dest`.
#[unsafe(no_mangle)]
pub unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // SAFETY: getrandom supplies one live, exclusively writable allocation.
    let buffer = unsafe { core::slice::from_raw_parts_mut(dest, len) };
    let mut offset = 0;
    while offset < buffer.len() {
        // SAFETY: the kernel writes only into the advertised remaining slice.
        let result = unsafe {
            scarlet_sys::syscall3(
                scarlet_sys::Syscall::GetRandom,
                buffer[offset..].as_mut_ptr() as usize,
                buffer.len() - offset,
                scarlet_sys::GET_RANDOM_FLAG_REQUIRE_ENTROPY,
            )
        };
        if result == 0 || result == usize::MAX || result > buffer.len() - offset {
            return Err(getrandom::Error::new_custom(1));
        }
        offset += result;
    }
    Ok(())
}
