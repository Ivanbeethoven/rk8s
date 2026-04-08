//! notify kernel.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;

use bincode::Options;
use bytes::{Buf, Bytes};
use futures_channel::mpsc::UnboundedSender;
use futures_util::future::Either;
use futures_util::sink::SinkExt;

use crate::helper::get_bincode_config;
use crate::raw::abi::{
    fuse_notify_code, fuse_notify_delete_out, fuse_notify_inval_entry_out,
    fuse_notify_inval_inode_out, fuse_notify_poll_wakeup_out, fuse_notify_retrieve_out,
    fuse_notify_store_out, fuse_out_header, FUSE_NOTIFY_DELETE_OUT_SIZE,
    FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE, FUSE_NOTIFY_INVAL_INODE_OUT_SIZE,
    FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE, FUSE_NOTIFY_RETRIEVE_OUT_SIZE, FUSE_NOTIFY_STORE_OUT_SIZE,
    FUSE_OUT_HEADER_SIZE,
};
use crate::raw::FuseData;

#[derive(Debug, Clone)]
/// notify kernel there are something need to handle.
pub struct Notify {
    sender: UnboundedSender<FuseData>,
}

impl Notify {
    pub(crate) fn new(sender: UnboundedSender<FuseData>) -> Self {
        Self { sender }
    }

    /// notify kernel there are something need to handle. If notify failed, the `kind` will be
    /// return in `Err`.
    async fn notify(&mut self, kind: NotifyKind) -> Result<(), NotifyKind> {
        let data = match &kind {
            NotifyKind::Wakeup { kh } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_POLL as i32,
                    unique: 0,
                };

                let wakeup_out = fuse_notify_poll_wakeup_out { kh: *kh };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &wakeup_out)
                    .expect("vec size is not enough");

                Either::Left(data)
            }

            NotifyKind::InvalidInode { inode, offset, len } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_INODE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_INVAL_INODE as i32,
                    unique: 0,
                };

                let invalid_inode_out = fuse_notify_inval_inode_out {
                    ino: *inode,
                    off: *offset,
                    len: *len,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_INODE_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &invalid_inode_out)
                    .expect("vec size is not enough");

                Either::Left(data)
            }

            NotifyKind::InvalidEntry { parent, name } => {
                let name_bytes = name.as_bytes();
                // Linux kernel reads namelen+1 bytes (name + null terminator)
                let payload_len = name_bytes.len() + 1;
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE
                        + FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE
                        + payload_len) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_INVAL_ENTRY as i32,
                    unique: 0,
                };

                let invalid_entry_out = fuse_notify_inval_entry_out {
                    parent: *parent,
                    namelen: name_bytes.len() as _,
                    _padding: 0,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &invalid_entry_out)
                    .expect("vec size is not enough");

                let mut name_with_nul = Vec::with_capacity(payload_len);
                name_with_nul.extend_from_slice(name_bytes);
                name_with_nul.push(0);

                Either::Right((data, Bytes::from(name_with_nul)))
            }

            NotifyKind::Delete {
                parent,
                child,
                name,
            } => {
                let name_bytes = name.as_bytes();
                // Linux kernel reads namelen+1 bytes (name + null terminator)
                let payload_len = name_bytes.len() + 1;
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE
                        + FUSE_NOTIFY_DELETE_OUT_SIZE
                        + payload_len) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_DELETE as i32,
                    unique: 0,
                };

                let delete_out = fuse_notify_delete_out {
                    parent: *parent,
                    child: *child,
                    namelen: name_bytes.len() as _,
                    _padding: 0,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_DELETE_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &delete_out)
                    .expect("vec size is not enough");

                let mut name_with_nul = Vec::with_capacity(payload_len);
                name_with_nul.extend_from_slice(name_bytes);
                name_with_nul.push(0);

                Either::Right((data, Bytes::from(name_with_nul)))
            }

            NotifyKind::Store {
                inode,
                offset,
                data,
            } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_STORE_OUT_SIZE + data.len())
                        as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_STORE as i32,
                    unique: 0,
                };

                let store_out = fuse_notify_store_out {
                    nodeid: *inode,
                    offset: *offset,
                    size: data.len() as _,
                    _padding: 0,
                };

                let mut data_buf =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_STORE_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data_buf, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data_buf, &store_out)
                    .expect("vec size is not enough");

                Either::Right((data_buf, data.clone()))
            }

            NotifyKind::Retrieve {
                notify_unique,
                inode,
                offset,
                size,
            } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_RETRIEVE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_RETRIEVE as i32,
                    unique: 0,
                };

                let retrieve_out = fuse_notify_retrieve_out {
                    notify_unique: *notify_unique,
                    nodeid: *inode,
                    offset: *offset,
                    size: *size,
                    _padding: 0,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_RETRIEVE_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &retrieve_out)
                    .expect("vec size is not enough");

                Either::Left(data)
            }
        };

        self.sender.send(data).await.or(Err(kind))
    }

    /// try to notify kernel the IO is ready, kernel can wakeup the waiting program.
    pub async fn wakeup(mut self, kh: u64) {
        let _ = self.notify(NotifyKind::Wakeup { kh }).await;
    }

    /// try to notify the cache invalidation about an inode.
    pub async fn invalid_inode(mut self, inode: u64, offset: i64, len: i64) {
        let _ = self
            .notify(NotifyKind::InvalidInode { inode, offset, len })
            .await;
    }

    /// try to notify the invalidation about a directory entry.
    pub async fn invalid_entry(mut self, parent: u64, name: OsString) {
        let _ = self.notify(NotifyKind::InvalidEntry { parent, name }).await;
    }

    /// try to notify a directory entry has been deleted.
    pub async fn delete(mut self, parent: u64, child: u64, name: OsString) {
        let _ = self
            .notify(NotifyKind::Delete {
                parent,
                child,
                name,
            })
            .await;
    }

    /// try to push the data in an inode for updating the kernel cache.
    pub async fn store(mut self, inode: u64, offset: u64, mut data: impl Buf) {
        let _ = self
            .notify(NotifyKind::Store {
                inode,
                offset,
                data: data.copy_to_bytes(data.remaining()),
            })
            .await;
    }

    /// try to retrieve data in an inode from the kernel cache.
    pub async fn retrieve(mut self, notify_unique: u64, inode: u64, offset: u64, size: u32) {
        let _ = self
            .notify(NotifyKind::Retrieve {
                notify_unique,
                inode,
                offset,
                size,
            })
            .await;
    }
}

#[derive(Debug)]
/// the kind of notify.
enum NotifyKind {
    /// notify the IO is ready.
    Wakeup { kh: u64 },

    // TODO need check is right or not
    /// notify the cache invalidation about an inode.
    InvalidInode { inode: u64, offset: i64, len: i64 },

    /// notify the invalidation about a directory entry.
    InvalidEntry { parent: u64, name: OsString },

    /// notify a directory entry has been deleted.
    Delete {
        parent: u64,
        child: u64,
        name: OsString,
    },

    /// push the data in an inode for updating the kernel cache.
    Store {
        inode: u64,
        offset: u64,
        data: Bytes,
    },

    /// retrieve data in an inode from the kernel cache.
    Retrieve {
        notify_unique: u64,
        inode: u64,
        offset: u64,
        size: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_channel::mpsc::unbounded;
    use futures_util::StreamExt;

    /// Helper: create a Notify backed by an unbounded channel and return both
    /// the Notify handle and the receiver so we can inspect serialised messages.
    fn make_notify() -> (Notify, futures_channel::mpsc::UnboundedReceiver<FuseData>) {
        let (tx, rx) = unbounded();
        (Notify::new(tx), rx)
    }

    /// Parse the fuse_out_header fields (len, error, unique) from raw bytes.
    fn parse_header(data: &[u8]) -> (u32, i32, u64) {
        let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let error = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let unique = u64::from_le_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        (len, error, unique)
    }

    #[tokio::test]
    async fn invalid_entry_includes_name_and_null_terminator() {
        let (mut notify, mut rx) = make_notify();
        let name = OsString::from("seqwrite.0.0");
        let name_len = name.len(); // 12

        notify
            .notify(NotifyKind::InvalidEntry {
                parent: 1,
                name: name.clone(),
            })
            .await
            .unwrap();

        let msg = rx.next().await.unwrap();
        let (header_data, extend_data) = match msg {
            Either::Right((h, e)) => (h, e),
            _ => panic!("InvalidEntry should produce Either::Right"),
        };

        // Header len must equal header + struct + name + null
        let expected_total =
            FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE + name_len + 1;
        let (len, error, unique) = parse_header(&header_data);
        assert_eq!(len as usize, expected_total, "header len must cover full payload");
        assert_eq!(error, fuse_notify_code::FUSE_NOTIFY_INVAL_ENTRY as i32);
        assert_eq!(unique, 0, "notifications use unique=0");

        // Extend data must be name bytes followed by a null terminator
        assert_eq!(extend_data.len(), name_len + 1);
        assert_eq!(&extend_data[..name_len], name.as_bytes());
        assert_eq!(extend_data[name_len], 0u8, "must be null-terminated");

        // Total wire size matches header len
        let actual_total = header_data.len() + extend_data.len();
        assert_eq!(actual_total, expected_total);
    }

    #[tokio::test]
    async fn delete_includes_name_and_null_terminator() {
        let (mut notify, mut rx) = make_notify();
        let name = OsString::from("test_file.txt");
        let name_len = name.len();

        notify
            .notify(NotifyKind::Delete {
                parent: 1,
                child: 42,
                name: name.clone(),
            })
            .await
            .unwrap();

        let msg = rx.next().await.unwrap();
        let (header_data, extend_data) = match msg {
            Either::Right((h, e)) => (h, e),
            _ => panic!("Delete should produce Either::Right"),
        };

        let expected_total =
            FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_DELETE_OUT_SIZE + name_len + 1;
        let (len, error, _) = parse_header(&header_data);
        assert_eq!(len as usize, expected_total);
        assert_eq!(error, fuse_notify_code::FUSE_NOTIFY_DELETE as i32);

        assert_eq!(extend_data.len(), name_len + 1);
        assert_eq!(&extend_data[..name_len], name.as_bytes());
        assert_eq!(extend_data[name_len], 0u8);
    }

    #[tokio::test]
    async fn store_header_len_includes_data() {
        let (mut notify, mut rx) = make_notify();
        let payload = Bytes::from_static(b"hello world");

        notify
            .notify(NotifyKind::Store {
                inode: 5,
                offset: 0,
                data: payload.clone(),
            })
            .await
            .unwrap();

        let msg = rx.next().await.unwrap();
        let (header_data, extend_data) = match msg {
            Either::Right((h, e)) => (h, e),
            _ => panic!("Store should produce Either::Right"),
        };

        let expected_total =
            FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_STORE_OUT_SIZE + payload.len();
        let (len, error, _) = parse_header(&header_data);
        assert_eq!(len as usize, expected_total);
        assert_eq!(error, fuse_notify_code::FUSE_NOTIFY_STORE as i32);
        assert_eq!(extend_data.as_ref(), payload.as_ref());
    }

    #[tokio::test]
    async fn invalid_inode_header_len_is_correct() {
        let (mut notify, mut rx) = make_notify();

        notify
            .notify(NotifyKind::InvalidInode {
                inode: 10,
                offset: 0,
                len: 0,
            })
            .await
            .unwrap();

        let msg = rx.next().await.unwrap();
        let header_data = match msg {
            Either::Left(h) => h,
            _ => panic!("InvalidInode should produce Either::Left"),
        };

        let expected_total = FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_INODE_OUT_SIZE;
        let (len, error, unique) = parse_header(&header_data);
        assert_eq!(len as usize, expected_total);
        assert_eq!(error, fuse_notify_code::FUSE_NOTIFY_INVAL_INODE as i32);
        assert_eq!(unique, 0);
        assert_eq!(header_data.len(), expected_total);
    }
}
