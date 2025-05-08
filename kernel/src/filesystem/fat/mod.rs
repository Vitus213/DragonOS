pub mod bpb;
pub mod entry;
pub mod fs;
pub mod utils;
use crate::filesystem::vfs::{FileSystemMaker};
use crate::filesystem::vfs::{FileSystem, FileSystemMakerData};
use alloc::sync::Arc;
use system_error::SystemError;
use crate::filesystem::fat::fs::fat_new;
use crate::filesystem::fat::fs::vfat_new;
use crate::filesystem::vfs::FSMAKER;

#[distributed_slice(FSMAKER)]
static FATMAKER: FileSystemMaker = FileSystemMaker::new(
    "fat",
    &(fat_new as fn(
        Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
);
#[distributed_slice(FSMAKER)]
static VFATMAKER: FileSystemMaker = FileSystemMaker::new(
    "vfat",
    &(vfat_new as fn(
        Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
);