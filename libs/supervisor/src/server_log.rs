//! Bounded output log for a goblin with attach handlers.
use goblins_unix as unix;
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
};
const LOG: u64 = 1 << 20;
/// The output of an agent without a terminal, kept in a bounded log.
pub(crate) struct ServerLog {
    pipe: Option<File>,
    path: PathBuf,
    file: File,
    size: u64,
}
impl ServerLog {
    pub(crate) fn new(pipe: OwnedFd, path: PathBuf) -> crate::Result<Self> {
        unix::nonblocking(pipe.as_raw_fd())?;
        Ok(Self {
            pipe: Some(File::from(pipe)),
            file: Self::create(&path)?,
            path,
            size: 0,
        })
    }
    fn create(path: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    pub(crate) fn drained(&self) -> bool {
        self.pipe.is_none()
    }
    /// Move what the agent wrote into the log, without blocking.
    pub(crate) fn tick(&mut self) {
        let mut bytes = [0; 8192];
        for _ in 0..8 {
            let Some(pipe) = &mut self.pipe else {
                return;
            };
            let n = match pipe.read(&mut bytes) {
                Ok(0) => {
                    self.pipe = None;
                    return;
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.pipe = None;
                    return;
                }
            };
            if self.size + n as u64 > LOG {
                let mut old = self.path.clone().into_os_string();
                old.push(".1");
                let _ = fs::rename(&self.path, old);
                match Self::create(&self.path) {
                    Ok(file) => self.file = file,
                    Err(_) => continue,
                }
                self.size = 0;
            }
            if self.file.write_all(&bytes[..n]).is_ok() {
                self.size += n as u64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logs_rotate_at_the_existing_bound_without_losing_the_final_bytes() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let path = directory.join("server.log");
        let (reader, writer) = unix::pipe().unwrap();
        let mut log = ServerLog::new(reader, path.clone()).unwrap();
        let mut writer = File::from(writer);
        for _ in 0..(LOG / 4096 + 1) {
            writer.write_all(&[b'x'; 4096]).unwrap();
            log.tick();
        }
        writer.write_all(b"last\0bytes").unwrap();
        drop(writer);
        log.tick();
        assert!(log.drained());
        assert_eq!(
            fs::metadata(directory.join("server.log.1")).unwrap().len(),
            LOG
        );
        let output = fs::read(&path).unwrap();
        assert_eq!(output.len(), 4096 + 10);
        assert!(output.ends_with(b"last\0bytes"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
