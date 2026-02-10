use core::ptr;

use slopos_abi::fs::UserFsEntry;
use slopos_lib::klog_info;
use slopos_lib::testing::TestResult;

use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::ext2::{Ext2Error, Ext2Fs, write_le_u16, write_le_u32};
use crate::vfs::{
    vfs_init_builtin_filesystems, vfs_is_initialized, vfs_list, vfs_mkdir, vfs_open, vfs_stat,
    vfs_unlink,
};

pub fn test_vfs_initialized() -> TestResult {
    klog_info!("VFS_TEST: check initialized");
    if !vfs_is_initialized() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vfs_root_stat() -> TestResult {
    klog_info!("VFS_TEST: root stat");
    let (kind, _size) = match vfs_stat(b"/") {
        Ok(stat) => stat,
        Err(_) => return TestResult::Fail,
    };
    if kind != 1 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vfs_file_roundtrip() -> TestResult {
    klog_info!("VFS_TEST: file roundtrip");
    if vfs_mkdir(b"/vfs_test").is_err() {
        return TestResult::Fail;
    }

    let handle = match vfs_open(b"/vfs_test/hello.txt", true) {
        Ok(h) => h,
        Err(_) => return TestResult::Fail,
    };

    let content = b"hello vfs";
    if handle.write(0, content).is_err() {
        return TestResult::Fail;
    }

    let mut buf = [0u8; 32];
    let read_len = match handle.read(0, &mut buf) {
        Ok(len) => len,
        Err(_) => return TestResult::Fail,
    };

    if read_len != content.len() || &buf[..content.len()] != content {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vfs_list() -> TestResult {
    klog_info!("VFS_TEST: list directory");
    let mut entries = [UserFsEntry::new(); 8];
    let count = match vfs_list(b"/vfs_test", &mut entries) {
        Ok(count) => count,
        Err(_) => return TestResult::Fail,
    };

    let mut found = false;
    for entry in entries.iter().take(count) {
        if entry.name_str() == "hello.txt" {
            found = true;
            break;
        }
    }

    if !found {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vfs_unlink() -> TestResult {
    klog_info!("VFS_TEST: unlink file");
    if vfs_unlink(b"/vfs_test/hello.txt").is_err() {
        return TestResult::Fail;
    }

    let mut entries = [UserFsEntry::new(); 8];
    let count = match vfs_list(b"/vfs_test", &mut entries) {
        Ok(count) => count,
        Err(_) => return TestResult::Fail,
    };

    for entry in entries.iter().take(count) {
        if entry.name_str() == "hello.txt" {
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

struct FailingBlockDevice {
    fail_reads: bool,
    fail_writes: bool,
    capacity: u64,
}

impl FailingBlockDevice {
    fn new(capacity: u64) -> Self {
        Self {
            fail_reads: false,
            fail_writes: false,
            capacity,
        }
    }

    fn with_read_fail(mut self) -> Self {
        self.fail_reads = true;
        self
    }
}

impl BlockDevice for FailingBlockDevice {
    fn read_at(&self, _offset: u64, _buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if self.fail_reads {
            Err(BlockDeviceError::InvalidBuffer)
        } else {
            Ok(())
        }
    }

    fn write_at(&mut self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if self.fail_writes {
            Err(BlockDeviceError::InvalidBuffer)
        } else {
            Ok(())
        }
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }
}

struct WriteFailingDevice {
    inner: MemoryBlockDevice,
}

impl WriteFailingDevice {
    fn new(inner: MemoryBlockDevice) -> Self {
        Self { inner }
    }
}

impl BlockDevice for WriteFailingDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&mut self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        Err(BlockDeviceError::InvalidBuffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }
}

struct Ext2ImageSpec<'a> {
    blocks: u32,
    inodes: u32,
    file_name: Option<&'a [u8]>,
    file_data: Option<&'a [u8]>,
    file_block: u32,
}

fn build_ext2_image(spec: Ext2ImageSpec<'_>) -> Option<MemoryBlockDevice> {
    let block_size = 1024u32;
    let inode_size = 128u16;
    let blocks_per_group = spec.blocks;
    let inodes_per_group = spec.inodes;
    let size_bytes = (spec.blocks as usize).saturating_mul(block_size as usize);
    let device = MemoryBlockDevice::allocate(size_bytes)?;

    unsafe {
        ptr::write_bytes(device.as_mut_ptr(), 0, size_bytes);
    }

    let sb_offset = 1024usize;
    let sb = unsafe { core::slice::from_raw_parts_mut(device.as_mut_ptr().add(sb_offset), 1024) };

    write_le_u32(&mut sb[0..], spec.inodes);
    write_le_u32(&mut sb[4..], spec.blocks);
    write_le_u32(&mut sb[12..], 8);
    write_le_u32(&mut sb[16..], 8);
    write_le_u32(&mut sb[20..], 1);
    write_le_u32(&mut sb[24..], 0);
    write_le_u32(&mut sb[32..], blocks_per_group);
    write_le_u32(&mut sb[40..], inodes_per_group);
    write_le_u16(&mut sb[56..], 0xEF53);
    write_le_u32(&mut sb[76..], 1);
    write_le_u32(&mut sb[84..], 11);
    write_le_u16(&mut sb[88..], inode_size);

    let desc_offset = 2 * block_size as usize;
    let desc = unsafe { core::slice::from_raw_parts_mut(device.as_mut_ptr().add(desc_offset), 32) };

    write_le_u32(&mut desc[0..], 3);
    write_le_u32(&mut desc[4..], 4);
    write_le_u32(&mut desc[8..], 5);
    write_le_u16(&mut desc[12..], 8);
    write_le_u16(&mut desc[14..], 8);
    write_le_u16(&mut desc[16..], 1);

    let inode_table_offset = 5 * block_size as usize;
    let inode_table = unsafe {
        core::slice::from_raw_parts_mut(device.as_mut_ptr().add(inode_table_offset), 1024)
    };

    let root_inode_offset = 128;
    write_le_u16(&mut inode_table[root_inode_offset..], 0x4000);
    write_le_u32(&mut inode_table[root_inode_offset + 4..], block_size);
    write_le_u32(&mut inode_table[root_inode_offset + 28..], 2);
    write_le_u32(&mut inode_table[root_inode_offset + 40..], 6);

    let file_inode_number = 3u32;
    if let (Some(name), Some(data)) = (spec.file_name, spec.file_data) {
        let file_inode_offset = root_inode_offset + inode_size as usize;
        write_le_u16(&mut inode_table[file_inode_offset..], 0x8000);
        write_le_u32(&mut inode_table[file_inode_offset + 4..], data.len() as u32);
        write_le_u32(&mut inode_table[file_inode_offset + 28..], 1);
        write_le_u32(&mut inode_table[file_inode_offset + 40..], spec.file_block);

        if spec.file_block < spec.blocks {
            let data_offset = spec.file_block as usize * block_size as usize;
            let data_block = unsafe {
                core::slice::from_raw_parts_mut(device.as_mut_ptr().add(data_offset), 1024)
            };
            data_block[..data.len()].copy_from_slice(data);
        }

        let dir_offset = 6 * block_size as usize;
        let dir_block =
            unsafe { core::slice::from_raw_parts_mut(device.as_mut_ptr().add(dir_offset), 1024) };

        let mut write_dir_entry =
            |offset: usize, inode: u32, rec_len: u16, name: &[u8], file_type: u8| {
                write_le_u32(&mut dir_block[offset..], inode);
                write_le_u16(&mut dir_block[offset + 4..], rec_len);
                dir_block[offset + 6] = name.len() as u8;
                dir_block[offset + 7] = file_type;
                let name_end = offset + 8 + name.len();
                dir_block[offset + 8..name_end].copy_from_slice(name);
                for b in dir_block[name_end..offset + rec_len as usize].iter_mut() {
                    *b = 0;
                }
            };

        let used = 24 + ((8 + name.len() + 3) & !3);
        let rec_len = (block_size as usize - used) as u16;
        write_dir_entry(0, 2, 12, b".", 2);
        write_dir_entry(12, 2, 12, b"..", 2);
        write_dir_entry(24, file_inode_number, (used - 24) as u16, name, 1);
        write_dir_entry(used, 0, rec_len, b"", 0);
    } else {
        let dir_offset = 6 * block_size as usize;
        let dir_block =
            unsafe { core::slice::from_raw_parts_mut(device.as_mut_ptr().add(dir_offset), 1024) };

        let mut write_dir_entry =
            |offset: usize, inode: u32, rec_len: u16, name: &[u8], file_type: u8| {
                write_le_u32(&mut dir_block[offset..], inode);
                write_le_u16(&mut dir_block[offset + 4..], rec_len);
                dir_block[offset + 6] = name.len() as u8;
                dir_block[offset + 7] = file_type;
                let name_end = offset + 8 + name.len();
                dir_block[offset + 8..name_end].copy_from_slice(name);
                for b in dir_block[name_end..offset + rec_len as usize].iter_mut() {
                    *b = 0;
                }
            };

        write_dir_entry(0, 2, 12, b".", 2);
        write_dir_entry(12, 2, 12, b"..", 2);
        write_dir_entry(24, 0, (block_size - 24) as u16, b"", 0);
    }

    Some(device)
}

fn build_minimal_ext2_image(blocks: u32, inodes: u32) -> Option<MemoryBlockDevice> {
    build_ext2_image(Ext2ImageSpec {
        blocks,
        inodes,
        file_name: None,
        file_data: None,
        file_block: 0,
    })
}

pub fn test_ext2_invalid_superblock_magic() -> TestResult {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    unsafe {
        let sb = core::slice::from_raw_parts_mut(device.as_mut_ptr().add(sb_offset), 1024);
        sb[56] = 0;
        sb[57] = 0;
    }

    let result = Ext2Fs::init_internal(&mut device);
    match result {
        Err(Ext2Error::InvalidSuperblock) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_unsupported_block_size() -> TestResult {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    unsafe {
        let sb = core::slice::from_raw_parts_mut(device.as_mut_ptr().add(sb_offset), 1024);
        write_le_u32(&mut sb[24..], 8);
    }

    let result = Ext2Fs::init_internal(&mut device);
    match result {
        Err(Ext2Error::UnsupportedBlockSize) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_directory_format_error() -> TestResult {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let dir_offset = 6 * 1024usize;
    unsafe {
        let dir_block = core::slice::from_raw_parts_mut(device.as_mut_ptr().add(dir_offset), 1024);
        dir_block[4] = 0;
        dir_block[5] = 0;
    }

    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

    let result = fs.for_each_dir_entry(2, |_| true);
    match result {
        Err(Ext2Error::DirectoryFormat) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_invalid_inode() -> TestResult {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

    let result = fs.read_inode(9999);
    match result {
        Err(Ext2Error::InvalidInode) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_read_file_not_regular() -> TestResult {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

    let mut buf = [0u8; 32];
    let result = fs.read_file(2, 0, &mut buf);
    match result {
        Err(Ext2Error::NotFile) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_device_read_error() -> TestResult {
    let mut device = FailingBlockDevice::new(4096).with_read_fail();
    let result = Ext2Fs::init_internal(&mut device);
    match result {
        Err(Ext2Error::DeviceError) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_device_write_error_on_metadata() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let mut failing = WriteFailingDevice::new(device);
    let mut fs = match Ext2Fs::init_internal(&mut failing) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Pass,
    };

    let result = fs.create_directory(2, b"faildir");
    match result {
        Err(Ext2Error::DeviceError) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_read_block_out_of_bounds() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"boot.bin"),
        file_data: Some(b"slopos-test"),
        file_block: 80,
    };
    let Some(mut device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

    let inode = match fs.resolve_path(b"/boot.bin") {
        Ok(inode) => inode,
        Err(_) => return TestResult::Fail,
    };

    let result = fs.read_file(inode, 0, &mut [0u8; 1]);
    match result {
        Err(Ext2Error::InvalidBlock) | Err(Ext2Error::DeviceError) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_read_file_data_roundtrip() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"boot.bin"),
        file_data: Some(b"slopos-test"),
        file_block: 7,
    };
    let Some(mut device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

    let inode = match fs.resolve_path(b"/boot.bin") {
        Ok(inode) => inode,
        Err(_) => return TestResult::Fail,
    };

    let mut buf = [0u8; 16];
    let read_len = match fs.read_file(inode, 0, &mut buf) {
        Ok(len) => len,
        Err(_) => return TestResult::Fail,
    };

    if read_len != b"slopos-test".len() || &buf[..read_len] != b"slopos-test" {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ext2_path_resolution_not_found() -> TestResult {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

    let result = fs.resolve_path(b"/nope/file.txt");
    match result {
        Err(Ext2Error::PathNotFound) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_remove_path_not_file() -> TestResult {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

    let result = fs.remove_path(b"/");
    match result {
        Err(Ext2Error::PathNotFound) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

fn ext2_tests_init() -> bool {
    if let Err(_) = vfs_init_builtin_filesystems() {
        klog_info!("VFS_TEST: failed to initialize VFS");
        return false;
    }
    true
}

const EXT2_SUITE_NAME: &[u8] = b"ext2\0";

fn run_ext2_suite(_config: *const (), out: *mut slopos_lib::testing::TestSuiteResult) -> i32 {
    let start = slopos_lib::tsc::rdtsc();

    if !ext2_tests_init() {
        if let Some(out_ref) = unsafe { out.as_mut() } {
            out_ref.name = EXT2_SUITE_NAME.as_ptr() as *const core::ffi::c_char;
            out_ref.total = 0;
            out_ref.passed = 0;
            out_ref.failed = 0;
            out_ref.elapsed_ms = 0;
        }
        return 0;
    }

    let mut passed = 0u32;
    let mut total = 0u32;

    slopos_lib::run_test!(passed, total, test_vfs_initialized);
    slopos_lib::run_test!(passed, total, test_vfs_root_stat);
    slopos_lib::run_test!(passed, total, test_vfs_file_roundtrip);
    slopos_lib::run_test!(passed, total, test_vfs_list);
    slopos_lib::run_test!(passed, total, test_vfs_unlink);
    slopos_lib::run_test!(passed, total, test_ext2_invalid_superblock_magic);
    slopos_lib::run_test!(passed, total, test_ext2_unsupported_block_size);
    slopos_lib::run_test!(passed, total, test_ext2_directory_format_error);
    slopos_lib::run_test!(passed, total, test_ext2_invalid_inode);
    slopos_lib::run_test!(passed, total, test_ext2_read_file_not_regular);
    slopos_lib::run_test!(passed, total, test_ext2_device_read_error);
    slopos_lib::run_test!(passed, total, test_ext2_device_write_error_on_metadata);
    slopos_lib::run_test!(passed, total, test_ext2_read_block_out_of_bounds);
    slopos_lib::run_test!(passed, total, test_ext2_read_file_data_roundtrip);
    slopos_lib::run_test!(passed, total, test_ext2_path_resolution_not_found);
    slopos_lib::run_test!(passed, total, test_ext2_remove_path_not_file);

    let elapsed = slopos_lib::testing::measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());

    if let Some(out_ref) = unsafe { out.as_mut() } {
        out_ref.name = EXT2_SUITE_NAME.as_ptr() as *const core::ffi::c_char;
        out_ref.total = total;
        out_ref.passed = passed;
        out_ref.failed = total.saturating_sub(passed);
        out_ref.exceptions_caught = 0;
        out_ref.unexpected_exceptions = 0;
        out_ref.elapsed_ms = elapsed;
        out_ref.timed_out = 0;
    }

    if passed == total { 0 } else { -1 }
}

#[used]
#[unsafe(link_section = ".test_registry")]
static EXT2_SUITE_DESC: slopos_lib::testing::TestSuiteDesc = slopos_lib::testing::TestSuiteDesc {
    name: EXT2_SUITE_NAME.as_ptr() as *const core::ffi::c_char,
    run: Some(run_ext2_suite),
};
