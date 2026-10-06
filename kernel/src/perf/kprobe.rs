use super::Result;
use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::KProbeContext;
use crate::bpf::helper::BPF_HELPER_FUN_SET;
use crate::bpf::prog::BpfProg;
use crate::debug::kprobe::args::KprobeInfo;
use crate::debug::kprobe::{register_kprobe, unregister_kprobe, LockKprobe};
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, IndexNode};
use crate::libs::casting::DowncastArc;
use crate::libs::mutex::MutexGuard;
use crate::perf::util::PerfProbeArgs;
use crate::perf::{BasicPerfEbpfCallBack, PerfEventOps};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Debug;
use kprobe::{CallBackFunc, ProbeArgs};
use rbpf::EbpfVmRaw;
use system_error::SystemError;
#[derive(Debug)]
pub struct KprobePerfEvent {
    _args: PerfProbeArgs,
    kprobe: LockKprobe,
}

impl Drop for KprobePerfEvent {
    fn drop(&mut self) {
        unregister_kprobe(self.kprobe.clone());
    }
}

impl KprobePerfEvent {
    pub fn do_set_bpf_prog(&self, prog_file: Arc<File>) -> Result<()> {
        let file = prog_file
            .inode()
            .downcast_arc::<BpfProg>()
            .ok_or(SystemError::EINVAL)?;
        let prog_slice = file.insns();

        let prog_slice =
            unsafe { core::slice::from_raw_parts(prog_slice.as_ptr(), prog_slice.len()) };
        let mut vm = EbpfVmRaw::new(Some(prog_slice)).map_err(|e| {
            log::error!("create ebpf vm failed: {:?}", e);
            SystemError::EINVAL
        })?;

        for (id, f) in BPF_HELPER_FUN_SET.get() {
            vm.register_helper(*id, *f)
                .map_err(|_| SystemError::EINVAL)?;
        }

        // create a callback to execute the ebpf prog
        let callback;

        #[cfg(target_arch = "x86_64")]
        {
            use crate::perf::JITMem;

            log::info!("Using JIT compilation for BPF program on x86_64 architecture");
            // memcg 计费拒绝（当前任务组 memory.max 打满）是可预期的 ENOMEM，
            // 必须经 `?` 传播回 PERF_EVENT_IOC_SET_BPF 调用者，而不是 panic。
            let jit_mem = Box::new(JITMem::try_for_bpf_program(prog_slice.len())?);
            let jit_mem_addr = Box::into_raw(jit_mem) as usize;
            let jit_result = unsafe {
                vm.set_jit_exec_memory(&mut *(jit_mem_addr as *mut JITMem))
                    .and_then(|_| vm.jit_compile())
            };
            if let Err(err) = jit_result {
                log::error!("kprobe BPF JIT compilation failed: {:?}", err);
                // 归还已泄漏前的所有权，触发 uncharge，避免计费滞留。
                unsafe { drop(Box::from_raw(jit_mem_addr as *mut JITMem)) };
                return Err(SystemError::EINVAL);
            }
            let basic_callback = BasicPerfEbpfCallBack::new(file, vm, jit_mem_addr);
            callback = Box::new(KprobePerfCallBack(basic_callback));
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            vm.register_allowed_memory(0..u64::MAX);
            let basic_callback = BasicPerfEbpfCallBack::new(file, vm);
            callback = Box::new(KprobePerfCallBack(basic_callback));
        }

        // update callback for kprobe
        self.kprobe.write().update_event_callback(callback);
        Ok(())
    }
}

pub struct KprobePerfCallBack(BasicPerfEbpfCallBack);

impl CallBackFunc for KprobePerfCallBack {
    fn call(&self, trap_frame: &dyn ProbeArgs) {
        let trap_frame = trap_frame.as_any().downcast_ref::<TrapFrame>().unwrap();
        let pt_regs = KProbeContext::from(trap_frame);
        let probe_context = unsafe {
            core::slice::from_raw_parts_mut(
                &pt_regs as *const KProbeContext as *mut u8,
                size_of::<KProbeContext>(),
            )
        };
        self.0.call(probe_context);
    }
}

impl IndexNode for KprobePerfEvent {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("read_at not implemented for PerfEvent");
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("write_at not implemented for PerfEvent");
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        panic!("fs not implemented for PerfEvent");
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>> {
        Err(SystemError::ENOSYS)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        None
    }

    fn absolute_path(&self) -> core::result::Result<String, SystemError> {
        Ok(String::from("kprobe_perf_event"))
    }
}

impl PerfEventOps for KprobePerfEvent {
    fn set_bpf_prog(&self, bpf_prog: Arc<File>) -> Result<()> {
        self.do_set_bpf_prog(bpf_prog)
    }
    fn enable(&self) -> Result<()> {
        self.kprobe.write().enable();
        Ok(())
    }
    fn disable(&self) -> Result<()> {
        self.kprobe.write().disable();
        Ok(())
    }

    fn readable(&self) -> bool {
        false
    }
}

pub fn perf_event_open_kprobe(args: PerfProbeArgs) -> KprobePerfEvent {
    let symbol = args.name.clone();
    log::info!("create kprobe for symbol: {symbol}");
    let kprobe_info = KprobeInfo {
        pre_handler: |_| {},
        post_handler: |_| {},
        fault_handler: None,
        event_callback: None,
        symbol: Some(symbol),
        addr: None,
        offset: 0,
        enable: false,
    };
    let kprobe = register_kprobe(kprobe_info).expect("create kprobe failed");
    KprobePerfEvent {
        _args: args,
        kprobe,
    }
}
