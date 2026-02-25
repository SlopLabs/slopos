# Scheduler and Tasking TODO

## Scheduling Enhancements
- [x] Calibrate/use LAPIC timer for preemption (HPET + LAPIC timer mandatory since Phase 0E).

## Async Coordination
- [ ] Extend join/wait primitives with timeout and cancellation support.
- [ ] Provide a lightweight async completion primitive for cross-task signaling.

_Pending:_ A detailed execution plan will be pushed to elaborate on each item before implementation starts.
