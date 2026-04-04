impl MountableFileSystem for Cgroup2Fs {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mut nsdelegate = false;
        if let Some(opts) = raw_data {
            for raw in opts.split(',') {
                let token = raw.trim();
                if token.is_empty() {
                    continue;
                }
                match token {
                    "nsdelegate" => nsdelegate = true,
                    "nsdelegate=0" => nsdelegate = false,
                    "nsdelegate=1" => nsdelegate = true,
                    _ => return Err(SystemError::EINVAL),
                }
            }
        }

        let root_cgroup = ProcessManager::current_pcb()
            .nsproxy()
            .cgroup_ns
            .root_cgroup()
            .clone();
        Ok(Some(Arc::new(Cgroup2MountData {
            root_cgroup,
            nsdelegate,
        })))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data.and_then(|d| d.as_any().downcast_ref::<Cgroup2MountData>());
        let root_cgroup = mount_data
            .map(|d| d.root_cgroup.clone())
            .unwrap_or_else(|| cgroup_root().root());
        let nsdelegate = mount_data.map(|d| d.nsdelegate).unwrap_or(false);
        Ok(Cgroup2Fs::new(root_cgroup, nsdelegate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::cgroup_root_node;

    fn read_all(inode: &Arc<Cgroup2Inode>) -> String {
        let mut inner = inode.inner.lock();
        let mut buf = [0u8; 64];
        let n = Cgroup2Inode::read_file(&mut *inner, 0, buf.len(), &mut buf).unwrap();
        core::str::from_utf8(&buf[..n]).unwrap().to_string()
    }

    #[test]
    fn memory_max_and_high_roundtrip_and_max_keyword() {
        let cg = cgroup_root_node();

        let file_max = Cgroup2Inode::new_file(
            "memory.max".to_string(),
            cg.clone(),
            CgroupCoreFile::MemoryMax,
            b"max\n",
        );

        assert_eq!(cg.memory_max(), None);
        let written = Cgroup2Inode::write_file(&file_max, 0, b"4096\n").unwrap();
        assert_eq!(written, b"4096\n".len());
        assert_eq!(cg.memory_max(), Some(4096));
        assert_eq!(read_all(&file_max), "4096\n");

        let written = Cgroup2Inode::write_file(&file_max, 0, b"max\n").unwrap();
        assert_eq!(written, b"max\n".len());
        assert_eq!(cg.memory_max(), None);
        assert_eq!(read_all(&file_max), "max\n");

        let file_high = Cgroup2Inode::new_file(
            "memory.high".to_string(),
            cg.clone(),
            CgroupCoreFile::MemoryHigh,
            b"max\n",
        );
        assert_eq!(cg.memory_high(), None);
        let written = Cgroup2Inode::write_file(&file_high, 0, b"2048\n").unwrap();
        assert_eq!(written, b"2048\n".len());
        assert_eq!(cg.memory_high(), Some(2048));
        assert_eq!(read_all(&file_high), "2048\n");

        let written = Cgroup2Inode::write_file(&file_high, 0, b"max\n").unwrap();
        assert_eq!(written, b"max\n".len());
        assert_eq!(cg.memory_high(), None);
        assert_eq!(read_all(&file_high), "max\n");
    }

    #[test]
    fn memory_low_roundtrip_and_rejects_max_keyword() {
        let cg = cgroup_root_node();

        let file_low = Cgroup2Inode::new_file(
            "memory.low".to_string(),
            cg.clone(),
            CgroupCoreFile::MemoryLow,
            b"0\n",
        );

        assert_eq!(cg.memory_low(), Some(0));

        let written = Cgroup2Inode::write_file(&file_low, 0, b"1024\n").unwrap();
        assert_eq!(written, b"1024\n".len());
        assert_eq!(cg.memory_low(), Some(1024));
        assert_eq!(read_all(&file_low), "1024\n");

        let err = Cgroup2Inode::write_file(&file_low, 0, b"max\n").unwrap_err();
        assert_eq!(err, SystemError::EINVAL);
        assert_eq!(cg.memory_low(), Some(1024));
        assert_eq!(read_all(&file_low), "1024\n");
    }
}

register_mountable_fs!(Cgroup2Fs, CGROUP2FSMAKER, "cgroup2");
