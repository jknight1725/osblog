// block.rs
// Block device using VirtIO protocol
// Stephen Marz
// 10 March 2020

use crate::{page::{zalloc, PAGE_SIZE},
			kmem::{kmalloc, kfree},
            virtio::{MmioOffsets, Queue, VIRTIO_RING_SIZE, StatusField, Descriptor}};
use alloc::collections::VecDeque;
use core::mem::size_of;
use crate::virtio;

#[repr(C)]
pub struct Geometry {
	cylinders: u16,
	heads:     u8,
	sectors:   u8,
}

#[repr(C)]
pub struct Topology {
	physical_block_exp: u8,
	alignment_offset:   u8,
	min_io_size:        u16,
	opt_io_size:        u32,
}

// There is a configuration space for VirtIO that begins
// at offset 0x100 and continues to the size of the configuration.
// The structure below represents the configuration for a 
// block device. Really, all that this OS cares about is the
// capacity.
#[repr(C)]
pub struct Config {
	capacity:                 u64,
	size_max:                 u32,
	seg_max:                  u32,
	geometry:                 Geometry,
	blk_size:                 u32,
	topology:                 Topology,
	writeback:                u8,
	unused0:                  [u8; 3],
	max_discard_sector:       u32,
	max_discard_seg:          u32,
	discard_sector_alignment: u32,
	max_write_zeroes_sectors: u32,
	max_write_zeroes_seg:     u32,
	write_zeroes_may_unmap:   u8,
	unused1:                  [u8; 3],
}

#[repr(C)]
pub struct Header {
	blktype: u32,
	reserved: u32,
	sector: u64,
}

#[repr(C)]
pub struct Data {
	data: *mut u8
}

#[repr(C)]
pub struct Status {
	status: u8
}

#[repr(C)]
pub struct Request {
	header: Header,
	data: Data,
	status: Status,
	head: u16,
}

// Internal block device structure
pub struct BlockDevice {
    queue: *mut Queue,
    dev: *mut u32,
	idx:   u16,
}

// Type values
pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

// Status values
pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// Feature bits
pub const VIRTIO_BLK_F_SIZE_MAX: u32 = 1;
pub const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
pub const VIRTIO_BLK_F_GEOMETRY: u32 = 4;
pub const VIRTIO_BLK_F_RO: u32 = 5;
pub const VIRTIO_BLK_F_BLK_SIZE: u32 = 6;
pub const VIRTIO_BLK_F_FLUSH: u32 = 9;
pub const VIRTIO_BLK_F_TOPOLOGY: u32 = 10;
pub const VIRTIO_BLK_F_CONFIG_WCE: u32 = 11;
pub const VIRTIO_BLK_F_DISCARD: u32 = 13;
pub const VIRTIO_BLK_F_WRITE_ZEROES: u32 = 14;


// Much like with processes, Rust requires some initialization
// when we declare a static. In this case, we use the Option
// value type to signal that the variable exists, but not the
// queue itself. We will replace this with an actual queue when
// we initialize the block system.
static mut BLOCK_DEVICES: Option<VecDeque<BlockDevice>> = None;

pub fn init() {
	unsafe {
		BLOCK_DEVICES.replace(VecDeque::with_capacity(1));
	}
}

pub fn setup_block_device(ptr: *mut u32) -> bool {
	unsafe {
		if let Some(mut vdq) = BLOCK_DEVICES.take() {
			// [Driver] Device Initialization
			// 1. Reset the device (write 0 into status)
			ptr.add(MmioOffsets::Status.scale32()).write_volatile(0);
			let mut status_bits = StatusField::Acknowledge.val32();
			// 2. Set ACKNOWLEDGE status bit
			ptr.add(MmioOffsets::Status.scale32()).write_volatile(status_bits);
			// 3. Set the DRIVER status bit
			status_bits |= StatusField::DriverOk.val32();
			ptr.add(MmioOffsets::Status.scale32()).write_volatile(status_bits);
			// 4. Read device feature bits, write subset of feature bits understood by OS and driver
			//    to the device.
			let host_features = ptr.add(MmioOffsets::HostFeatures.scale32()).read_volatile();
			let guest_features = host_features & !(1 << VIRTIO_BLK_F_RO);
			ptr.add(MmioOffsets::GuestFeatures.scale32()).write_volatile(guest_features);
			// 5. Set the FEATURES_OK status bit
			status_bits |= StatusField::FeaturesOk.val32();
			ptr.add(MmioOffsets::Status.scale32()).write_volatile(status_bits);
			// 6. Re-read status to ensure FEATURES_OK is still set. Otherwise, it doesn't support our features.
			let status_ok = ptr.add(MmioOffsets::Status.scale32()).read_volatile();
			// If the status field no longer has features_ok set, that means that the device couldn't accept
			// the features that we request. Therefore, this is considered a "failed" state.
			if false == StatusField::features_ok(status_ok) {
				print!("features fail...");
				ptr.add(MmioOffsets::Status.scale32()).write_volatile(StatusField::Failed.val32());
				return false;
			}
			// 7. Perform device-specific setup.
			// Set the queue num. We have to make sure that the queue size is valid
			// because the device can only take a certain size.
			let qnmax = ptr.add(MmioOffsets::QueueNumMax.scale32()).read_volatile();
			ptr.add(MmioOffsets::QueueNum.scale32()).write_volatile(VIRTIO_RING_SIZE as u32);
			if VIRTIO_RING_SIZE as u32 > qnmax {
				print!("queue size fail...");
				return false;
			}
			// First, if the block device array is empty, create it!
			// We add 4095 to round this up and then do an integer divide
			// to truncate the decimal. We don't add 4096, because if it is
			// exactly 4096 bytes, we would get two pages, not one.
			let num_pages =
				(size_of::<Queue>() + PAGE_SIZE - 1) / PAGE_SIZE;
			// println!("np = {}", num_pages);
			// We allocate a page for each device. This will the the descriptor
			// where we can communicate with the block device. We will still use
			// an MMIO register (in particular, QueueNotify) to actually tell
			// the device we put something in memory.
			// We also have to be careful with memory ordering. We don't want to
			// issue a notify before all memory writes have finished. We will
			// look at that later, but we need what is called a memory "fence"
			// or barrier.
			let queue_ptr = zalloc(num_pages) as *mut Queue;
			let queue_pfn = queue_ptr as u32;
			// QueuePFN is a physical page number, however it appears for QEMU
			// we have to write the entire memory address. This is a physical
			// memory address where we (the OS) and the block device have
			// in common for making and receiving requests.
			ptr.add(MmioOffsets::QueuePfn.scale32())
			   .write_volatile(queue_pfn);
			// We need to store all of this data as a "BlockDevice" structure
			// We will be referring to this structure when making block requests
			// AND when handling responses.
            let bd = BlockDevice { queue: queue_ptr,
                                   dev: ptr,
			                       idx:   1, };
			vdq.push_back(bd);

			// Update the global block device array.
			BLOCK_DEVICES.replace(vdq);

			// 8. Set the DRIVER_OK status bit. Device is now "live"
			status_bits |= StatusField::DriverOk.val32();
			ptr.add(MmioOffsets::Status.scale32()).write_volatile(status_bits);
			true
		} /* if let Some(mut vdq) = BLOCK_DEVICES.take() */
		else { 
			// If we get here, the block devices array couldn't be taken. This can
			// be due to duplicate access or that init wasn't called before this setup.
			false
		}
	}
}

pub fn fill_next_descriptor(bd: &mut BlockDevice, desc: Descriptor) -> u16 {
	unsafe {
		bd.idx = (bd.idx + 1) % VIRTIO_RING_SIZE as u16;
		println!("idx = {}", bd.idx);
		 (*bd.queue).desc[bd.idx as usize] = desc;
		if (*bd.queue).desc[bd.idx as usize].flags & virtio::VIRTIO_DESC_F_NEXT != 0 {
			(*bd.queue).desc[bd.idx as usize].next = (bd.idx + 1) % VIRTIO_RING_SIZE as u16;
		}
		bd.idx
	}
}


pub fn read(dev: usize, buffer: *mut u8, size: u32, offset: usize) {
	unsafe {
		if let Some(mut bdev_alloc) = BLOCK_DEVICES.take() {
			let bdev = bdev_alloc.get_mut(dev).unwrap();
			let sector = offset / 512;
			let blk_request_size = size_of::<Request>();
			let blk_request = kmalloc(blk_request_size) as *mut Request;
			let desc = Descriptor {
				addr: &(*blk_request).header as *const Header as u64,
				len: blk_request_size as u32,
				flags: virtio::VIRTIO_DESC_F_NEXT,
				next: 0,
			};
			let head_idx = fill_next_descriptor(bdev, desc);
			(*blk_request).header.sector = sector as u64;
			(*blk_request).header.blktype = VIRTIO_BLK_T_IN;
			(*blk_request).data.data = buffer;
			let desc = Descriptor {
				addr: buffer as u64,
				len: size,
				flags: virtio::VIRTIO_DESC_F_NEXT | virtio::VIRTIO_DESC_F_WRITE,
				next: 0,
			};
			let data_idx = fill_next_descriptor(bdev, desc);
			let desc = Descriptor {
				addr: &(*blk_request).status as *const Status as u64,
				len: size_of::<Status>() as u32,
				flags: virtio::VIRTIO_DESC_F_WRITE,
				next: 0,
			};
			let status_idx = fill_next_descriptor(bdev, desc);
			(*bdev.queue).avail.ring[(*bdev.queue).avail.idx as usize] = head_idx;
			println!("Avail at {}, set head to {}", (*bdev.queue).avail.idx, head_idx);
			(*bdev.queue).avail.idx = ((*bdev.queue).avail.idx + 1) % virtio::VIRTIO_RING_SIZE as u16;
			bdev.dev.add(MmioOffsets::QueueNotify.scale32()).write_volatile(0);
		}
	}
}
pub fn write(dev: usize, buffer: *const u8, size: usize, offset: usize) {
	unsafe {
		if let Some(mut bdev_alloc) = BLOCK_DEVICES.take() {
			let bdev = bdev_alloc.get_mut(dev).unwrap();
			let sector = offset / 512;
			let desc = Descriptor {
				addr: 0,
				len: 0,
				flags: 0,
				next: 0,
			};
			let head_idx = fill_next_descriptor(bdev, desc);
			let desc = Descriptor {
				addr: 0,
				len: 0,
				flags: 0,
				next: 0,
			};
			let data_idx = fill_next_descriptor(bdev, desc);
			let desc = Descriptor {
				addr: 0,
				len: 0,
				flags: 0,
				next: 0,
			};
			let status_idx = fill_next_descriptor(bdev, desc);
		}
	}
}