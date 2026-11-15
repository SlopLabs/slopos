# Pending for the docs

- `docs/development/adding-tests`, `docs/getting-started/qemu-options`: a kernel
  test reaches a disk the suite attaches only by identity, through
  `slopos_fs::tests::suite_disks` (feature `tests`).
  `scratch_disk(TestDisk::…)` and `scratch_disk_by_id(b"<by-id name>")`
  answer the node of the one registered whole disk whose `DiskId` matches
  the table, built with `DiskId::nvme`/`virtio`/`usb` from the serials
  `scripts/qemu_run.sh` gives. Scratch: slopos-root namespace 2, slopos-4kn
  namespace 2, slopos-spare, slopos-scratch, slopos-usb. Kept, reached only
  through `suite_disk`: slopos-root namespace 1, slopos-4kn namespace 1,
  slopos-verified. Anything else is a `Refusal` (`Absent`, `Ambiguous`,
  `NotScratch`, `Unknown`). Never claim, write, mount or partition by node
  name: on hardware `nvme0n2`, `vdb` or `sda` is somebody's disk. A test
  whose subject is node naming may still spell names. A disk added to
  `qemu_run.sh` gets a `TestDisk` variant; `test_block_scratch_refuses_other_disks`
  holds the refusals. The qemu-options table could name each disk's by-id
  identity beside its node.
- `docs/abi/ktap-test-output` (fsperf): `FSIMG[<phase>]: bytes= coldroot=
  coldimage= warmroot= warmimage=` (ns), from
  `slopos_fs::tests::image_read`: one 768 KiB file faulted in from the root
  disk and from an image of it, cold after a remount, then warm. Graded as
  `max-image-cold-pct` (image over root, capped at twice the emitted
  observation) and `min-image-bytes`; warm both are page-set hits, reported
  only.
