//! This file contains the entry point to the Hypervisor. The Uhyve utilizes KVM to
//! create a Virtual Machine and load the kernel.

use consts::*;
use debug_manager::DebugManager;
use error::*;
use kvm_bindings::*;
use kvm_ioctls::VmFd;
use libc::EFD_NONBLOCK;
use linux::vcpu::*;
use linux::virtio::*;
use linux::{MemoryRegion, KVM};
use nix::sys::mman::*;
use shared_queue::*;
use std;
use std::convert::TryInto;
use std::mem;
use std::net::Ipv4Addr;
use std::os::raw::c_void;
use std::ptr;
use std::ptr::{read_volatile, write_volatile};
use std::str::FromStr;
use std::sync::atomic::{spin_loop_hint, AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread;
use tun_tap::{Iface, Mode};
use vm::{BootInfo, VirtualCPU, Vm, VmParameter};
use vmm_sys_util::eventfd::EventFd;

const KVM_32BIT_MAX_MEM_SIZE: usize = 1 << 32;
const KVM_32BIT_GAP_SIZE: usize = 768 << 20;
const KVM_32BIT_GAP_START: usize = KVM_32BIT_MAX_MEM_SIZE - KVM_32BIT_GAP_SIZE;

struct UhyveNetwork {
	reader: std::thread::JoinHandle<()>,
	writer: std::thread::JoinHandle<()>,
	tx: std::sync::mpsc::Sender<usize>,
}

impl UhyveNetwork {
	pub fn new(eventfd: EventFd, name: String, start: usize) -> Self {
		let iface = Arc::new(
			Iface::without_packet_info(&name.to_owned(), Mode::Tap)
				.expect("Unable to creat TUN/TAP device"),
		);

		let iface_writer = Arc::clone(&iface);
		let iface_reader = Arc::clone(&iface);
		let (tx, rx) = channel::<usize>();

		let writer = thread::spawn(move || {
			let tx_queue = unsafe {
				&mut *((start + align_up!(mem::size_of::<SharedQueue>(), 64)) as *mut u8
					as *mut SharedQueue)
			};
			tx_queue.init();

			loop {
				let _value = rx.recv().expect("Failed to read from channel");

				loop {
					let written = tx_queue.written.load(Ordering::SeqCst);
					let read = tx_queue.read.load(Ordering::SeqCst);
					let distance = written - read;

					if distance > 0 {
						let idx = read % UHYVE_QUEUE_SIZE;
						let len = unsafe { read_volatile(&tx_queue.inner[idx].len) } as usize;
						let _ = iface_writer
							.send(&mut tx_queue.inner[idx].data[0..len])
							.expect("Send on TUN/TAP device failed!");
						tx_queue.read.fetch_add(1, Ordering::SeqCst);
					} else {
						break;
					}
				}
			}
		});

		let reader = thread::spawn(move || {
			let rx_queue = unsafe { &mut *(start as *mut u8 as *mut SharedQueue) };
			rx_queue.init();

			loop {
				let written = rx_queue.written.load(Ordering::SeqCst);
				let read = rx_queue.read.load(Ordering::SeqCst);
				let distance = written - read;

				if distance < UHYVE_QUEUE_SIZE {
					let idx = written % UHYVE_QUEUE_SIZE;
					unsafe {
						write_volatile(
							&mut rx_queue.inner[idx].len,
							iface_reader
								.recv(&mut rx_queue.inner[idx].data)
								.expect("Receive on TUN/TAP device failed!")
								.try_into()
								.unwrap(),
						);
					}
					rx_queue.written.fetch_add(1, Ordering::SeqCst);

					if written == rx_queue.read.load(Ordering::SeqCst) {
						//debug!("Trigger Interrupt");
						static IRQ_COUNTER: AtomicU64 = AtomicU64::new(0);
						eventfd
							.write(IRQ_COUNTER.fetch_add(1, Ordering::SeqCst))
							.expect("Unable to trigger interrupt");
					}
				} else {
					spin_loop_hint();
				}
			}
		});

		UhyveNetwork {
			reader: reader,
			writer: writer,
			tx: tx.clone(),
		}
	}
}

impl Drop for UhyveNetwork {
	fn drop(&mut self) {
		debug!("Dropping network interface!");
	}
}

pub struct Uhyve {
	vm: VmFd,
	entry_point: u64,
	mem: MmapMemory,
	num_cpus: u32,
	path: String,
	boot_info: *const BootInfo,
	verbose: bool,
	ip: Option<Ipv4Addr>,
	gateway: Option<Ipv4Addr>,
	mask: Option<Ipv4Addr>,
	uhyve_device: Option<UhyveNetwork>,
	virtio_device: Arc<Mutex<VirtioNetPciDevice>>,
	dbg: Option<Arc<Mutex<DebugManager>>>,
}

impl Uhyve {
	pub fn new(
		kernel_path: String,
		specs: &VmParameter,
		dbg: Option<DebugManager>,
	) -> Result<Uhyve> {
		// parse string to get IP address
		let ip_addr = match &specs.ip {
			Some(addr_str) => {
				Some(Ipv4Addr::from_str(addr_str).expect("Unable to parse ip address"))
			}
			_ => None,
		};

		// parse string to get gateway address
		let gw_addr = match &specs.gateway {
			Some(addr_str) => {
				Some(Ipv4Addr::from_str(addr_str).expect("Unable to parse gateway address"))
			}
			_ => None,
		};

		// parse string to get gateway address
		let mask = match &specs.mask {
			Some(addr_str) => {
				Some(Ipv4Addr::from_str(addr_str).expect("Unable to parse network parse"))
			}
			_ => None,
		};

		let vm = KVM.create_vm().or_else(to_error)?;

		let mem = MmapMemory::new(0, specs.mem_size, 0, specs.hugepage, specs.mergeable);

		let sz = if specs.mem_size < KVM_32BIT_GAP_START {
			specs.mem_size
		} else {
			KVM_32BIT_GAP_START
		};

		// create virtio interface
		let virtio_device = Arc::new(Mutex::new(VirtioNetPciDevice::new()));

		let kvm_mem = kvm_userspace_memory_region {
			slot: 0,
			flags: mem.flags(),
			memory_size: sz as u64,
			guest_phys_addr: mem.guest_address() as u64,
			userspace_addr: mem.host_address() as u64,
		};

		unsafe { vm.set_user_memory_region(kvm_mem) }.or_else(to_error)?;

		if specs.mem_size > KVM_32BIT_GAP_START + KVM_32BIT_GAP_SIZE {
			let kvm_mem = kvm_userspace_memory_region {
				slot: 1,
				flags: mem.flags(),
				memory_size: (specs.mem_size - KVM_32BIT_GAP_START - KVM_32BIT_GAP_SIZE) as u64,
				guest_phys_addr: (mem.guest_address() + KVM_32BIT_GAP_START + KVM_32BIT_GAP_SIZE)
					as u64,
				userspace_addr: (mem.host_address() + KVM_32BIT_GAP_START + KVM_32BIT_GAP_SIZE)
					as u64,
			};

			unsafe { vm.set_user_memory_region(kvm_mem) }.or_else(to_error)?;
		}

		debug!("Initialize interrupt controller");

		// create basic interrupt controller
		vm.create_irq_chip().or_else(to_error)?;

		// enable x2APIC support
		let mut cap: kvm_enable_cap = Default::default();
		cap.cap = KVM_CAP_X2APIC_API;
		cap.flags = 0;
		cap.args[0] =
			(KVM_X2APIC_API_USE_32BIT_IDS | KVM_X2APIC_API_DISABLE_BROADCAST_QUIRK) as u64;
		vm.enable_cap(&cap)
			.expect("Unable to enable x2apic support");

		// initialize IOAPIC with HermitCore's default settings
		let mut irqchip = kvm_irqchip::default();
		irqchip.chip_id = KVM_IRQCHIP_IOAPIC;
		vm.get_irqchip(&mut irqchip)
			.expect("Unable to determine IOAPIC");
		unsafe {
			for (i, redir) in irqchip.chip.ioapic.redirtbl.iter_mut().enumerate() {
				redir.fields.vector = (0x20 + i).try_into().unwrap();
				redir.fields.set_delivery_mode(0);
				redir.fields.set_dest_mode(0);
				redir.fields.set_delivery_status(0);
				redir.fields.set_polarity(0);
				redir.fields.set_remote_irr(0);
				redir.fields.set_trig_mode(0);
				if i != 2 {
					redir.fields.set_mask(0);
				} else {
					redir.fields.set_mask(1);
				}
				redir.fields.dest_id = 0;
			}
		}
		vm.set_irqchip(&irqchip)
			.expect("Unable to configure IOAPIC");

		// currently, we support only system, which provides the
		// cpu feature TSC_DEADLINE
		let mut cap: kvm_enable_cap = Default::default();
		cap.cap = KVM_CAP_TSC_DEADLINE_TIMER;
		cap.args[0] = 0;
		if vm.enable_cap(&cap).is_ok() {
			panic!("Processor feature \"tsc deadline\" isn't supported!")
		}

		let mut cap: kvm_enable_cap = Default::default();
		cap.cap = KVM_CAP_IRQFD;
		if vm.enable_cap(&cap).is_ok() {
			panic!("The support of KVM_CAP_IRQFD is curently required");
		}

		let evtfd = EventFd::new(EFD_NONBLOCK).unwrap();
		vm.register_irqfd(&evtfd, UHYVE_IRQ_NET).or_else(to_error)?;
		// create TUN/TAP device
		let uhyve_device = match &specs.nic {
			Some(nic) => {
				debug!("Intialize network interface");
				Some(UhyveNetwork::new(
					evtfd,
					nic.to_owned().to_string(),
					mem.host_address() + SHAREDQUEUE_START,
				))
			}
			_ => None,
		};

		let hyve = Uhyve {
			vm: vm,
			entry_point: 0,
			mem: mem,
			num_cpus: specs.num_cpus,
			path: kernel_path,
			boot_info: ptr::null(),
			verbose: specs.verbose,
			ip: ip_addr,
			gateway: gw_addr,
			mask: mask,
			uhyve_device: uhyve_device,
			virtio_device: virtio_device.clone(),
			dbg: dbg.map(|g| Arc::new(Mutex::new(g))),
		};

		hyve.init_guest_mem();

		Ok(hyve)
	}
}

impl Vm for Uhyve {
	fn verbose(&self) -> bool {
		self.verbose
	}

	fn set_entry_point(&mut self, entry: u64) {
		self.entry_point = entry;
	}

	fn get_entry_point(&self) -> u64 {
		self.entry_point
	}

	fn get_ip(&self) -> Option<Ipv4Addr> {
		self.ip
	}

	fn get_gateway(&self) -> Option<Ipv4Addr> {
		self.gateway
	}

	fn get_mask(&self) -> Option<Ipv4Addr> {
		self.mask
	}

	fn num_cpus(&self) -> u32 {
		self.num_cpus
	}

	fn guest_mem(&self) -> (*mut u8, usize) {
		(self.mem.host_address() as *mut u8, self.mem.memory_size())
	}

	fn kernel_path(&self) -> &str {
		&self.path
	}

	fn create_cpu(&self, id: u32) -> Result<Box<dyn VirtualCPU>> {
		let vm_start = self.mem.host_address() as usize;
		let tx = match &self.uhyve_device {
			Some(dev) => Some(dev.tx.clone()),
			_ => None,
		};

		Ok(Box::new(UhyveCPU::new(
			id,
			self.path.clone(),
			self.vm
				.create_vcpu(id.try_into().unwrap())
				.or_else(to_error)?,
			vm_start,
			tx,
			self.virtio_device.clone(),
			self.dbg.as_ref().cloned(),
		)))
	}

	fn set_boot_info(&mut self, header: *const BootInfo) {
		self.boot_info = header;
	}

	fn cpu_online(&self) -> u32 {
		if self.boot_info.is_null() {
			0
		} else {
			unsafe { read_volatile(&(*self.boot_info).cpu_online) }
		}
	}
}

impl Drop for Uhyve {
	fn drop(&mut self) {
		debug!("Drop virtual machine");
	}
}

unsafe impl Send for Uhyve {}
unsafe impl Sync for Uhyve {}

#[derive(Debug)]
struct MmapMemory {
	flags: u32,
	memory_size: usize,
	guest_address: usize,
	host_address: usize,
}

impl MmapMemory {
	pub fn new(
		flags: u32,
		memory_size: usize,
		guest_address: u64,
		huge_pages: bool,
		mergeable: bool,
	) -> MmapMemory {
		let host_address = unsafe {
			mmap(
				std::ptr::null_mut(),
				memory_size,
				ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
				MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_NORESERVE,
				-1,
				0,
			)
			.expect("mmap failed")
		};

		if mergeable {
			debug!("Enable kernel feature to merge same pages");
			unsafe {
				if madvise(host_address, memory_size, MmapAdvise::MADV_MERGEABLE).is_err() {
					panic!("madvise failed");
				}
			}
		}

		if huge_pages {
			debug!("Uhyve uses huge pages");
			unsafe {
				if madvise(host_address, memory_size, MmapAdvise::MADV_HUGEPAGE).is_err() {
					panic!("madvise failed");
				}
			}
		}

		MmapMemory {
			flags: flags,
			memory_size: memory_size,
			guest_address: guest_address as usize,
			host_address: host_address as usize,
		}
	}

	#[allow(dead_code)]
	fn as_slice_mut(&mut self) -> &mut [u8] {
		unsafe { std::slice::from_raw_parts_mut(self.host_address as *mut u8, self.memory_size) }
	}
}

impl MemoryRegion for MmapMemory {
	fn flags(&self) -> u32 {
		self.flags
	}

	fn memory_size(&self) -> usize {
		self.memory_size
	}

	fn guest_address(&self) -> usize {
		self.guest_address
	}

	fn host_address(&self) -> usize {
		self.host_address
	}
}

impl Drop for MmapMemory {
	fn drop(&mut self) {
		if self.memory_size() > 0 {
			unsafe {
				if munmap(self.host_address() as *mut c_void, self.memory_size()).is_err() {
					panic!("munmap failed");
				}
			}
		}
	}
}
