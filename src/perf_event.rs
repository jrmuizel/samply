use crate::perf_event_raw::{
    PERF_FORMAT_GROUP, PERF_FORMAT_ID, PERF_FORMAT_TOTAL_TIME_ENABLED,
    PERF_FORMAT_TOTAL_TIME_RUNNING, PERF_RECORD_COMM, PERF_RECORD_EXIT, PERF_RECORD_FORK,
    PERF_RECORD_LOST, PERF_RECORD_MISC_COMM_EXEC, PERF_RECORD_MISC_CPUMODE_MASK,
    PERF_RECORD_MISC_GUEST_KERNEL, PERF_RECORD_MISC_GUEST_USER, PERF_RECORD_MISC_KERNEL,
    PERF_RECORD_MISC_MMAP_BUILD_ID, PERF_RECORD_MISC_MMAP_DATA, PERF_RECORD_MISC_SWITCH_OUT,
    PERF_RECORD_MISC_SWITCH_OUT_PREEMPT, PERF_RECORD_MISC_USER, PERF_RECORD_MMAP,
    PERF_RECORD_MMAP2, PERF_RECORD_SAMPLE, PERF_RECORD_SWITCH, PERF_RECORD_THROTTLE,
    PERF_RECORD_UNTHROTTLE, PERF_SAMPLE_ADDR, PERF_SAMPLE_AUX, PERF_SAMPLE_BRANCH_HW_INDEX,
    PERF_SAMPLE_BRANCH_STACK, PERF_SAMPLE_CALLCHAIN, PERF_SAMPLE_CODE_PAGE_SIZE, PERF_SAMPLE_CPU,
    PERF_SAMPLE_DATA_PAGE_SIZE, PERF_SAMPLE_DATA_SRC, PERF_SAMPLE_ID, PERF_SAMPLE_IDENTIFIER,
    PERF_SAMPLE_IP, PERF_SAMPLE_PERIOD, PERF_SAMPLE_PHYS_ADDR, PERF_SAMPLE_RAW, PERF_SAMPLE_READ,
    PERF_SAMPLE_REGS_INTR, PERF_SAMPLE_REGS_USER, PERF_SAMPLE_STACK_USER, PERF_SAMPLE_STREAM_ID,
    PERF_SAMPLE_TID, PERF_SAMPLE_TIME, PERF_SAMPLE_TRANSACTION, PERF_SAMPLE_WEIGHT,
};
use crate::raw_data::{RawData, RawRegs};
use crate::utils::{HexSlice, HexValue};
use byteorder::{ByteOrder, ReadBytesExt};
use std::{fmt, io::Cursor};

pub struct RawEvent<'a> {
    pub kind: u32,
    pub misc: u16,
    pub data: RawData<'a>,
}

pub struct SampleEvent<'a> {
    pub timestamp: Option<u64>,
    pub pid: Option<i32>,
    pub tid: Option<i32>,
    pub cpu: Option<u32>,
    pub period: Option<u64>,
    pub regs: Option<Regs<'a>>,
    pub dynamic_stack_size: u64,
    pub stack: RawData<'a>,
    pub callchain: Option<Vec<u64>>,
}

#[derive(Debug)]
pub struct Regs<'a> {
    regs_mask: u64,
    raw_regs: RawRegs<'a>,
}

impl<'a> Regs<'a> {
    pub fn new(regs_mask: u64, raw_regs: RawRegs<'a>) -> Self {
        Self {
            regs_mask,
            raw_regs,
        }
    }

    pub fn get(&self, register: u64) -> Option<u64> {
        if self.regs_mask & (1 << register) == 0 {
            return None;
        }

        let mut index = 0;
        for i in 0..register {
            if self.regs_mask & (1 << i) != 0 {
                index += 1;
            }
        }
        Some(self.raw_regs.get(index))
    }
}

#[derive(Debug)]
pub struct ProcessEvent {
    pub pid: i32,
    pub ppid: i32,
    pub tid: i32,
    pub ptid: i32,
    pub timestamp: u64,
}

pub struct CommEvent {
    pub pid: i32,
    pub tid: i32,
    pub name: Vec<u8>,
    pub is_execve: bool,
}

/// These aren't emitted by the kernel any more - the kernel uses MMAP2 events
/// these days.
/// However, `perf record` still emits synthetic MMAP events (not MMAP2!) for
/// the kernel image. So if you want to symbolicate kernel addresses you still
/// need to process these.
/// The kernel image MMAP events have pid -1.
pub struct MmapEvent {
    pub pid: i32,
    pub tid: i32,
    pub address: u64,
    pub length: u64,
    pub page_offset: u64,
    pub is_executable: bool,
    pub dso_key: Option<DsoKey>,
    pub path: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DsoKey {
    Kernel,
    GuestKernel,
    Vdso32,
    VdsoX32,
    Vdso64,
    Vsyscall,
    KernelModule(String),
    User(String, Vec<u8>),
}

impl DsoKey {
    pub fn detect(path: &[u8], misc: u16) -> Option<Self> {
        if path == b"//anon" || path == b"[stack]" || path == b"[heap]" || path == b"[vvar]" {
            return None;
        }

        let cpumode = misc & PERF_RECORD_MISC_CPUMODE_MASK;
        if path.starts_with(b"[kernel.kallsyms]") {
            let dso_key = if cpumode == PERF_RECORD_MISC_GUEST_KERNEL {
                DsoKey::GuestKernel
            } else {
                DsoKey::Kernel
            };
            return Some(dso_key);
        }
        if path.starts_with(b"[guest.kernel.kallsyms") {
            return Some(DsoKey::GuestKernel);
        }
        if path == b"[vdso32]" {
            return Some(DsoKey::Vdso32);
        }
        if path == b"[vdsox32]" {
            return Some(DsoKey::VdsoX32);
        }
        if path == b"[vdso]" {
            // TODO: I think this could also be Vdso32 when recording on a 32 bit machine.
            return Some(DsoKey::Vdso64);
        }
        if path == b"[vsyscall]" {
            return Some(DsoKey::Vsyscall);
        }
        if (cpumode == PERF_RECORD_MISC_KERNEL || cpumode == PERF_RECORD_MISC_GUEST_KERNEL)
            && path.starts_with(b"[")
        {
            return Some(DsoKey::KernelModule(String::from_utf8_lossy(path).into()));
        }

        let filename = if let Some(final_slash_pos) = path.iter().rposition(|b| *b == b'/') {
            &path[final_slash_pos + 1..]
        } else {
            path
        };

        let dso_key = match (cpumode, filename.strip_suffix(b".ko")) {
            (PERF_RECORD_MISC_KERNEL | PERF_RECORD_MISC_GUEST_KERNEL, Some(kmod_name)) => {
                // "/lib/modules/5.13.0-35-generic/kernel/sound/core/snd-seq-device.ko" -> "[snd-seq-device]"
                let kmod_name = String::from_utf8_lossy(kmod_name);
                DsoKey::KernelModule(format!("[{}]", kmod_name))
            }
            (PERF_RECORD_MISC_KERNEL, _) => DsoKey::Kernel,
            (PERF_RECORD_MISC_GUEST_KERNEL, _) => DsoKey::GuestKernel,
            (PERF_RECORD_MISC_USER | PERF_RECORD_MISC_GUEST_USER, _) => {
                DsoKey::User(String::from_utf8_lossy(filename).into(), path.to_owned())
            }
            _ => return None,
        };
        Some(dso_key)
    }

    pub fn name(&self) -> &str {
        match self {
            DsoKey::Kernel => "[kernel.kallsyms]",
            DsoKey::GuestKernel => "[guest.kernel.kallsyms]",
            DsoKey::Vdso32 => "[vdso32]",
            DsoKey::VdsoX32 => "[vdsox32]",
            DsoKey::Vdso64 => "[vdso]",
            DsoKey::Vsyscall => "[vsyscall]",
            DsoKey::KernelModule(name) => name,
            DsoKey::User(name, _) => name,
        }
    }
}

pub enum Mmap2FileId {
    InodeAndVersion(Mmap2InodeAndVersion),
    BuildId(Vec<u8>),
}

pub struct Mmap2Event {
    pub pid: i32,
    pub tid: i32,
    pub address: u64,
    pub length: u64,
    pub page_offset: u64,
    pub file_id: Mmap2FileId,
    pub protection: u32,
    pub flags: u32,
    pub dso_key: Option<DsoKey>,
    pub path: Vec<u8>,
}

pub struct Mmap2InodeAndVersion {
    pub major: u32,
    pub minor: u32,
    pub inode: u64,
    pub inode_generation: u64,
}

#[derive(Debug)]
pub struct LostEvent {
    pub id: u64,
    pub count: u64,
}

#[derive(Debug)]
pub struct ThrottleEvent {
    pub id: u64,
    pub timestamp: u64,
}

#[derive(Debug)]
pub enum ContextSwitchKind {
    In,
    OutWhileIdle,
    OutWhileRunning,
}

#[derive(Debug)]
pub enum Event<'a> {
    Sample(SampleEvent<'a>),
    Comm(CommEvent),
    Exit(ProcessEvent),
    Fork(ProcessEvent),
    Mmap(MmapEvent),
    Mmap2(Mmap2Event),
    Lost(LostEvent),
    Throttle(ThrottleEvent),
    Unthrottle(ThrottleEvent),
    ContextSwitch(ContextSwitchKind),
    Raw(RawEvent<'a>),
}

impl<'a> fmt::Debug for SampleEvent<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        fmt.debug_map()
            .entry(&"timestamp", &self.timestamp)
            .entry(&"pid", &self.pid)
            .entry(&"tid", &self.tid)
            .entry(&"cpu", &self.cpu)
            .entry(&"period", &self.period)
            .entry(&"regs", &self.regs)
            .entry(&"stack", &self.stack)
            .entry(&"callchain", &self.callchain.as_deref().map(HexSlice))
            .finish()
    }
}

impl fmt::Debug for CommEvent {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use std::str;

        let mut map = fmt.debug_map();
        map.entry(&"pid", &self.pid).entry(&"tid", &self.tid);

        if let Ok(string) = str::from_utf8(&self.name) {
            map.entry(&"name", &string);
        } else {
            map.entry(&"name", &self.name);
        }

        map.finish()
    }
}

impl fmt::Debug for MmapEvent {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        fmt.debug_map()
            .entry(&"pid", &self.pid)
            .entry(&"tid", &self.tid)
            .entry(&"address", &HexValue(self.address))
            .entry(&"length", &HexValue(self.length))
            .entry(&"page_offset", &HexValue(self.page_offset))
            .entry(&"dso_key", &self.dso_key)
            .entry(&"path", &&*String::from_utf8_lossy(&self.path))
            .finish()
    }
}

impl fmt::Debug for Mmap2Event {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        fmt.debug_map()
            .entry(&"pid", &self.pid)
            .entry(&"tid", &self.tid)
            .entry(&"address", &HexValue(self.address))
            .entry(&"length", &HexValue(self.length))
            .entry(&"page_offset", &HexValue(self.page_offset))
            // .entry(&"major", &self.major)
            // .entry(&"minor", &self.minor)
            // .entry(&"inode", &self.inode)
            // .entry(&"inode_generation", &self.inode_generation)
            .entry(&"protection", &HexValue(self.protection as _))
            .entry(&"flags", &HexValue(self.flags as _))
            .entry(&"dso_key", &self.dso_key)
            .entry(&"path", &&*String::from_utf8_lossy(&self.path))
            .finish()
    }
}

impl<'a> fmt::Debug for RawEvent<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        fmt.debug_map()
            .entry(&"kind", &self.kind)
            .entry(&"misc", &self.misc)
            .entry(&"data.len", &self.data.len())
            .finish()
    }
}

impl<'a> RawEvent<'a> {
    #[allow(unused)]
    fn skip_sample_id<T: ByteOrder, R: std::io::Read>(cur: &mut R, sample_type: u64) {
        // struct sample_id {
        //     { u32 pid, tid; }   /* if PERF_SAMPLE_TID set */
        //     { u64 time;     }   /* if PERF_SAMPLE_TIME set */
        //     { u64 id;       }   /* if PERF_SAMPLE_ID set */
        //     { u64 stream_id;}   /* if PERF_SAMPLE_STREAM_ID set  */
        //     { u32 cpu, res; }   /* if PERF_SAMPLE_CPU set */
        //     { u64 id;       }   /* if PERF_SAMPLE_IDENTIFIER set */
        // };
        let (pid, tid) = if sample_type & PERF_SAMPLE_TID != 0 {
            let pid = cur.read_u32::<T>().unwrap();
            let tid = cur.read_u32::<T>().unwrap();
            (Some(pid), Some(tid))
        } else {
            (None, None)
        };

        let timestamp = if sample_type & PERF_SAMPLE_TIME != 0 {
            Some(cur.read_u64::<T>().unwrap())
        } else {
            None
        };

        if sample_type & PERF_SAMPLE_ID != 0 {
            let _id = cur.read_u64::<T>().unwrap();
        }

        if sample_type & PERF_SAMPLE_STREAM_ID != 0 {
            let _stream_id = cur.read_u64::<T>().unwrap();
        }

        let cpu = if sample_type & PERF_SAMPLE_CPU != 0 {
            let cpu = cur.read_u32::<T>().unwrap();
            let _ = cur.read_u32::<T>().unwrap(); // Reserved field; is always zero.
            Some(cpu)
        } else {
            None
        };

        let period = if sample_type & PERF_SAMPLE_PERIOD != 0 {
            let period = cur.read_u64::<T>().unwrap();
            Some(period)
        } else {
            None
        };

        if sample_type & PERF_SAMPLE_IDENTIFIER != 0 {
            let _identifier = cur.read_u64::<T>().unwrap();
        }

        let _ = (pid, tid, cpu, period);
    }

    pub fn parse<T: ByteOrder>(
        self,
        sample_type: u64,
        read_format: u64,
        regs_count: usize,
        sample_regs_user: u64,
        _sample_id_all: bool,
    ) -> Event<'a> {
        match self.kind {
            PERF_RECORD_EXIT | PERF_RECORD_FORK => {
                let raw_data = self.data.as_slice();
                let mut cur = Cursor::new(&raw_data);

                let pid = cur.read_i32::<T>().unwrap();
                let ppid = cur.read_i32::<T>().unwrap();
                let tid = cur.read_i32::<T>().unwrap();
                let ptid = cur.read_i32::<T>().unwrap();
                let timestamp = cur.read_u64::<T>().unwrap();
                // if sample_id_all {
                //     Self::skip_sample_id::<T, _>(&mut cur, sample_type);
                // }

                let event = ProcessEvent {
                    pid,
                    ppid,
                    tid,
                    ptid,
                    timestamp,
                };

                if self.kind == PERF_RECORD_EXIT {
                    Event::Exit(event)
                } else {
                    Event::Fork(event)
                }
            }

            PERF_RECORD_SAMPLE => {
                let raw_data = self.data.as_slice();
                let mut cur = Cursor::new(&raw_data);

                if sample_type & PERF_SAMPLE_IDENTIFIER != 0 {
                    let _identifier = cur.read_u64::<T>().unwrap();
                }

                if sample_type & PERF_SAMPLE_IP != 0 {
                    let _ip = cur.read_u64::<T>().unwrap();
                }

                let (pid, tid) = if sample_type & PERF_SAMPLE_TID != 0 {
                    let pid = cur.read_i32::<T>().unwrap();
                    let tid = cur.read_i32::<T>().unwrap();
                    (Some(pid), Some(tid))
                } else {
                    (None, None)
                };

                let timestamp = if sample_type & PERF_SAMPLE_TIME != 0 {
                    Some(cur.read_u64::<T>().unwrap())
                } else {
                    None
                };

                if sample_type & PERF_SAMPLE_ADDR != 0 {
                    let _addr = cur.read_u64::<T>().unwrap();
                }

                if sample_type & PERF_SAMPLE_ID != 0 {
                    let _id = cur.read_u64::<T>().unwrap();
                }

                if sample_type & PERF_SAMPLE_STREAM_ID != 0 {
                    let _stream_id = cur.read_u64::<T>().unwrap();
                }

                let cpu = if sample_type & PERF_SAMPLE_CPU != 0 {
                    let cpu = cur.read_u32::<T>().unwrap();
                    let _ = cur.read_u32::<T>().unwrap(); // Reserved field; is always zero.
                    Some(cpu)
                } else {
                    None
                };

                let period = if sample_type & PERF_SAMPLE_PERIOD != 0 {
                    let period = cur.read_u64::<T>().unwrap();
                    Some(period)
                } else {
                    None
                };

                if sample_type & PERF_SAMPLE_READ != 0 {
                    if read_format & PERF_FORMAT_GROUP == 0 {
                        let _value = cur.read_u64::<T>().unwrap();
                        if read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 {
                            let _time_enabled = cur.read_u64::<T>().unwrap();
                        }
                        if read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 {
                            let _time_running = cur.read_u64::<T>().unwrap();
                        }
                        if read_format & PERF_FORMAT_ID != 0 {
                            let _id = cur.read_u64::<T>().unwrap();
                        }
                    } else {
                        let nr = cur.read_u64::<T>().unwrap();
                        if read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 {
                            let _time_enabled = cur.read_u64::<T>().unwrap();
                        }
                        if read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 {
                            let _time_running = cur.read_u64::<T>().unwrap();
                        }
                        for _ in 0..nr {
                            let _value = cur.read_u64::<T>().unwrap();
                            if read_format & PERF_FORMAT_ID != 0 {
                                let _id = cur.read_u64::<T>().unwrap();
                            }
                        }
                    }
                }

                let callchain = if sample_type & PERF_SAMPLE_CALLCHAIN != 0 {
                    let callchain_length = cur.read_u64::<T>().unwrap();
                    let mut callchain = Vec::with_capacity(callchain_length as usize);
                    for _ in 0..callchain_length {
                        let addr = cur.read_u64::<T>().unwrap();
                        callchain.push(addr);
                    }
                    Some(callchain)
                } else {
                    None
                };

                if sample_type & PERF_SAMPLE_RAW != 0 {
                    let size = cur.read_u32::<T>().unwrap();
                    cur.set_position(cur.position() + size as u64);
                }

                if sample_type & PERF_SAMPLE_BRANCH_STACK != 0 {
                    let nr = cur.read_u64::<T>().unwrap();
                    if sample_type & PERF_SAMPLE_BRANCH_HW_INDEX != 0 {
                        let _hw_idx = cur.read_u64::<T>().unwrap();
                    }
                    for _ in 0..nr {
                        let _from = cur.read_u64::<T>().unwrap();
                        let _to = cur.read_u64::<T>().unwrap();
                        let _flags = cur.read_u64::<T>().unwrap();
                    }
                }

                let regs = if sample_type & PERF_SAMPLE_REGS_USER != 0 {
                    let regs_abi = cur.read_u64::<T>().unwrap();
                    if regs_abi == 0 {
                        None
                    } else {
                        let regs_end_pos =
                            cur.position() + regs_count as u64 * std::mem::size_of::<u64>() as u64;
                        let regs_range = cur.position() as usize..regs_end_pos as usize;
                        cur.set_position(regs_end_pos);

                        let raw_regs = RawRegs::from_raw_data(self.data.get(regs_range));
                        let regs = Regs::new(sample_regs_user, raw_regs);
                        Some(regs)
                    }
                } else {
                    None
                };

                let stack;
                let dynamic_stack_size;
                if sample_type & PERF_SAMPLE_STACK_USER != 0 {
                    let stack_size = cur.read_u64::<T>().unwrap();
                    let stack_end_pos = cur.position() + stack_size;
                    let stack_range = cur.position() as usize..stack_end_pos as usize;
                    cur.set_position(stack_end_pos);

                    dynamic_stack_size = if stack_size != 0 {
                        cur.read_u64::<T>().unwrap()
                    } else {
                        0
                    };

                    stack = self.data.get(stack_range)
                } else {
                    dynamic_stack_size = 0;
                    stack = RawData::empty();
                }

                if sample_type & PERF_SAMPLE_WEIGHT != 0 {
                    let _weight = cur.read_u64::<T>().unwrap();
                }

                if sample_type & PERF_SAMPLE_DATA_SRC != 0 {
                    let _data_src = cur.read_u64::<T>().unwrap();
                }

                if sample_type & PERF_SAMPLE_TRANSACTION != 0 {
                    let _transaction = cur.read_u64::<T>().unwrap();
                }

                if sample_type & PERF_SAMPLE_REGS_INTR != 0 {
                    let regs_abi = cur.read_u64::<T>().unwrap();
                    if regs_abi != 0 {
                        let regs_end_pos =
                            cur.position() + regs_count as u64 * std::mem::size_of::<u64>() as u64;
                        cur.set_position(regs_end_pos);
                    }
                }

                if sample_type & PERF_SAMPLE_PHYS_ADDR != 0 {
                    let _phys_addr = cur.read_u64::<T>().unwrap();
                }

                if sample_type & PERF_SAMPLE_AUX != 0 {
                    let size = cur.read_u64::<T>().unwrap();
                    cur.set_position(cur.position() + size);
                }

                if sample_type & PERF_SAMPLE_DATA_PAGE_SIZE != 0 {
                    let _data_page_size = cur.read_u64::<T>().unwrap();
                }

                if sample_type & PERF_SAMPLE_CODE_PAGE_SIZE != 0 {
                    let _code_page_size = cur.read_u64::<T>().unwrap();
                }

                Event::Sample(SampleEvent {
                    regs,
                    dynamic_stack_size,
                    stack,
                    callchain,
                    cpu,
                    timestamp,
                    pid,
                    tid,
                    period,
                })
            }

            PERF_RECORD_COMM => {
                let raw_data = self.data.as_slice();
                let mut cur = Cursor::new(&raw_data);

                let pid = cur.read_i32::<T>().unwrap();
                let tid = cur.read_i32::<T>().unwrap();
                let name = &raw_data[cur.position() as usize..];
                let name = &name[0..name
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(name.len())];

                // TODO: Maybe feature-gate this on 3.16+
                let is_execve = self.misc & PERF_RECORD_MISC_COMM_EXEC != 0;

                Event::Comm(CommEvent {
                    pid,
                    tid,
                    name: name.to_owned(),
                    is_execve,
                })
            }

            PERF_RECORD_MMAP => {
                let raw_data = self.data.as_slice();
                let mut cur = Cursor::new(&raw_data);

                // struct {
                //   struct perf_event_header header;
                //
                //   u32 pid, tid;
                //   u64 addr;
                //   u64 len;
                //   u64 pgoff;
                //   char filename[];
                //   struct sample_id sample_id;
                // };

                let pid = cur.read_i32::<T>().unwrap();
                let tid = cur.read_i32::<T>().unwrap();
                let address = cur.read_u64::<T>().unwrap();
                let length = cur.read_u64::<T>().unwrap();
                let page_offset = cur.read_u64::<T>().unwrap();
                let name = &raw_data[cur.position() as usize..];
                let name = &name[0..name
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(name.len())];
                let is_executable = self.misc & PERF_RECORD_MISC_MMAP_DATA == 0;

                Event::Mmap(MmapEvent {
                    pid,
                    tid,
                    address,
                    length,
                    page_offset,
                    is_executable,
                    dso_key: DsoKey::detect(name, self.misc),
                    path: name.to_owned(),
                })
            }

            PERF_RECORD_MMAP2 => {
                let raw_data = self.data.as_slice();
                let mut cur = Cursor::new(&raw_data);

                let pid = cur.read_i32::<T>().unwrap();
                let tid = cur.read_i32::<T>().unwrap();
                let address = cur.read_u64::<T>().unwrap();
                let length = cur.read_u64::<T>().unwrap();
                let page_offset = cur.read_u64::<T>().unwrap();
                let file_id = if self.misc & PERF_RECORD_MISC_MMAP_BUILD_ID != 0 {
                    let build_id_len = cur.read_u8().unwrap();
                    assert!(build_id_len <= 20);
                    let _ = cur.read_u8().unwrap();
                    let _ = cur.read_u16::<T>().unwrap();
                    let build_id =
                        raw_data[cur.position() as usize..][..build_id_len as usize].to_owned();
                    cur.set_position(cur.position() + 20);
                    Mmap2FileId::BuildId(build_id)
                } else {
                    let major = cur.read_u32::<T>().unwrap();
                    let minor = cur.read_u32::<T>().unwrap();
                    let inode = cur.read_u64::<T>().unwrap();
                    let inode_generation = cur.read_u64::<T>().unwrap();
                    Mmap2FileId::InodeAndVersion(Mmap2InodeAndVersion {
                        major,
                        minor,
                        inode,
                        inode_generation,
                    })
                };
                let protection = cur.read_u32::<T>().unwrap();
                let flags = cur.read_u32::<T>().unwrap();
                let name = &raw_data[cur.position() as usize..];
                let name = &name[0..name
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(name.len())];

                Event::Mmap2(Mmap2Event {
                    pid,
                    tid,
                    address,
                    length,
                    page_offset,
                    file_id,
                    protection,
                    flags,
                    dso_key: DsoKey::detect(name, self.misc),
                    path: name.to_owned(),
                })
            }

            PERF_RECORD_LOST => {
                let raw_data = self.data.as_slice();
                let mut cur = Cursor::new(&raw_data);

                let id = cur.read_u64::<T>().unwrap();
                let count = cur.read_u64::<T>().unwrap();
                Event::Lost(LostEvent { id, count })
            }

            PERF_RECORD_THROTTLE | PERF_RECORD_UNTHROTTLE => {
                let raw_data = self.data.as_slice();
                let mut cur = Cursor::new(&raw_data);

                let timestamp = cur.read_u64::<T>().unwrap();
                let id = cur.read_u64::<T>().unwrap();
                let event = ThrottleEvent { id, timestamp };
                if self.kind == PERF_RECORD_THROTTLE {
                    Event::Throttle(event)
                } else {
                    Event::Unthrottle(event)
                }
            }

            PERF_RECORD_SWITCH => {
                let is_out = self.misc & PERF_RECORD_MISC_SWITCH_OUT != 0;
                let is_out_preempt = self.misc & PERF_RECORD_MISC_SWITCH_OUT_PREEMPT != 0;
                let kind = if is_out {
                    if is_out_preempt {
                        ContextSwitchKind::OutWhileRunning
                    } else {
                        ContextSwitchKind::OutWhileIdle
                    }
                } else {
                    ContextSwitchKind::In
                };

                Event::ContextSwitch(kind)
            }

            _ => Event::Raw(self),
        }
    }
}
