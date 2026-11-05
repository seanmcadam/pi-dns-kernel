#[derive(Debug)]
pub enum FsError {
    NotInitialized,
    ReadError,
    WriteError,
}

pub fn init() -> Result<(), FsError> {
    log::info!("Filesystem initialization");

    // TODO: Initialize SD card driver (EMMC2)
    // TODO: Mount FAT32 filesystem

    log::warn!("Filesystem support not yet implemented");
    Err(FsError::NotInitialized)
}

pub fn read_config() -> Result<alloc::string::String, FsError> {
    Err(FsError::NotInitialized)
}

pub fn write_config(_data: &str) -> Result<(), FsError> {
    Err(FsError::NotInitialized)
}
