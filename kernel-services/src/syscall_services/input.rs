use slopos_abi::InputEvent;

slopos_service_core::define_service! {
    input => InputServices {
        drain_batch(task_id: u32, buffer: *mut InputEvent, max_count: usize) -> usize;
        take_sink(task_id: u32);
        switch_compositor(task_id: u32);
        cleanup_task(task_id: u32);
        route_key(keycode: u16, codepoint: u32, pressed: bool);
    }
}
