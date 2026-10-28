use slopos_abi::InputEvent;

slopos_service_core::define_service! {
    input => InputServices {
        drain_batch(task_id: u32, buffer: *mut InputEvent, max_count: usize) -> usize;
        clipboard_copy(login: u32, src: &[u8]) -> usize;
        clipboard_paste(login: u32, dst: &mut [u8]) -> usize;
        register_compositor(task_id: u32);
        switch_compositor(task_id: u32);
        cleanup_task(task_id: u32);
        route_key(keycode: u16, codepoint: u32, pressed: bool);
    }
}
