use core::{any::Any, fmt::{Debug, Formatter}};
use log::error;
use system_error::SystemError;
use bitmap::traits::BitMapOps;
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use unified_init::macros::unified_init;
use crate::{
    driver::{
        base::{
            block::{
                block_device::{BlockDevice, BlockId, GeneralBlockRange, LBA_SIZE},
                disk_info::Partition,
                manager::{block_dev_manager, BlockDevMeta},
            },
            class::Class,
            device::{
                bus::{Bus, bus_manager},
                device_number::{DeviceNumber, Major},
                driver::{Driver, DriverCommonData},
                DevName, Device, DeviceCommonData, DeviceId, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
            subsys::SubSysPrivate,
        },
    },
    filesystem::{
        devfs::{DevFS, DeviceINode, devfs_register},
        kernfs::KernFSInode,
        vfs::{IndexNode, Metadata, InodeId},
    },
    init::initcall::INITCALL_POSTCORE,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};
const LOOP_BASENAME: &str = "loop";
//LoopDevice是一个虚拟的块设备，它将文件映射到块设备上.
pub struct LoopDevice{
    inner:SpinLock<LoopDeviceInner>,//加锁保护LoopDeviceInner
    block_dev_meta: BlockDevMeta,
    dev_id: Arc<DeviceId>,
    locked_kobj_state: LockedKObjectState,//对Kobject状态的锁
    self_ref: Weak<Self>,//对自身的弱引用
    fs: RwLock<Weak<DevFS>>,//文件系统弱引用
}
//Inner内数据会改变所以加锁
pub struct LoopDeviceInner{
   // 关联的文件节点
    pub file_inode: Arc<dyn IndexNode>,
    // 文件大小
    pub file_size: usize,
    // 设备名称
    pub device_number: DeviceNumber,
    // 数据偏移量
    pub offset: usize,
    // 数据大小限制
    pub size_limit: usize,
    // 是否允许用户直接 I/O 操作
    pub user_direct_io: bool,
    // 是否只读
    pub read_only: bool,
    // 是否可见
    pub visible: bool,
    // 使用弱引用避免循环引用
    pub self_ref: Weak<LoopDevice>,
    pub kobject_common: KObjectCommonData,
    pub device_common: DeviceCommonData,
}
impl Debug for LoopDevice{
     fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoopDevice")
            .field("devname", &self.block_dev_meta.devname)
            .field("dev_id", &self.dev_id.id())
            .finish()
    }
}
impl LoopDevice{
    fn inner(&self) -> SpinLockGuard<LoopDeviceInner> {
        self.inner.lock()
    }

    //将文件绑定到空余的loop设备中,在loop设备里面包着一个indexnode,indexnode是一个trait对文件的一个抽象
    pub fn new(file_inode: Arc<dyn IndexNode>, dev_id: Arc<DeviceId>) -> Option<Arc<Self>> {
        let devname = loop_manager().alloc_id()?;
        log::info!("Find loop device with name: {}", devname.name());
        // 获取文件大小
        let file_size = match file_inode.metadata() {
            Ok(metadata) => metadata.size,
            Err(_) => {
                error!("Failed to get file metadata for loop device");
                return None;
            }
        };
        
        let dev = Arc::new_cyclic(|self_ref| Self {
            inner: SpinLock::new(
            LoopDeviceInner {
                file_inode,
                file_size: file_size as usize,
                device_number: DeviceNumber::new(Major::new(7), 0), // Loop 设备主设备号为 7
                offset: 0,
                size_limit: 0,
                user_direct_io: false,
                read_only: false,
                visible: true,
                self_ref: self_ref.clone(),
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
            }),
            block_dev_meta: BlockDevMeta::new(devname, Major::new(7)), // Loop 设备主设备号为 7
            dev_id,// DeviceId { ty: "loop", name: "loop3" }
            locked_kobj_state: LockedKObjectState::default(),
            self_ref: self_ref.clone(),
            fs: RwLock::new(Weak::default()),
        });

        Some(dev)
    }

    /// 设置 loop 设备关联的文件
    pub fn set_file(&self, file_inode: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let mut inner = self.inner();
        
        // 获取文件大小
        let file_size = file_inode.metadata()?.size;
        
        inner.file_inode = file_inode;
        inner.file_size = file_size as usize;
        
        Ok(())
    }

    /// 获取文件大小
    pub fn file_size(&self) -> usize {
        self.inner().file_size
    }

    /// 设置只读模式
    pub fn set_read_only(&self, read_only: bool) {
        self.inner().read_only = read_only;
    }

    /// 检查是否为只读
    pub fn is_read_only(&self) -> bool {
        self.inner().read_only
    }
}

impl KObject for LoopDevice {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }
    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        LOOP_BASENAME.to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }
}

//对loopdevice进行抽象
impl IndexNode for LoopDevice {
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }
    
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
    
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }
     fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let file_metadata = self.inner().file_inode.metadata()?;
        let metadata = Metadata{
            dev_id: 0,
            inode_id: InodeId::new(0), // Loop 设备通常没有实际的 inode ID
            size: self.inner().file_size as i64,
            blk_size: LBA_SIZE as usize,
            blocks: (self.inner().file_size + LBA_SIZE - 1) / LBA_SIZE as usize, // 计算块数
            atime: file_metadata.atime,
            mtime: file_metadata.mtime,
            ctime: file_metadata.ctime,
            btime: file_metadata.btime,
            file_type: crate::filesystem::vfs::FileType::BlockDevice,
            mode: crate::filesystem::vfs::syscall::ModeType::from_bits_truncate(0o644),
            nlinks: 1,
            uid: 0, // 默认用户 ID
            gid: 0, // 默认组 ID
            raw_dev: self.inner().device_number,
        };
        Ok(metadata.clone())
    }
}

impl DeviceINode for LoopDevice {
    fn set_fs(&self, fs: alloc::sync::Weak<crate::filesystem::devfs::DevFS>) {
        *self.fs.write() = fs;
    }
}

impl Device for LoopDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Block
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(LOOP_BASENAME.to_string(), None)  
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone()?.upgrade();
        if r.is_none() {
            guard.device_common.class = None;
        }
        return r;
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let r = self.inner().device_common.driver.clone()?.upgrade();
        if r.is_none() {
            self.inner().device_common.driver = None;
        }
        return r;
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        self.inner().device_common.can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner().device_common.can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl BlockDevice for LoopDevice {
    fn dev_name(&self) -> &DevName {
        &self.block_dev_meta.devname
    }

    fn blkdev_meta(&self) -> &BlockDevMeta {
        &self.block_dev_meta
    }

    fn disk_range(&self) -> GeneralBlockRange {
        let inner = self.inner();
        let blocks = inner.file_size / LBA_SIZE;
        drop(inner);
        GeneralBlockRange::new(0, blocks).unwrap()
    }
    fn read_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let inner = self.inner();
        let offset = inner.offset + lba_id_start * LBA_SIZE;
        let len = count * LBA_SIZE;
        
        // 通过文件 inode 读取数据
        // 使用一个空的 FilePrivateData 作为占位符
        use crate::filesystem::vfs::FilePrivateData;
        use crate::libs::spinlock::SpinLock;
        let data = SpinLock::new(FilePrivateData::Unused);
        let data_guard = data.lock();
        
        inner.file_inode.read_at(offset, len, buf, data_guard).map_err(|_| SystemError::EIO)
    }

    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let inner = self.inner();
        
        // 检查是否只读
        if inner.read_only {
            return Err(SystemError::EROFS);
        }
        
        let offset = inner.offset + lba_id_start * LBA_SIZE;
        let len = count * LBA_SIZE;
        
        // 通过文件 inode 写入数据
        // 使用一个空的 FilePrivateData 作为占位符
        use crate::filesystem::vfs::FilePrivateData;
        use crate::libs::spinlock::SpinLock;
        let data = SpinLock::new(FilePrivateData::Unused);
        let data_guard = data.lock();
        
        inner.file_inode.write_at(offset, len, buf, data_guard).map_err(|_| SystemError::EIO)
    }

    fn sync(&self) -> Result<(), SystemError> {
        // Loop 设备的同步操作
        Ok(())
    }

    fn blk_size_log2(&self) -> u8 {
        9 // 512 bytes = 2^9
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn device(&self) -> Arc<dyn Device> {
        self.self_ref.upgrade().unwrap()
    }

    fn block_size(&self) -> usize {
        LBA_SIZE
    }

    fn partitions(&self) -> Vec<Arc<Partition>> {
        // Loop 设备通常不支持分区
        Vec::new()
    }
}


/// Loop设备驱动
#[derive(Debug)]
#[cast_to([sync] Driver)]
pub struct LoopDeviceDriver {
    inner: SpinLock<InnerLoopDeviceDriver>,
    kobj_state: LockedKObjectState,
}
struct InnerLoopDeviceDriver{
    driver_common: DriverCommonData,
    kobj_common: KObjectCommonData,
}
impl Debug for InnerLoopDeviceDriver {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerLoopDeviceDriver")
            .field("driver_common", &self.driver_common)
            .field("kobj_common", &self.kobj_common)
            .finish()
    }
}
impl LoopDeviceDriver {
    pub fn new() -> Arc<Self> {
        let inner = InnerLoopDeviceDriver{
            driver_common: DriverCommonData::default(),
            kobj_common: KObjectCommonData::default(),
        };
        Arc::new(Self {
            inner: SpinLock::new(inner),
            kobj_state: LockedKObjectState::default(),
        })
    }
    fn inner(&self) -> SpinLockGuard<InnerLoopDeviceDriver> {
        self.inner.lock()
    }
}

impl Driver for LoopDeviceDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new("loop".to_string(), None))
    }

     fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.inner().driver_common.devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        self.inner().driver_common.push_device(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        self.inner().driver_common.delete_device(device);
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().driver_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().driver_common.bus = bus;
    }
}

impl KObject for LoopDeviceDriver {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobj_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobj_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobj_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobj_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobj_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobj_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobj_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobj_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        "loop".to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}
//负责管理 Loop 设备 ID 的分配和释放
pub struct LoopManager {
    inner: SpinLock<InnerLoopManager>,
}

struct InnerLoopManager {
    //管理loop设备分配情况
    id_bmp: bitmap::StaticBitmap<{ LoopManager::MAX_DEVICES }>,
    devname: [Option<DevName>; LoopManager::MAX_DEVICES],
}

impl LoopManager {
    pub const MAX_DEVICES: usize = 8; // 最多支持 8 个 loop 设备

    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerLoopManager {
                id_bmp: bitmap::StaticBitmap::new(),
                devname: [const { None }; Self::MAX_DEVICES],
            }),
        }
    }

    fn inner(&self) -> SpinLockGuard<InnerLoopManager> {
        self.inner.lock()
    }

    pub fn alloc_id(&self) -> Option<DevName> {
        let mut inner = self.inner();
        let idx = inner.id_bmp.first_false_index()?;
        inner.id_bmp.set(idx, true);
        let name = Self::format_name(idx);
        inner.devname[idx] = Some(name.clone());
        Some(name)
    }

    /// 生成 loop 设备名称，如 'loop0', 'loop1' 等
    fn format_name(id: usize) -> DevName {
        DevName::new(format!("loop{}", id), id)
    }

    #[allow(dead_code)]
    pub fn free_id(&self, id: usize) {
        if id >= Self::MAX_DEVICES {
            return;
        }
        self.inner().id_bmp.set(id, false);
        self.inner().devname[id] = None;
    }
}
/// Loop设备总线
#[derive(Debug)]
pub struct LoopBus {
    private_data: SpinLock<LoopBusPrivate>,
    subsystem: SubSysPrivate,
    kobj_state: LockedKObjectState,
}

#[derive(Debug)]
struct LoopBusPrivate {
    kobject_common: KObjectCommonData, 
}

impl LoopBus {
    pub fn new() -> Arc<Self> {
        let subsystem = SubSysPrivate::new("loop".to_string(), None, None, &[]);
        Arc::new(Self {
            private_data: SpinLock::new(LoopBusPrivate {
                kobject_common: KObjectCommonData::default(),
            }),
            subsystem,
            kobj_state: LockedKObjectState::default(),
        })
    }
}

impl Bus for LoopBus {
    fn name(&self) -> String {
        "loop".to_string()
    }

    fn dev_name(&self) -> String {
        "loop".to_string()
    }

    fn root_device(&self) -> Option<Weak<dyn Device>> {
        None
    }

    fn set_root_device(&self, _device: Option<Weak<dyn Device>>) {
        // Loop总线不需要根设备
    }

    fn subsystem(&self) -> &SubSysPrivate {
        &self.subsystem
    }

    fn remove(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        Ok(())
    }

    fn shutdown(&self, _device: &Arc<dyn Device>) {
        // Loop设备关闭逻辑
    }

    fn resume(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        Ok(())
    }

    fn match_device(
        &self,
        device: &Arc<dyn Device>,
        _driver: &Arc<dyn Driver>,
    ) -> Result<bool, SystemError> {
        // 检查设备是否为loop设备
        if device.id_table().name().starts_with("loop") {
            return Ok(true);
        }
        Ok(false)
    }
}

impl KObject for LoopBus {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.private_data.lock().kobject_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.private_data.lock().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.private_data.lock().kobject_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.private_data.lock().kobject_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.private_data.lock().kobject_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.private_data.lock().kobject_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.private_data.lock().kobject_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.private_data.lock().kobject_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        "loop".to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

/// Loop总线全局实例
static mut LOOP_BUS: Option<Arc<LoopBus>> = None;
/// Loop驱动全局实例  
static mut LOOP_DRIVER: Option<Arc<LoopDeviceDriver>> = None;

/// 获取loop总线实例
pub fn loop_bus() -> Option<Arc<LoopBus>> {
    unsafe { LOOP_BUS.clone() }
}

/// 获取loop驱动实例
pub fn loop_driver() -> Option<Arc<LoopDeviceDriver>> {
    unsafe { LOOP_DRIVER.clone() }
}

/// 初始化 Loop 设备子系统
#[unified_init(INITCALL_POSTCORE)]
pub fn loop_init() -> Result<(), SystemError> {
    log::info!("Initializing Loop device subsystem");

    // 初始化管理器
    unsafe {
        LOOP_MANAGER = Some(LoopManager::new());
    }
    // 注册 Loop 设备 ID 分配器
    // loop_manager().register_id_allocator("loop", Arc::new(LoopManager::new())); // Removed: No such method
    // 创建并注册总线
    let bus = LoopBus::new();
    bus_manager().register(bus.clone())?;
    unsafe {
        LOOP_BUS = Some(bus);
    }

    // 创建并注册驱动
    let driver = LoopDeviceDriver::new();
    if let Some(bus) = loop_bus() {
        driver.set_bus(Some(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>))));
    }
    
    use crate::driver::base::device::driver::driver_manager;
    driver_manager().register(driver.clone())?;
    unsafe {
        LOOP_DRIVER = Some(driver);
    }

    // 创建并注册8个loop设备
    for i in 0..LoopManager::MAX_DEVICES {
        let dummy_inode = Arc::new(DummyIndexNode::new(i)); // 创建一个虚拟的文件节点
        log::info!("Creating loop device loop{}", i);
        if let Err(e) = create_loop_device(dummy_inode) {
            log::error!("Failed to create loop device {}: {:?}", i, e);
        } else {
            log::info!("Successfully created loop device loop{}", i);
        }
    }
    log::info!("initializing loop device complete");

    Ok(())

}

/// 创建并注册一个新的 loop 设备
pub fn create_loop_device(file_inode: Arc<dyn IndexNode>) -> Result<Arc<LoopDevice>, SystemError> {
    log::info!("starting to create loop device");
    log::info!("Creating loop device for file: {:?}", file_inode);
    // 创建设备 ID
    let dev_id = DeviceId::new(None, None).unwrap_or_else(|| DeviceId::new(Some("loop"), Some("unknown".to_string())).expect("Failed to create device ID"));
    
    // 创建 loop 设备
    let loop_device = LoopDevice::new(file_inode, dev_id)
        .ok_or(SystemError::ENOMEM)?;

    // 设置总线
    if let Some(bus) = loop_bus() {
        loop_device.set_bus(Some(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>))));
    }

    // 注册到设备管理器
    use crate::driver::base::device::device_manager;
    device_manager().add_device(loop_device.clone())?;

    // 注册到块设备管理器
    block_dev_manager().register(loop_device.clone() as Arc<dyn BlockDevice>)?;
    
    // 注册到 DevFS
    devfs_register(loop_device.dev_name().name(), loop_device.clone())?;
    
    Ok(loop_device)
}



/// Loop 设备管理器,负责分配和释放 Loop 设备 ID
static mut LOOP_MANAGER: Option<LoopManager> = None;

#[inline]
fn loop_manager() -> &'static LoopManager {
    unsafe { LOOP_MANAGER.as_ref().unwrap() }
}



/// 定义一个虚拟的 IndexNode 实现，用于占位
#[derive(Debug)]
struct DummyIndexNode {
    id: usize,
}

impl DummyIndexNode {
    pub fn new(id: usize) -> Self {
        Self { id }
    }
}

impl IndexNode for DummyIndexNode {
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        Ok(crate::filesystem::vfs::Metadata {
            dev_id: 0,
            inode_id: InodeId::new(self.id),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: crate::time::PosixTimeSpec::default(),
            mtime: crate::time::PosixTimeSpec::default(),
            ctime: crate::time::PosixTimeSpec::default(),
            btime: crate::time::PosixTimeSpec::default(),
            file_type: crate::filesystem::vfs::FileType::BlockDevice,
            mode: crate::filesystem::vfs::syscall::ModeType::from_bits_truncate(0o644),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: DeviceNumber::new(Major::new(7), self.id as u32),
        })
    }
}

