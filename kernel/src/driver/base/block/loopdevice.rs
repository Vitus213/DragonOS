use alloc::rc::Weak;
use alloc::string::String;
use alloc::sync::Arc;
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::IndexNode;
use crate::driver::base::block::manager::BlockDevMeta;
use crate::driver::base::kobject::KObjectCommonData;
//参考源码为linux-6.6.21
use crate::libs::spinlock::SpinLock;
use crate::driver::base::device::DeviceCommonData;

pub struct LoopDevice{
    inner:SpinLock<LoopDeviceInner>,
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
    block_dev_meta: BlockDevMeta,
}
//Inner内数据会改变所以加锁
pub struct LoopDeviceInner{
    pub file_inode: Arc<dyn IndexNode>,
    pub file_size: usize,
    pub device_name: String,//loop设备的名称
    pub device_number: DeviceNumber, //设备号
    pub offset:usize, //偏移量
    pub size_limit: usize, //大小限制
    pub user_direct_io: bool, //是否允许用户直接IO
    pub read_only: bool, //是否只读
    pub visible: bool, //是否可见
    pub self_ref: Weak<LoopDevice>, //使用Weak引用避免Arc循环引用导致
}